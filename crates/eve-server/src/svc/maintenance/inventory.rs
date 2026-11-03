//! Maintenance authorization belongs to the inventory transaction, including
//! source bays and implicit StackAll flags, rather than only RPC preflight.
use super::*;
use std::collections::BTreeMap;

#[derive(Clone,Copy)]
pub(crate) struct Bay {pub host:i64,pub flag:i64,pub capacity:f64}

#[derive(Default)]
pub(crate) struct InventoryWrite {
    hosts:BTreeMap<i64,Host>,
    reservation:Option<Reservation>,
    actor_locked:bool,
    pub recipients:BTreeSet<i64>,
}

/// A pending final snapshot also protects its fittings and nested contents.
/// Call only with the source item locked; ancestor locks stabilize its custody.
pub(crate) async fn pending_mutation(ctx:&Ctx<'_>,tx:&mut ItemTxn<'_>,item:&Item)->Result<(),MoveError>{
    let mut current=item.clone();let mut seen=BTreeSet::new();
    for _ in 0..16 {
        if !seen.insert(current.id){return refuse("The item's containment is invalid.");}
        repair::ready(ctx.world,current.id)?;
        if crate::losses::journal::blocked_in_memory(ctx.world,current.id){return refuse("The ship's confirmed loss is awaiting settlement.");}
        if crate::travel::blocked_in_memory(ctx.world,current.id){return refuse("The ship's travel transaction is awaiting reconciliation.");}
        if crate::svc::jumps::portal::blocked_in_memory(ctx.world,current.id){return refuse("The ship's portal activation is awaiting reconciliation.");}
        if crate::losses::pending::blocked(tx.tx(),current.id).await?{return refuse("The ship is finishing its station arrival. Try again shortly.");}
        if Some(current.id)==ctx.session.ship_id || current.location_id==0
            || Some(current.location_id)==ctx.session.char_id || Some(current.location_id)==ctx.session.station_id
            || Some(current.location_id)==ctx.session.solar_system_id {return Ok(());}
        let Some(parent)=tx.item(current.location_id).await? else{return Ok(());};
        current=parent;
    }
    refuse("The item's containment is too deep to modify safely.")
}

/// An ownership handover also mutates descendants. Moving a carrier to a
/// corporation office must not invalidate a pending stored hull's generation.
/// Lock and revalidate membership just as transfer_tree does, so a child that
/// already left the subtree is not mistakenly included after a lock wait.
pub(crate) async fn pending_tree_mutation(ctx:&Ctx<'_>,tx:&mut ItemTxn<'_>,root:i64)->Result<(),MoveError>{
    let mut pending=vec![(root,None)];let mut seen=BTreeSet::new();
    while let Some((id,parent))=pending.pop(){
        if !seen.insert(id){return refuse("The item's containment is invalid.");}
        let Some(item)=tx.item(id).await? else{continue;};
        if parent.is_some_and(|p|item.location_id!=p){continue;}
        pending_mutation(ctx,tx,&item).await?;
        let children=tx.tx().query(r#"SELECT "itemID" FROM entity WHERE "locationID"=$1 ORDER BY "itemID""#,&[&(id as i32)]).await?;
        for row in children.into_iter().rev(){pending.push((i64::from(row.get::<_,i32>(0)),Some(id)));}
    }
    Ok(())
}

impl InventoryWrite {
    async fn actor(&mut self,ctx:&Ctx<'_>,tx:&ItemTxn<'_>)->Result<(),MoveError>{
        if self.actor_locked{return Ok(());}
        if let Some(station)=ctx.session.station_id {
            let me=ctx.session.char_id.unwrap_or(0);let actor=ctx.session.ship_id.unwrap_or(0);
            if !super::super::login::lock_character_lifecycle(tx.tx(),me as i32).await?{return refuse("That character is unavailable.");}
            if tx.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2 AND "stationID"=$3 FOR UPDATE"#,
                &[&(me as i32),&(actor as i32),&(station as i32)]).await?.is_none(){return refuse("Your ship or station changed.");}
        }else{actor_lock(ctx,tx).await?;}
        self.actor_locked=true;Ok(())
    }

    /// Returns None only for an ordinary inventory location. A POS array never
    /// accepts a second inventory under a caller-selected noncanonical flag.
    pub async fn destination(&mut self,ctx:&Ctx<'_>,tx:&mut ItemTxn<'_>,item:&Item,flag:Option<i64>,access:starbase::Access)->Result<Option<Bay>,MoveError>{
        let group:i32=tx.tx().query_one(r#"SELECT "groupID" FROM "invTypes" WHERE "typeID"=$1"#,&[&(item.type_id as i32)]).await?.get(0);
        let array=group==363;
        if !array && flag!=Some(FLAG){return Ok(None);}
        if array && flag.is_some_and(|f|f!=0){return refuse("A maintenance array uses its main ship inventory.");}
        if !array && (item.category_id!=6 || attribute(tx.tx(),item.type_id,907).await?<=0.0){return refuse("That ship has no maintenance bay.");}
        pending_mutation(ctx,tx,item).await?;
        self.actor(ctx,tx).await?;
        let mut h=if let Some(station)=ctx.session.station_id {
            if array || Some(item.owner_id)!=ctx.session.char_id || item.location_id!=station || item.flag!=4 || !item.singleton || item.quantity!=1 {
                return refuse("Use your own ship's maintenance bay in this station.");
            }
            let capacity=attribute(tx.tx(),item.type_id,908).await?;
            if !capacity.is_finite() || capacity<=0.0{return refuse("That bay has no usable capacity.");}
            Host{item:item.clone(),system:0,capacity,range:0.0,users:0,structure:None,permission:0,owner_corp:ctx.session.corp_id.unwrap_or(0)}
        }else{host(tx.tx(),ctx,item.clone(),access).await?};
        if let Some(old)=self.hosts.get(&item.id) {
            // Take permission is stronger than view/put for POS inventories.
            if old.structure.is_some() && access==starbase::Access::ViewPut {h.permission=old.permission;}
        }
        if ctx.session.station_id.is_none(){
            if let Some(reserve)=&mut self.reservation {
                let mut w=ctx.world.lock();check(ctx,&w,&h,&reserve.ids,true)?;
                if h.structure.is_none() && reserve.ids.insert(item.id) {
                    w.parks.get_mut(&h.system).unwrap().ships.get_mut(&item.id).unwrap().identity_reserved=true;
                }
            }else{self.reservation=Some(Reservation::new(ctx,&h,&[])?);}
        }
        self.recipients.extend(ctx.world.lock().clients_of(item.owner_id));
        let bay=Bay{host:item.id,flag:if array{0}else{FLAG},capacity:h.capacity};
        self.hosts.insert(item.id,h);Ok(Some(bay))
    }

    pub async fn source(&mut self,ctx:&Ctx<'_>,tx:&mut ItemTxn<'_>,item:&Item)->Result<Option<Bay>,MoveError>{
        pending_mutation(ctx,tx,item).await?;
        if Some(item.location_id)==ctx.session.station_id || Some(item.location_id)==ctx.session.solar_system_id
            || Some(item.location_id)==ctx.session.char_id || item.location_id==0 {return Ok(None);}
        let Some(parent)=tx.item(item.location_id).await? else{return Ok(None);};
        let bay=self.destination(ctx,tx,&parent,Some(item.flag),starbase::Access::Take).await?;
        if bay.is_some(){
            if item.owner_id!=ctx.session.char_id.unwrap_or(0)
                && !(self.hosts[&parent.id].structure.is_some() && Some(item.owner_id)==ctx.session.corp_id) {
                return refuse("You may only move your own ships or authorized corporation ships from this bay.");
            }
            self.recipients.extend(ctx.world.lock().clients_of(item.owner_id));
        }
        Ok(bay)
    }

    pub async fn accepts(&self,ctx:&Ctx<'_>,tx:&mut ItemTxn<'_>,bay:Bay,item:&Item,quantity:i64)->Result<(),MoveError>{
        if item.id==bay.host || Some(item.id)==ctx.session.ship_id{return refuse("An active ship must use Store Vessel.");}
        if tx.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "shipID"=$1"#,&[&(item.id as i32)]).await?.is_some(){return refuse("A piloted ship must use Store Vessel.");}
        let h=&self.hosts[&bay.host];
        if item.owner_id!=ctx.session.char_id.unwrap_or(0)
            && !(h.structure.is_some() && Some(item.owner_id)==ctx.session.corp_id){return refuse("Store your own ships or authorized corporation ships in this bay.");}
        if item.location_id==bay.host && item.flag==bay.flag {stored_allowed(tx,h,item,quantity).await}
        else{cargo_allowed(tx,h,item,quantity).await}
    }

    pub fn validate(&self,ctx:&Ctx<'_>)->Result<(),MoveError>{
        if let Some(reserve)=&self.reservation {
            let w=ctx.world.lock();
            for host in self.hosts.values(){check(ctx,&w,host,&reserve.ids,true)?;}
        }
        Ok(())
    }
}
