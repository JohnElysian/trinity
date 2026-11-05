//! Durable launched-drone state. Inventory remains authoritative: an item
//! transition invalidates its launch generation, and snapshots never create one.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy)]
pub(super) struct Identity { pub(super) generation: i64, revision: i64 }

#[derive(Clone, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    motion: Option<crate::ballpark::Motion>,
    position: [f64; 3],
    velocity: [f64; 3],
    controller: Option<i64>,
    controller_owner: i64,
    assignment: Option<Assignment>,
    order: Order,
    activity: i64,
    mined: bool,
    settings: Settings,
    control_range: f64,
}

impl State {
    fn initial(owner: i64, controller: Option<i64>, pos: Vec3, settings: Settings, range: f64) -> Self {
        Self { motion:None, position: [pos.x,pos.y,pos.z], velocity:[0.0;3], controller, controller_owner:owner,
            assignment:None, order:Order::Idle, activity:activity::IDLE, mined:false, settings, control_range:range }
    }
    fn capture(ship: &Ship,tick:i64) -> Self {
        let d=ship.drone.as_ref().unwrap();
        Self { motion:d.restored_motion.clone().or_else(||Some(ship.motion(tick))), position:[ship.pos.x,ship.pos.y,ship.pos.z], velocity:[ship.vel.x,ship.vel.y,ship.vel.z],
            controller:d.controller, controller_owner:d.controller_owner, assignment:d.assignment,
            order:d.order.clone(), activity:d.activity, mined:d.mined, settings:d.settings, control_range:d.control_range }
    }
    fn valid(&self) -> bool {
        Vec3::new(self.position[0],self.position[1],self.position[2]).is_finite()
            && Vec3::new(self.velocity[0],self.velocity[1],self.velocity[2]).is_finite()
            && self.control_range.is_finite() && self.control_range>=0.0
    }
}

/// The launch records, their generations and the trigger that drops a
/// record when its item moves are defined in the store's schema.
pub fn ensure(_tx: &mut Tx<'_>) -> Result<(),StoreError> {Ok(())}

pub(super) fn begin(tx:&mut Tx<'_>,id:i64,owner:i64,system:i64,controller:Option<i64>,pos:Vec3,
    settings:Settings,range:f64) -> Result<Identity,StoreError> {
    let state=serde_json::to_string(&State::initial(owner,controller,pos,settings,range)).unwrap();
    let record=[("owner_id",store::Value::Int(owner)),("system_id",store::Value::Int(system)),("state",store::Value::Text(state.into()))];
    // A relaunch of a recorded drone starts a new generation.
    let row=match tx.table("launched_drone_state").get_id(id).map(|r|r.id) {
        Some(row)=>{
            let generation=tx.nextval("launched_drone_generation")?;
            let mut sets=vec![("generation",store::Value::Int(generation)),("revision",store::Value::Int(0))];
            sets.extend(record);
            tx.update("launched_drone_state",row,&sets)?;
            row
        }
        None=>{
            let mut sets=vec![("drone_id",store::Value::Int(id))];
            sets.extend(record);
            tx.insert("launched_drone_state",&sets)?
        }
    };
    let generation=tx.table("launched_drone_state").row(row).and_then(|r|r.int("generation")).unwrap_or(0);
    Ok(Identity{generation,revision:0})
}

#[derive(Clone)]
struct Snapshot { system:i64, tick:i64, ship:Ship, identity:Identity }

fn capture(world:&World, ids:Option<&[i64]>) -> Vec<Snapshot> {
    let mut w=world.lock(); let mut snapshots=Vec::new();
    for (system,park) in &mut w.parks {
        for ship in park.ships.values_mut() {
            if ids.is_some_and(|ids|!ids.contains(&ship.id)) || !ship.pos.is_finite() || !ship.vel.is_finite() {continue}
            let Some(d)=ship.drone.as_mut().filter(|d|!d.pending_bay) else {continue};
            let Some(identity)=d.persistence.as_mut() else {continue};
            identity.revision=identity.revision.saturating_add(1);
            let identity=*identity;
            snapshots.push(Snapshot{system:*system,tick:park.tick,ship:ship.clone(),identity});
        }
    }
    snapshots
}

async fn save(store:&Store,world:&World,snapshot:Snapshot) -> Result<bool,StoreError> {
    let id=snapshot.ship.id;
    let owner=snapshot.ship.drone.as_ref().unwrap().owner;
    store.write(|tx| {
        // The inventory item must still be this owner's drone in space here.
        let custody=tx.table("entity").get_id(id).is_some_and(|e|e.int("ownerID")==Some(owner)
            && e.int("locationID")==Some(snapshot.system) && e.int("flag")==Some(0) && e.int("quantity")==Some(1) && e.int("singleton")==Some(1)
            && e.int("typeID").is_some_and(|t|crate::sde::get().types_in_category(18).contains(&(t as i32))));
        if !custody {return Ok(false)}
        let live=world.lock().parks.get(&snapshot.system).and_then(|p|p.ships.get(&id))
            .and_then(|s|s.drone.as_ref()).is_some_and(|d|!d.pending_bay && d.owner==owner
                && d.persistence.is_some_and(|p|p.generation==snapshot.identity.generation));
        if !live {return Ok(false)}
        let state=serde_json::to_string(&State::capture(&snapshot.ship,snapshot.tick)).unwrap();
        let Some(record)=tx.table("launched_drone_state").get_id(id).filter(|r|r.int("generation")==Some(snapshot.identity.generation)
            && r.int("revision").is_some_and(|r|r<snapshot.identity.revision) && r.int("owner_id")==Some(owner)
            && r.int("system_id")==Some(snapshot.system)).map(|r|r.id) else {return Ok(false)};
        tx.update("launched_drone_state",record,&[("revision",store::Value::Int(snapshot.identity.revision)),("state",store::Value::Text(state.into()))])?;
        if let Some(item)=tx.table("entity").get_id(id).map(|r|r.id) {
            tx.update("entity",item,&[("x",store::Value::Float(snapshot.ship.pos.x)),("y",store::Value::Float(snapshot.ship.pos.y)),
                ("z",store::Value::Float(snapshot.ship.pos.z))])?;
        }
        if let Some(f)=&snapshot.ship.fight {crate::svc::inflight::write_vitals(tx,id,f,false)?;}
        Ok(true)
    })
}

pub async fn checkpoint(world:&World,store:&Store) {
    for snapshot in capture(world,None) {
        if let Err(e)=save(store,world,snapshot).await {warn!("saving launched drone: {}",crate::logging::chain(&e));}
    }
}

pub(super) async fn ordered(ctx:&Ctx<'_>,ids:&[i64]) {
    for snapshot in capture(ctx.world,Some(ids)) {
        if let Err(e)=save(ctx.store,ctx.world,snapshot).await {warn!("saving drone order: {e}");}
    }
}

pub(super) fn vitals(db:&Db,ids:&[i64]) -> HashMap<i64,Vec<(i32,f64)>> {
    let attributes=db.table("entity_attributes");
    let mut out=HashMap::<i64,Vec<(i32,f64)>>::new();
    for id in ids {
        if out.contains_key(id) {continue}
        for attribute in [attr::SHIELD_CHARGE,attr::CHARGE,attr::ARMOR_DAMAGE,attr::DAMAGE] {
            let Some(row)=attributes.get(&[store::Value::Int(*id),store::Value::Int(attribute.into())]) else {continue};
            if let Some(v)=row.float("valueFloat").or_else(||row.float("valueInt")).filter(|v|v.is_finite()) {out.entry(*id).or_default().push((attribute,v));}
        }
    }
    out
}

pub(super) fn apply_vitals(ship:&mut Ship,values:Option<&Vec<(i32,f64)>>,now:i64) {
    let Some(f)=ship.fight.as_mut() else {return};
    for (a,v) in values.into_iter().flatten() {
        match *a {
            attr::SHIELD_CHARGE=>f.vitals.shield.set(now,*v), attr::CHARGE=>f.vitals.capacitor.set(now,*v),
            attr::ARMOR_DAMAGE=>f.vitals.armor_damage=v.max(0.0), attr::DAMAGE=>f.vitals.hull_damage=v.max(0.0), _=>{}
        }
    }
}

/// Existing drones from databases predating this table keep their inventory
/// position and damage. Their unrecorded orders cannot be inferred.
pub async fn load(ctx:&Ctx<'_>,system:i64) -> Result<Vec<Ship>,StoreError> {
    let rows=ctx.store.read(|db| {
        let drones=crate::sde::get().types_in_category(18);
        let (characters,corporations)=(db.table("character_"),db.table("corporation"));
        let mut rows:Vec<(i64,i64,i64,i64,Option<i64>)>=db.table("entity").find(&["locationID","flag"],&[store::Value::Int(system),store::Value::Int(0)])
            .filter(|e|e.int("quantity")==Some(1) && e.int("singleton")==Some(1) && e.int("typeID").is_some_and(|t|drones.contains(&(t as i32))))
            .filter_map(|e|{
                let owner=e.int("ownerID")?;
                let corp=characters.get_id(owner).and_then(|c|c.int("corporationID"));
                let alliance=corp.and_then(|c|corporations.get_id(c)).and_then(|co|co.int("allianceID"));
                Some((e.int("itemID")?,e.int("typeID")?,owner,corp.unwrap_or(0),alliance))
            }).collect();
        rows.sort_by_key(|r|r.0);
        rows
    });
    let type_ids=rows.iter().map(|r|r.1).collect::<Vec<_>>();
    let types=ctx.store.read(|db|npc::load_types(db,&type_ids))?;
    let mut out=Vec::new(); let now=combat::now_ms();let mut rng=combat::Rng::seeded(now as u64);
    for (id,type_id,owner,corp_id,alliance_id) in rows {
        let Some(ty)=types.get(&type_id) else {continue};
        let (settings,loadout)=ctx.store.read(|db|{
            let settings=Settings::load(db,owner);
            let owner_ship=db.table("character_").get_id(owner).and_then(|c|c.int("shipID"))
                .filter(|ship|db.table("entity").get_id(*ship).is_some_and(|e|e.int("ownerID")==Some(owner)));
            crate::dogma::Loadout::load_with_drones(db,ctx.dogma,owner,owner_ship,&Default::default(),&[id]).map(|loadout|(settings,loadout))
        })?;
        let attributes=loadout.find(id).map(|i|loadout.attributes(i));let ty=ty.with_attributes(attributes.as_ref());
        let range=loadout.character().map_or(20_000.0,|c|loadout.value(c,458).max(0.0));
        let ship=ctx.store.write(|tx|->Result<Option<Ship>,StoreError> {
            let Some(item)=tx.table("entity").get_id(id).filter(|e|e.int("ownerID")==Some(owner) && e.int("locationID")==Some(system)
                && e.int("typeID")==Some(type_id) && e.int("flag")==Some(0) && e.int("quantity")==Some(1) && e.int("singleton")==Some(1)) else {return Ok(None)};
            let pos=Vec3::new(item.float("x").unwrap_or(0.0),item.float("y").unwrap_or(0.0),item.float("z").unwrap_or(0.0));
            if !pos.is_finite() {return Ok(None)}
            let record=tx.table("launched_drone_state").get_id(id).filter(|r|r.int("owner_id")==Some(owner) && r.int("system_id")==Some(system));
            let recorded=match record {
                Some(row)=>{
                    match row.text("state").and_then(|state|serde_json::from_str::<State>(state).ok()).filter(State::valid) {
                        Some(state)=>Some((Identity{generation:row.int("generation").unwrap_or(0),revision:row.int("revision").unwrap_or(0)},state)),
                        None=>{
                            warn!(drone=id,"invalid durable drone state; preserving inventory position and vitals");
                            None
                        },
                    }
                },
                None=>None,
            };
            let state=recorded.as_ref().map_or_else(||State::initial(owner,None,pos,settings,range),|(_,state)|state.clone());
            let stored=vitals(tx.db(),&[id]);
            let pilot=Pilot{type_id,group_id:ty.group_id,name:ty.name.clone(),char_id:owner,corp_id,alliance_id,security:0.0,bounty:0.0};
            let mut ship=make(&ty,id,pilot,0,state.controller.unwrap_or(0),pos,now,&mut rng);
            ship.vel=Vec3::new(state.velocity[0],state.velocity[1],state.velocity[2]);ship.command=Command::Stop;
            apply_vitals(&mut ship,stored.get(&id),now);
            if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_damage>=f.vitals.hull_max) {return Ok(None)}
            // A drone without a valid record starts a new generation.
            let identity=match recorded {
                Some((identity,_))=>identity,
                None=>begin(tx,id,owner,system,None,pos,settings,range)?,
            };
            let d=ship.drone.as_mut().unwrap();d.persistence=Some(identity);d.controller=state.controller;d.controller_owner=state.controller_owner;
            d.controller_client=0;d.assignment=state.assignment;d.order=state.order;d.activity=state.activity;d.mined=state.mined;
            d.restored_motion=state.motion;
            d.settings=settings;d.control_range=range;d.next_think_ms=0;
            Ok(Some(ship))
        })?;
        out.extend(ship);
    }
    Ok(out)
}

/// Revalidate the inventory immediately before making loaded drones live. A
/// scoop, loss, ownership transfer or relaunch during profile loading wins.
pub async fn install(ctx:&Ctx<'_>,system:i64,ships:Vec<Ship>) -> Result<(),StoreError> {
    if ships.is_empty() {return Ok(())}
    // The drones go live while the store is held, so an inventory change
    // either came first and is seen here, or comes after and sees them live.
    ctx.store.read(|db| {
        let mut ids:Vec<i64>=ships.iter().map(|s|s.id).collect();
        ids.sort_unstable();ids.dedup();
        let (entity,records)=(db.table("entity"),db.table("launched_drone_state"));
        let mut ships:HashMap<i64,Ship>=ships.into_iter().map(|s|(s.id,s)).collect();
        let stored=vitals(db,&ids);
        let now=combat::now_ms();let mut valid=Vec::new();
        for row in ids.iter().filter_map(|id|entity.get_id(*id)).filter(|e|e.int("locationID")==Some(system) && e.int("flag")==Some(0)
            && e.int("quantity")==Some(1) && e.int("singleton")==Some(1)) {
            let Some(id)=row.int("itemID") else {continue};
            let Some(mut ship)=ships.remove(&id) else {continue};
            let d=ship.drone.as_mut().unwrap();
            let Some(record)=records.get_id(id).filter(|r|d.persistence.is_some_and(|p|Some(p.generation)==r.int("generation"))) else {continue};
            if Some(d.owner)!=row.int("ownerID") || Some(ship.pilot.type_id)!=row.int("typeID") {continue}
            let Some(Ok(state))=record.text("state").map(serde_json::from_str::<State>) else {continue};
            if !state.valid() {continue}
            ship.pos=Vec3::new(row.float("x").unwrap_or(0.0),row.float("y").unwrap_or(0.0),row.float("z").unwrap_or(0.0));
            if !ship.pos.is_finite() {continue}
            ship.vel=Vec3::new(state.velocity[0],state.velocity[1],state.velocity[2]);
            d.persistence=Some(Identity{generation:record.int("generation").unwrap_or(0),revision:record.int("revision").unwrap_or(0)});
            d.client_id=0;d.controller_client=0;ship.client_id=0;
            d.controller=state.controller;d.controller_owner=state.controller_owner;d.assignment=state.assignment;
            d.order=state.order;d.activity=state.activity;d.mined=state.mined;
            d.restored_motion=(state.position==[ship.pos.x,ship.pos.y,ship.pos.z]).then_some(state.motion).flatten();
            apply_vitals(&mut ship,stored.get(&id),now);
            if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_damage>=f.vitals.hull_max) {continue}
            valid.push(ship);
        }
        let mut w=ctx.world.lock();let mut out=Out::default();let mut inserted=Vec::new();
        if let Some(park)=w.parks.get_mut(&system) {
            for ship in valid {
                let id=ship.id;
                if let std::collections::btree_map::Entry::Vacant(entry)=park.ships.entry(id) {
                    entry.insert(ship);
                    inserted.push(id);
                }
            }
        }
        reconcile_into(&mut w,&mut out);
        if let Some(park)=w.parks.get_mut(&system) {
            for id in inserted {
                // These balls have not been published yet; their first payload
                // carries the restored phase and needs no separate reset.
                if let Some(d)=park.ships.get_mut(&id).and_then(|s|s.drone.as_mut()) {d.motion_resync=false;}
                for e in space::add_ball_entries(park,id) {out.destiny(system,e);}
            }
        }
        out.deliver(&w);
    });
    Ok(())
}

/// Sessions are ephemeral. Bind to current piloted hulls, reject capsules,
/// empty hulls and revoked fleet delegation, and never reuse a saved client ID.
pub fn reconcile(world:&mut crate::world::WorldState) {
    let mut out=Out::default();
    reconcile_into(world,&mut out);
    out.deliver(world);
}

fn reconcile_into(world:&mut crate::world::WorldState,out:&mut Out) {
    for park in world.parks.values_mut() {
        let pilots:HashMap<i64,(i64,i64)>=park.ships.values().filter(|s|s.is_pilot() && s.pilot.group_id!=29)
            .map(|s|(s.pilot.char_id,(s.id,s.client_id))).collect();
        for ship in park.ships.values_mut() {
            let Some(d)=ship.drone.as_mut().filter(|d|!d.pending_bay) else {continue};
            let old=(d.client_id,d.controller,d.controller_owner,d.controller_client);
            d.client_id=pilots.get(&d.owner).map_or(0,|p|p.1);ship.client_id=d.client_id;
            let delegated=d.controller_owner!=d.owner && ship.pilot.group_id==GROUP_FIGHTER
                && world.gangs.of(d.owner).is_some() && world.gangs.of(d.owner)==world.gangs.of(d.controller_owner);
            if d.controller_owner!=d.owner && !delegated {
                d.restored_motion=None;d.order=Order::Idle;d.activity=activity::IDLE;
            }
            let selected=if delegated {pilots.get(&d.controller_owner).copied()} else {None}.or_else(||pilots.get(&d.owner).copied());
            let owner=selected.and_then(|(id,_)|pilots.iter().find(|(_,p)|p.0==id).map(|(owner,_)|*owner));
            d.controller=selected.map(|p|p.0);d.controller_client=selected.map_or(0,|p|p.1);d.controller_owner=owner.unwrap_or(d.owner);
            if d.owner==OWNER_SYSTEM {d.controller=None;d.controller_client=0;d.client_id=0;ship.client_id=0;}
            if d.assignment.is_some_and(|a|!pilots.get(&a.pilot()).is_some_and(|p|p.0==a.ship())
                || world.gangs.of(d.owner).is_none() || world.gangs.of(d.owner)!=world.gangs.of(a.pilot())) {clear_assignment(ship.id,d,out);}
            if old!=(d.client_id,d.controller,d.controller_owner,d.controller_client) {
                d.next_think_ms=0;state_note(out,ship.id,d,ship.pilot.type_id,Some(old.3));
            }
            let recovered=d.controller.and_then(|_|d.restored_motion.take());
            if let Some(motion)=recovered {
                if ship.restore_motion(&motion,park.tick) {ship.drone.as_mut().unwrap().motion_resync=true;}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SYSTEM:i64=30_000_001;

    async fn fixture() -> (crate::test_support::Database,i64,i64,i64,i64,i64) {
        let f=crate::test_support::Database::load().await;
        let owner=f.id().await as i64;let home=f.id().await as i64;let id=f.id().await as i64;
        let other=f.id().await as i64;let other_ship=f.id().await as i64;
        f.store.write(|tx|->Result<(),StoreError> {
            for (id,ty,who,location) in [(owner,1377,owner,home),(home,626,owner,SYSTEM),(id,2454,owner,SYSTEM),
                (other,1377,other,other_ship),(other_ship,626,other,SYSTEM)] {
                tx.insert("entity",&[("itemID",store::Value::Int(id)),("typeID",store::Value::Int(ty)),("ownerID",store::Value::Int(who)),
                    ("locationID",store::Value::Int(location)),("flag",store::Value::Int(0)),("quantity",store::Value::Int(1)),("singleton",store::Value::Int(1)),
                    ("x",store::Value::Float(123.0)),("y",store::Value::Float(234.0)),("z",store::Value::Float(345.0))])?;
            }
            for (item,attribute,value) in [(owner,attr::MAX_ACTIVE_DRONES,5.0),(other,attr::MAX_ACTIVE_DRONES,5.0),
                (home,283,100.0),(other_ship,283,100.0),(id,attr::CAPACITOR_CAPACITY,100.0),(id,attr::RECHARGE_RATE,0.0),
                (id,attr::SHIELD_CAPACITY,200.0),(id,attr::SHIELD_RECHARGE_RATE,0.0)] {
                tx.insert("entity_attributes",&[("itemID",store::Value::Int(item)),("attributeID",store::Value::Int(attribute.into())),
                    ("valueFloat",store::Value::Float(value))])?;
            }
            Ok(())
        }).unwrap();
        let mut park=Park::new(SYSTEM,vec![],0);
        for (ship,who,client) in [(home,owner,77),(other_ship,other,88)] {
            park.ships.insert(ship,Ship::new(ship,client,ShipStats::new(100.0,1e6,1.0,100.0,1.0),
                Pilot{char_id:who,type_id:626,group_id:26,name:"Controller".into(),corp_id:0,alliance_id:None,security:0.0,bounty:0.0},
                Vec3::new(123.0,234.0,345.0),Vec3::ZERO));
        }
        f.world.lock().parks.insert(SYSTEM,park);
        f.world.lock().gangs.test_members(vec![owner,other]);
        (f,owner,home,id,other,other_ship)
    }

    fn records(f:&crate::test_support::Database,id:i64)->usize {
        f.store.read(|db|db.table("launched_drone_state").find(&["drone_id"],&[store::Value::Int(id)]).count())
    }

    fn x(f:&crate::test_support::Database,id:i64)->Option<f64> {
        f.store.read(|db|db.table("entity").get_id(id).and_then(|e|e.float("x")))
    }

    /// Move the drone away and back to the same owner and system, an ABA
    /// transition, and launch it again.
    fn relaunch(f:&crate::test_support::Database,id:i64,owner:i64,home:i64,at:Vec3)->Identity {
        f.store.write(|tx|{
            let row=tx.table("entity").get_id(id).unwrap().id;
            tx.update("entity",row,&[("locationID",store::Value::Int(home)),("flag",store::Value::Int(87))])?;
            tx.update("entity",row,&[("locationID",store::Value::Int(SYSTEM)),("flag",store::Value::Int(0)),("x",store::Value::Float(at.x))])?;
            begin(tx,id,owner,SYSTEM,Some(home),at,Settings::default(),20000.0)
        }).unwrap()
    }

    fn session(owner:i64,home:i64)->crate::session::Session {
        crate::session::Session{char_id:Some(owner),ship_id:Some(home),solar_system_id:Some(SYSTEM),..Default::default()}
    }

    #[test]
    fn restored_delegation_uses_current_fleet_pilots_and_connection_ids() {
        let mut w=crate::world::WorldState::default();let mut park=Park::new(SYSTEM,vec![],0);
        let pilot=|owner,group|Pilot{char_id:owner,type_id:2454,group_id:group,name:"Pilot".into(),corp_id:0,alliance_id:None,security:0.0,bounty:0.0};
        for (id,owner,client) in [(10,100,1),(20,200,2)] {
            park.ships.insert(id,Ship::new(id,client,ShipStats::new(100.0,1e6,1.0,100.0,1.0),pilot(owner,26),Vec3::ZERO,Vec3::ZERO));
        }
        let ty=npc::NpcType::with(2454,GROUP_FIGHTER,5.0,3000.0,&[(attr::HP,100.0)]);
        let mut drone=make(&ty,30,pilot(100,GROUP_FIGHTER),999,20,Vec3::ZERO,0,&mut combat::Rng::seeded(1));
        drone.drone.as_mut().unwrap().controller_owner=200;
        park.ships.insert(30,drone);w.parks.insert(SYSTEM,park);w.gangs.test_members(vec![100,200]);
        reconcile(&mut w);
        let d=w.parks[&SYSTEM].ships[&30].drone.as_ref().unwrap();
        assert_eq!((d.client_id,d.controller,d.controller_owner,d.controller_client),(1,Some(20),200,2));
        w.parks.get_mut(&SYSTEM).unwrap().ships.get_mut(&20).unwrap().unpiloted=true;
        reconcile(&mut w);
        assert_eq!(w.parks[&SYSTEM].ships[&30].drone.as_ref().unwrap().controller,Some(10));
        let p=w.parks.get_mut(&SYSTEM).unwrap();p.ships.get_mut(&20).unwrap().unpiloted=false;
        let d=p.ships.get_mut(&30).unwrap().drone.as_mut().unwrap();d.controller=Some(20);d.controller_owner=200;
        d.assignment=Some(Assignment::Guard{pilot:200,ship:20});w.gangs=Default::default();
        reconcile(&mut w);
        let d=w.parks[&SYSTEM].ships[&30].drone.as_ref().unwrap();assert_eq!(d.controller,Some(10));assert!(d.assignment.is_none());
        w.parks.get_mut(&SYSTEM).unwrap().ships.get_mut(&10).unwrap().pilot.group_id=29;
        reconcile(&mut w);assert_eq!(w.parks[&SYSTEM].ships[&30].drone.as_ref().unwrap().controller,None);
    }

    #[test]
    fn recovered_fighter_warp_waits_for_permission_and_keeps_the_native_phase() {
        let now=crate::destiny::tick_now();let mut park=Park::new(SYSTEM,vec![],now-10);
        let pilot=Pilot{char_id:100,type_id:2454,group_id:GROUP_FIGHTER,name:"Fighter".into(),corp_id:0,alliance_id:None,security:0.0,bounty:0.0};
        let ty=npc::NpcType::with(2454,GROUP_FIGHTER,5.0,3000.0,&[(attr::HP,100.0),(attr::MAX_VELOCITY,500.0)]);
        let mut ship=make(&ty,30,pilot.clone(),0,10,Vec3::ZERO,combat::now_ms(),&mut combat::Rng::seeded(1));
        ship.drone.as_mut().unwrap().order=Order::Engage(20);park.ships.insert(30,ship);
        park.command(30,Command::Warp{target:Vec3::new(5e12,0.0,0.0),range:1000.0});park.advance(now);
        let mut saved=park.ships.remove(&30).unwrap();assert!(saved.in_warp());
        let motion=saved.motion(now);let phase=saved.warp_snapshot();saved.arrive(saved.pos);
        saved.drone.as_mut().unwrap().restored_motion=Some(motion.clone());
        park.ships.insert(30,saved.clone());let mut w=crate::world::WorldState::default();w.parks.insert(SYSTEM,park);
        reconcile(&mut w);
        assert!(!w.parks[&SYSTEM].ships[&30].in_warp());
        assert!(w.parks[&SYSTEM].ships[&30].drone.as_ref().unwrap().restored_motion.is_some());
        let home=Ship::new(10,77,ShipStats::new(100.0,1e6,1.0,100.0,1.0),Pilot{group_id:26,..pilot},Vec3::ZERO,Vec3::ZERO);
        w.parks.get_mut(&SYSTEM).unwrap().ships.insert(10,home);reconcile(&mut w);
        let recovered=&w.parks[&SYSTEM].ships[&30];assert!(recovered.in_warp());assert_eq!(recovered.warp_snapshot(),phase);
        assert!(recovered.drone.as_ref().unwrap().motion_resync);
        w.parks.get_mut(&SYSTEM).unwrap().ships.get_mut(&30).unwrap().drone.as_mut().unwrap().next_think_ms=i64::MAX;
        let mut out=Out::default();think(w.parks.get_mut(&SYSTEM).unwrap(),0,&mut out);
        assert_eq!(out.jobs.iter().filter(|j|matches!(j,Job::WarpResync{system:SYSTEM})).count(),1);
        let mut again=Out::default();think(w.parks.get_mut(&SYSTEM).unwrap(),0,&mut again);assert!(again.jobs.is_empty());
        // Expired delegation cannot resurrect an old attack/warp course even
        // when its property owner has a valid ship ready as fallback.
        let d=saved.drone.as_mut().unwrap();d.controller=Some(20);d.controller_owner=200;d.restored_motion=Some(motion);
        w.parks.get_mut(&SYSTEM).unwrap().ships.insert(30,saved);reconcile(&mut w);
        let rejected=&w.parks[&SYSTEM].ships[&30];assert!(!rejected.in_warp());
        assert_eq!(rejected.drone.as_ref().unwrap().order,Order::Idle);
    }

    #[test]
    fn recovering_into_a_lagging_park_does_not_advance_motion_twice() {
        let now=crate::destiny::tick_now();let base=now-70;
        let mut park=Park::new(SYSTEM,vec![],base);
        let pilot=Pilot{char_id:100,type_id:2454,group_id:GROUP_FIGHTER,name:"Fighter".into(),corp_id:0,alliance_id:None,security:0.0,bounty:0.0};
        let ty=npc::NpcType::with(2454,GROUP_FIGHTER,5.0,3000.0,&[(attr::HP,100.0),(attr::MAX_VELOCITY,500.0)]);
        let ship=make(&ty,30,pilot.clone(),1,10,Vec3::ZERO,0,&mut combat::Rng::seeded(1));park.ships.insert(30,ship);
        park.command(30,Command::Warp{target:Vec3::new(5e12,0.0,0.0),range:1000.0});park.advance(base+10);
        let mut recovered=park.ships[&30].clone();let motion=recovered.motion(park.tick);assert!(recovered.in_warp());
        park.advance(now);let expected=&park.ships[&30];
        recovered.arrive(recovered.pos);recovered.drone.as_mut().unwrap().restored_motion=Some(motion);
        let mut resumed=Park::new(SYSTEM,vec![],now-20);resumed.ships.insert(30,recovered);
        resumed.ships.insert(10,Ship::new(10,77,ShipStats::new(100.0,1e6,1.0,100.0,1.0),Pilot{group_id:26,..pilot},Vec3::new(-1e9,0.0,0.0),Vec3::ZERO));
        let mut w=crate::world::WorldState::default();w.parks.insert(SYSTEM,resumed);reconcile(&mut w);
        w.parks.get_mut(&SYSTEM).unwrap().advance(now);let actual=&w.parks[&SYSTEM].ships[&30];
        assert!((actual.pos-expected.pos).len()<0.02);assert!((actual.vel-expected.vel).len()<0.002);
        assert_eq!(actual.in_warp(),expected.in_warp());
    }

    async fn prepared(f:&crate::test_support::Database,owner:i64,home:i64,id:i64) {
        let session=session(owner,home);let ctx=f.ctx(&session);
        let ships=load(&ctx,SYSTEM).await.unwrap();install(&ctx,SYSTEM,ships).await.unwrap();
        let mut w=f.world.lock();let s=w.parks.get_mut(&SYSTEM).unwrap().ships.get_mut(&id).unwrap();
        s.pos=Vec3::new(345.0,456.0,567.0);s.vel=Vec3::new(3.0,4.0,5.0);
        let v=&mut s.fight.as_mut().unwrap().vitals;v.armor_damage=3.0;v.hull_damage=2.0;
        v.shield.set(combat::now_ms(),12.0);v.capacitor.set(combat::now_ms(),7.0);
        let d=s.drone.as_mut().unwrap();d.controller=Some(home);d.controller_owner=owner;
        d.order=Order::Mine{target:99,repeat:true};d.mined=true;
    }

    #[tokio::test]
    async fn restart_restores_drone_orders_damage_charges_and_current_control() {
        let (f,owner,home,id,other,other_ship)=fixture().await;
        prepared(&f,owner,home,id).await;
        f.world.lock().parks.get_mut(&SYSTEM).unwrap().ships.get_mut(&id).unwrap().drone.as_mut().unwrap()
            .assignment=Some(Assignment::Guard{pilot:other,ship:other_ship});
        checkpoint(&f.world,&f.store).await;
        let stale=capture(&f.world,Some(&[id])).pop().unwrap();
        f.world.lock().parks.get_mut(&SYSTEM).unwrap().ships.remove(&id);
        let s=session(owner,home);let ctx=f.ctx(&s);
        let loaded=load(&ctx,SYSTEM).await.unwrap();install(&ctx,SYSTEM,loaded).await.unwrap();
        {
            let w=f.world.lock();let ship=&w.parks[&SYSTEM].ships[&id];let d=ship.drone.as_ref().unwrap();
            assert_eq!(ship.pos,Vec3::new(345.0,456.0,567.0));assert_eq!(ship.vel,Vec3::new(3.0,4.0,5.0));
            assert_eq!(d.order,Order::Mine{target:99,repeat:true});assert!(d.mined);
            assert_eq!(d.assignment,Some(Assignment::Guard{pilot:other,ship:other_ship}));
            assert_eq!((d.client_id,d.controller,d.controller_client),(77,Some(home),77));
            let v=&ship.fight.as_ref().unwrap().vitals;
            assert_eq!((v.armor_damage,v.hull_damage),(3.0,2.0));
            assert!((v.shield.at(combat::now_ms())-12.0).abs()<1e-6);assert!((v.capacitor.at(combat::now_ms())-7.0).abs()<1e-6);
        }
        // Ownership transition invalidates an already captured old-owner save.
        assert_eq!(abandon(&ctx,&[id]).await,Value::Dict(vec![]));
        assert!(!save(&f.store,&f.world,stale).await.unwrap());
        f.world.lock().parks.get_mut(&SYSTEM).unwrap().ships.remove(&id);
        install(&ctx,SYSTEM,load(&ctx,SYSTEM).await.unwrap()).await.unwrap();
        {
            let w=f.world.lock();let d=w.parks[&SYSTEM].ships[&id].drone.as_ref().unwrap();
            assert_eq!((d.owner,d.controller,d.controller_client,d.client_id),(OWNER_SYSTEM,None,0,0));
        }
        let s2=session(other,other_ship);let ctx2=f.ctx(&s2);
        assert!(matches!(scoop(&ctx2,vec![id]).await,Outcome::Value(_)));
        assert!(!load(&ctx,SYSTEM).await.unwrap().iter().any(|s|s.id==id));
        assert_eq!(records(&f,id),0);
        assert!(matches!(launch(&ctx2,vec![id]).await,Outcome::Value(_)));
        let w=f.world.lock();let v=&w.parks[&SYSTEM].ships[&id].fight.as_ref().unwrap().vitals;
        assert_eq!((v.armor_damage,v.hull_damage),(3.0,2.0));
        assert!((v.capacitor.at(combat::now_ms())-7.0).abs()<1e-6);
    }

    #[tokio::test]
    async fn delayed_drone_snapshots_cannot_undo_moves_relaunches_or_newer_snapshots() {
        let (f,owner,home,id,_,_)=fixture().await;
        prepared(&f,owner,home,id).await;
        let old=capture(&f.world,Some(&[id])).pop().unwrap();
        f.world.lock().parks.get_mut(&SYSTEM).unwrap().ships.get_mut(&id).unwrap().pos.x=999.0;
        let newer=capture(&f.world,Some(&[id])).pop().unwrap();
        assert!(save(&f.store,&f.world,newer).await.unwrap());
        assert!(!save(&f.store,&f.world,old.clone()).await.unwrap());
        assert_eq!(x(&f,id),Some(999.0));
        // A snapshot captured before the drone went away and came back is
        // of an older generation.
        let snapshot=capture(&f.world,Some(&[id])).pop().unwrap();
        relaunch(&f,id,owner,home,Vec3::new(777.0,0.0,0.0));
        assert!(!save(&f.store,&f.world,snapshot.clone()).await.unwrap());
        assert_eq!(x(&f,id),Some(777.0));
        f.store.write(|tx|tx.delete_where("entity",&["itemID"],&[store::Value::Int(id)])).unwrap();
        assert!(!save(&f.store,&f.world,snapshot).await.unwrap());
        assert_eq!(records(&f,id),0);
    }

    #[tokio::test]
    async fn drone_restore_rechecks_inventory_after_profile_loading() {
        let (f,owner,home,id,other,other_ship)=fixture().await;
        let session=session(owner,home);let ctx=f.ctx(&session);
        let loaded=load(&ctx,SYSTEM).await.unwrap();
        f.store.write(|tx|{let row=tx.table("entity").get_id(id).unwrap().id;
            tx.update("entity",row,&[("ownerID",store::Value::Int(other)),("locationID",store::Value::Int(other_ship)),("flag",store::Value::Int(87))])}).unwrap();
        install(&ctx,SYSTEM,loaded).await.unwrap();
        assert!(!f.world.lock().parks[&SYSTEM].ships.contains_key(&id));
        assert!(!load(&ctx,SYSTEM).await.unwrap().iter().any(|s|s.id==id));
    }

    #[tokio::test]
    async fn corrupt_drone_state_recovers_inventory_without_reusing_obsolete_orders() {
        let (f,owner,home,id,_,_)=fixture().await;prepared(&f,owner,home,id).await;
        checkpoint(&f.world,&f.store).await;
        let obsolete=capture(&f.world,Some(&[id])).pop().unwrap();
        f.store.write(|tx|{let row=tx.table("launched_drone_state").get_id(id).unwrap().id;
            tx.update("launched_drone_state",row,&[("state",store::Value::Text("invalid JSON".into()))])}).unwrap();
        f.world.lock().parks.get_mut(&SYSTEM).unwrap().ships.remove(&id);
        let session=session(owner,home);let ctx=f.ctx(&session);
        install(&ctx,SYSTEM,load(&ctx,SYSTEM).await.unwrap()).await.unwrap();
        {
            let w=f.world.lock();let ship=&w.parks[&SYSTEM].ships[&id];let d=ship.drone.as_ref().unwrap();
            assert_eq!(ship.pos,Vec3::new(345.0,456.0,567.0));assert_eq!(d.order,Order::Idle);
            assert_ne!(d.generation(),Some(obsolete.identity.generation));
            assert_eq!(ship.fight.as_ref().unwrap().vitals.hull_damage,2.0);
        }
        assert!(!save(&f.store,&f.world,obsolete).await.unwrap());
    }

    #[tokio::test]
    async fn delayed_drone_loss_does_not_destroy_a_relaunched_generation() {
        let (f,owner,home,id,_,_)=fixture().await;prepared(&f,owner,home,id).await;
        let generation=f.world.lock().parks[&SYSTEM].ships[&id].drone.as_ref().unwrap().generation();
        // The drone went away and came back, launched again, before the
        // loss of the old generation was destroyed.
        let current=relaunch(&f,id,owner,home,Vec3::ZERO).generation;
        assert!(destroy_lost(&f.store,SYSTEM,owner,id,generation).await.unwrap().is_none());assert_ne!(generation,Some(current));
        assert!(f.store.read(|db|db.table("entity").get_id(id).is_some()));
        assert!(destroy_lost(&f.store,SYSTEM,owner+1,id,Some(current)).await.unwrap().is_none());
        assert!(destroy_lost(&f.store,SYSTEM+1,owner,id,Some(current)).await.unwrap().is_none());
        assert!(destroy_lost(&f.store,SYSTEM,owner,id,Some(current)).await.unwrap().is_some());
        assert!(f.store.read(|db|db.table("entity").get_id(id).is_none()));
        assert_eq!(records(&f,id),0);
    }
}
