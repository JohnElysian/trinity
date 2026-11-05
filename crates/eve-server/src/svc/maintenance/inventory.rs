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
/// Call inside the transaction that changes the source item.
pub(crate) fn pending_mutation(ctx:&Ctx<'_>,tx:&mut ItemTxn<'_,'_>,item:&Item)->Result<(),MoveError>{
    let mut current=item.clone();let mut seen=BTreeSet::new();
    for _ in 0..16 {
        if !seen.insert(current.id){return refuse("The item's containment is invalid.");}
        repair::ready(ctx.world,current.id)?;
        if crate::losses::journal::blocked_in_memory(ctx.world,current.id){return refuse("The ship's confirmed loss is awaiting settlement.");}
        if crate::travel::blocked_in_memory(ctx.world,current.id){return refuse("The ship's travel transaction is awaiting reconciliation.");}
        if crate::svc::jumps::portal::blocked_in_memory(ctx.world,current.id){return refuse("The ship's portal activation is awaiting reconciliation.");}
        if crate::losses::pending::blocked(tx.db(),current.id)?{return refuse("The ship is finishing its station arrival. Try again shortly.");}
        if Some(current.id)==ctx.session.ship_id || current.location_id==0
            || Some(current.location_id)==ctx.session.char_id || Some(current.location_id)==ctx.session.station_id
            || Some(current.location_id)==ctx.session.solar_system_id {return Ok(());}
        let Some(parent)=tx.item(current.location_id)? else{return Ok(());};
        current=parent;
    }
    refuse("The item's containment is too deep to modify safely.")
}

/// An ownership handover also mutates descendants. Moving a carrier to a
/// corporation office must not invalidate a pending stored hull's generation.
/// Revalidate membership just as transfer_tree does, so a child that already
/// left the subtree is not mistakenly included.
pub(crate) fn pending_tree_mutation(ctx:&Ctx<'_>,tx:&mut ItemTxn<'_,'_>,root:i64)->Result<(),MoveError>{
    let mut pending=vec![(root,None)];let mut seen=BTreeSet::new();
    while let Some((id,parent))=pending.pop(){
        if !seen.insert(id){return refuse("The item's containment is invalid.");}
        let Some(item)=tx.item(id)? else{continue;};
        if parent.is_some_and(|p|item.location_id!=p){continue;}
        pending_mutation(ctx,tx,&item)?;
        for child in children(tx.db(),id).into_iter().rev(){pending.push((child,Some(id)));}
    }
    Ok(())
}

impl InventoryWrite {
    fn actor(&mut self,ctx:&Ctx<'_>,tx:&ItemTxn<'_,'_>)->Result<(),MoveError>{
        if self.actor_locked{return Ok(());}
        let db=tx.db();
        if let Some(station)=ctx.session.station_id {
            let me=ctx.session.char_id.unwrap_or(0);let actor=ctx.session.ship_id.unwrap_or(0);
            if !super::super::login::lock_character_lifecycle(db,me)?{return refuse("That character is unavailable.");}
            if !db.table("character_").get_id(me).is_some_and(|c|c.int("shipID")==Some(actor) && c.int("stationID")==Some(station)){return refuse("Your ship or station changed.");}
        }else{actor_lock(ctx,db)?;}
        self.actor_locked=true;Ok(())
    }

    /// Returns None only for an ordinary inventory location. A POS array never
    /// accepts a second inventory under a caller-selected noncanonical flag.
    pub fn destination(&mut self,ctx:&Ctx<'_>,tx:&mut ItemTxn<'_,'_>,item:&Item,flag:Option<i64>,access:starbase::Access)->Result<Option<Bay>,MoveError>{
        let group=type_group(item.type_id)?;
        let array=group==363;
        if !array && flag!=Some(FLAG){return Ok(None);}
        if array && flag.is_some_and(|f|f!=0){return refuse("A maintenance array uses its main ship inventory.");}
        if !array && (item.category_id!=6 || attribute(item.type_id,907)?<=0.0){return refuse("That ship has no maintenance bay.");}
        pending_mutation(ctx,tx,item)?;
        self.actor(ctx,tx)?;
        let mut h=if let Some(station)=ctx.session.station_id {
            if array || Some(item.owner_id)!=ctx.session.char_id || item.location_id!=station || item.flag!=4 || !item.singleton || item.quantity!=1 {
                return refuse("Use your own ship's maintenance bay in this station.");
            }
            let capacity=attribute(item.type_id,908)?;
            if !capacity.is_finite() || capacity<=0.0{return refuse("That bay has no usable capacity.");}
            Host{item:item.clone(),system:0,capacity,range:0.0,users:0,structure:None,permission:0,owner_corp:ctx.session.corp_id.unwrap_or(0)}
        }else{host(tx.db(),ctx,item.clone(),access)?};
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

    pub fn source(&mut self,ctx:&Ctx<'_>,tx:&mut ItemTxn<'_,'_>,item:&Item)->Result<Option<Bay>,MoveError>{
        pending_mutation(ctx,tx,item)?;
        if Some(item.location_id)==ctx.session.station_id || Some(item.location_id)==ctx.session.solar_system_id
            || Some(item.location_id)==ctx.session.char_id || item.location_id==0 {return Ok(None);}
        let Some(parent)=tx.item(item.location_id)? else{return Ok(None);};
        let bay=self.destination(ctx,tx,&parent,Some(item.flag),starbase::Access::Take)?;
        if bay.is_some(){
            if item.owner_id!=ctx.session.char_id.unwrap_or(0)
                && !(self.hosts[&parent.id].structure.is_some() && Some(item.owner_id)==ctx.session.corp_id) {
                return refuse("You may only move your own ships or authorized corporation ships from this bay.");
            }
            self.recipients.extend(ctx.world.lock().clients_of(item.owner_id));
        }
        Ok(bay)
    }

    pub fn accepts(&self,ctx:&Ctx<'_>,tx:&mut ItemTxn<'_,'_>,bay:Bay,item:&Item,quantity:i64)->Result<(),MoveError>{
        if item.id==bay.host || Some(item.id)==ctx.session.ship_id{return refuse("An active ship must use Store Vessel.");}
        if piloted(tx.db(),item.id){return refuse("A piloted ship must use Store Vessel.");}
        let h=&self.hosts[&bay.host];
        if item.owner_id!=ctx.session.char_id.unwrap_or(0)
            && !(h.structure.is_some() && Some(item.owner_id)==ctx.session.corp_id){return refuse("Store your own ships or authorized corporation ships in this bay.");}
        if item.location_id==bay.host && item.flag==bay.flag {stored_allowed(tx,h,item,quantity)}
        else{cargo_allowed(tx,h,item,quantity)}
    }

    pub fn validate(&self,ctx:&Ctx<'_>)->Result<(),MoveError>{
        if let Some(reserve)=&self.reservation {
            let w=ctx.world.lock();
            for host in self.hosts.values(){check(ctx,&w,host,&reserve.ids,true)?;}
        }
        Ok(())
    }
}
