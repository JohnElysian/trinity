//! Durable launched-drone state. Inventory remains authoritative: an item
//! transition invalidates its launch generation, and snapshots never create one.
use super::*;
use serde::{Deserialize, Serialize};
use tokio_postgres::GenericClient;

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

pub async fn ensure(db: &Client) -> Result<(),tokio_postgres::Error> {
    db.batch_execute(r#"
        CREATE SEQUENCE IF NOT EXISTS launched_drone_generation;
        CREATE TABLE IF NOT EXISTS launched_drone_state (
            drone_id integer PRIMARY KEY REFERENCES entity("itemID") ON DELETE CASCADE,
            generation bigint NOT NULL DEFAULT nextval('launched_drone_generation'),
            revision bigint NOT NULL DEFAULT 0,
            owner_id integer NOT NULL, system_id integer NOT NULL, state text NOT NULL);
        CREATE INDEX IF NOT EXISTS launched_drone_system ON launched_drone_state(system_id);
        CREATE OR REPLACE FUNCTION invalidate_drone_launch() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF ROW(OLD."ownerID",OLD."locationID",OLD.flag,OLD."typeID",OLD.quantity,OLD.singleton)
                IS DISTINCT FROM ROW(NEW."ownerID",NEW."locationID",NEW.flag,NEW."typeID",NEW.quantity,NEW.singleton) THEN
                DELETE FROM launched_drone_state WHERE drone_id=OLD."itemID";
            END IF;
            RETURN NEW;
        END $$;
        DO $$ BEGIN
            IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname='invalidate_drone_launch') THEN
                CREATE TRIGGER invalidate_drone_launch AFTER UPDATE OF "ownerID","locationID",flag,"typeID",quantity,singleton
                    ON entity FOR EACH ROW EXECUTE FUNCTION invalidate_drone_launch();
            END IF;
        END $$;
    "#).await
}

pub(super) async fn begin(db:&impl GenericClient,id:i64,owner:i64,system:i64,controller:Option<i64>,pos:Vec3,
    settings:Settings,range:f64) -> Result<Identity,tokio_postgres::Error> {
    let state=serde_json::to_string(&State::initial(owner,controller,pos,settings,range)).unwrap();
    let row=db.query_one("INSERT INTO launched_drone_state(drone_id,owner_id,system_id,state) VALUES($1,$2,$3,$4)
        ON CONFLICT(drone_id) DO UPDATE SET generation=nextval('launched_drone_generation'),revision=0,
            owner_id=EXCLUDED.owner_id,system_id=EXCLUDED.system_id,state=EXCLUDED.state RETURNING generation",
        &[&(id as i32),&(owner as i32),&(system as i32),&state]).await?;
    Ok(Identity{generation:row.get(0),revision:0})
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

async fn save(db:&mut Client,world:&World,snapshot:Snapshot) -> Result<bool,tokio_postgres::Error> {
    let tx=db.transaction().await?;
    let id=snapshot.ship.id;
    let owner=snapshot.ship.drone.as_ref().unwrap().owner;
    // Lock the inventory row first, in the same order as scoop/abandon/loss.
    // READ COMMITTED rechecks this predicate after a concurrent move commits.
    if tx.query_opt(r#"SELECT 1 FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID"
        JOIN "invGroups" g ON g."groupID"=t."groupID" WHERE e."itemID"=$1 AND e."ownerID"=$2
        AND e."locationID"=$3 AND e.flag=0 AND e.quantity=1 AND e.singleton=1 AND g."categoryID"=18 FOR UPDATE OF e"#,
        &[&(id as i32),&(owner as i32),&(snapshot.system as i32)]).await?.is_none() {return Ok(false)}
    let live=world.lock().parks.get(&snapshot.system).and_then(|p|p.ships.get(&id))
        .and_then(|s|s.drone.as_ref()).is_some_and(|d|!d.pending_bay && d.owner==owner
            && d.persistence.is_some_and(|p|p.generation==snapshot.identity.generation));
    if !live {return Ok(false)}
    let state=serde_json::to_string(&State::capture(&snapshot.ship,snapshot.tick)).unwrap();
    if tx.execute("UPDATE launched_drone_state SET revision=$3,state=$4 WHERE drone_id=$1 AND generation=$2 AND revision<$3
        AND owner_id=$5 AND system_id=$6",&[&(id as i32),&snapshot.identity.generation,&snapshot.identity.revision,&state,
        &(owner as i32),&(snapshot.system as i32)]).await?!=1 {return Ok(false)}
    tx.execute(r#"UPDATE entity SET x=$2,y=$3,z=$4 WHERE "itemID"=$1"#,
        &[&(id as i32),&snapshot.ship.pos.x,&snapshot.ship.pos.y,&snapshot.ship.pos.z]).await?;
    if let Some(f)=&snapshot.ship.fight {crate::svc::inflight::write_vitals(&tx,id,f,false).await?;}
    tx.commit().await?;
    Ok(true)
}

pub async fn checkpoint(world:&World,db:&mut Client) {
    for snapshot in capture(world,None) {
        if let Err(e)=save(db,world,snapshot).await {warn!("saving launched drone: {}",crate::logging::chain(&e));}
    }
}

pub(super) async fn ordered(ctx:&Ctx<'_>,ids:&[i64]) {
    let snapshots=capture(ctx.world,Some(ids));
    if snapshots.is_empty() {return}
    match crate::db::connect(ctx.db_url).await {
        Ok(mut db)=>for snapshot in snapshots {if let Err(e)=save(&mut db,ctx.world,snapshot).await {warn!("saving drone order: {e}");}},
        Err(e)=>warn!("saving drone orders: {e}"),
    }
}

pub(super) async fn vitals(db:&impl GenericClient,ids:&[i64]) -> Result<HashMap<i64,Vec<(i32,f64)>>,tokio_postgres::Error> {
    let ids:Vec<i32>=ids.iter().map(|id|*id as i32).collect();
    let mut out=HashMap::<i64,Vec<(i32,f64)>>::new();
    for row in db.query(r#"SELECT "itemID","attributeID",coalesce("valueFloat","valueInt"::float8)
        FROM entity_attributes WHERE "itemID"=ANY($1) AND "attributeID"=ANY($2)"#,
        &[&ids,&vec![attr::SHIELD_CHARGE,attr::CHARGE,attr::ARMOR_DAMAGE,attr::DAMAGE]]).await? {
        if let Some(v)=row.get::<_,Option<f64>>(2).filter(|v|v.is_finite()) {out.entry(row.get::<_,i32>(0) as i64).or_default().push((row.get(1),v));}
    }
    Ok(out)
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
pub async fn load(ctx:&Ctx<'_>,system:i64) -> Result<Vec<Ship>,tokio_postgres::Error> {
    let rows=ctx.db.query(r#"SELECT e."itemID",e."typeID",e."ownerID",coalesce(e.x,0),coalesce(e.y,0),coalesce(e.z,0),
        coalesce(c."corporationID",0),co."allianceID" FROM entity e
        JOIN "invTypes" t ON t."typeID"=e."typeID" JOIN "invGroups" g ON g."groupID"=t."groupID"
        LEFT JOIN "character_" c ON c."characterID"=e."ownerID" LEFT JOIN corporation co ON co."corporationID"=c."corporationID"
        WHERE e."locationID"=$1 AND e.flag=0 AND e.quantity=1 AND e.singleton=1 AND g."categoryID"=18 ORDER BY e."itemID""#,
        &[&(system as i32)]).await?;
    let type_ids=rows.iter().map(|r|r.get::<_,i32>(1) as i64).collect::<Vec<_>>();
    let types=npc::load_types(ctx.db,&type_ids).await?;
    let mut db=crate::db::connect(ctx.db_url).await?;
    let mut out=Vec::new(); let now=combat::now_ms();let mut rng=combat::Rng::seeded(now as u64);
    for r in rows {
        let id=r.get::<_,i32>(0) as i64;let type_id=r.get::<_,i32>(1) as i64;let owner=r.get::<_,i32>(2) as i64;
        let Some(ty)=types.get(&type_id) else {continue};
        let settings=Settings::load(ctx.db,owner).await?;
        let owner_ship=ctx.db.query_opt(r#"SELECT c."shipID" FROM "character_" c JOIN entity e ON e."itemID"=c."shipID"
            WHERE c."characterID"=$1 AND e."ownerID"=$1"#,&[&(owner as i32)]).await?
            .and_then(|r|r.get::<_,Option<i32>>(0)).map(i64::from);
        let loadout=crate::dogma::Loadout::load_with_drones(ctx.db,ctx.dogma,owner,owner_ship,&Default::default(),&[id]).await?;
        let attributes=loadout.find(id).map(|i|loadout.attributes(i));let ty=ty.with_attributes(attributes.as_ref());
        let range=loadout.character().map_or(20_000.0,|c|loadout.value(c,458).max(0.0));
        let tx=db.transaction().await?;
        let Some(item)=tx.query_opt(r#"SELECT x,y,z FROM entity WHERE "itemID"=$1 AND "ownerID"=$2 AND "locationID"=$3
            AND "typeID"=$4 AND flag=0 AND quantity=1 AND singleton=1 FOR UPDATE"#,
            &[&(id as i32),&(owner as i32),&(system as i32),&(type_id as i32)]).await? else {continue};
        let pos=Vec3::new(item.get::<_,Option<f64>>(0).unwrap_or(0.0),item.get::<_,Option<f64>>(1).unwrap_or(0.0),item.get::<_,Option<f64>>(2).unwrap_or(0.0));
        if !pos.is_finite() {continue}
        let (identity,state)=match tx.query_opt("SELECT generation,revision,state FROM launched_drone_state WHERE drone_id=$1 AND owner_id=$2 AND system_id=$3",
            &[&(id as i32),&(owner as i32),&(system as i32)]).await? {
            Some(row)=>{
                match serde_json::from_str::<State>(&row.get::<_,String>(2)).ok().filter(State::valid) {
                    Some(state)=>(Identity{generation:row.get(0),revision:row.get(1)},state),
                    None=>{
                        warn!(drone=id,"invalid durable drone state; preserving inventory position and vitals");
                        (begin(&tx,id,owner,system,None,pos,settings,range).await?,State::initial(owner,None,pos,settings,range))
                    },
                }
            },
            None=>(begin(&tx,id,owner,system,None,pos,settings,range).await?,State::initial(owner,None,pos,settings,range)),
        };
        let stored=vitals(&tx,&[id]).await?;
        let pilot=Pilot{type_id,group_id:ty.group_id,name:ty.name.clone(),char_id:owner,corp_id:r.get::<_,i32>(6) as i64,
            alliance_id:r.get::<_,Option<i32>>(7).map(i64::from),security:0.0,bounty:0.0};
        let mut ship=make(&ty,id,pilot,0,state.controller.unwrap_or(0),pos,now,&mut rng);
        ship.vel=Vec3::new(state.velocity[0],state.velocity[1],state.velocity[2]);ship.command=Command::Stop;
        apply_vitals(&mut ship,stored.get(&id),now);
        if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_damage>=f.vitals.hull_max) {continue}
        let d=ship.drone.as_mut().unwrap();d.persistence=Some(identity);d.controller=state.controller;d.controller_owner=state.controller_owner;
        d.controller_client=0;d.assignment=state.assignment;d.order=state.order;d.activity=state.activity;d.mined=state.mined;
        d.restored_motion=state.motion;
        d.settings=settings;d.control_range=range;d.next_think_ms=0;
        tx.commit().await?;out.push(ship);
    }
    Ok(out)
}

/// Revalidate under item locks immediately before making loaded drones live.
/// A scoop, loss, ownership transfer or relaunch during profile loading wins.
pub async fn install(ctx:&Ctx<'_>,system:i64,ships:Vec<Ship>) -> Result<(),tokio_postgres::Error> {
    if ships.is_empty() {return Ok(())}
    let mut db=crate::db::connect(ctx.db_url).await?;
    let tx=db.transaction().await?;
    let ids:Vec<i32>=ships.iter().map(|s|s.id as i32).collect();
    let rows=tx.query(r#"SELECT "itemID","ownerID","typeID",x,y,z FROM entity WHERE "itemID"=ANY($1)
        AND "locationID"=$2 AND flag=0 AND quantity=1 AND singleton=1 ORDER BY "itemID" FOR UPDATE"#,&[&ids,&(system as i32)]).await?;
    let records=tx.query("SELECT drone_id,generation,revision,state FROM launched_drone_state WHERE drone_id=ANY($1)",&[&ids]).await?;
    let records:HashMap<i64,_>=records.into_iter().map(|r|(r.get::<_,i32>(0) as i64,r)).collect();
    let mut ships:HashMap<i64,Ship>=ships.into_iter().map(|s|(s.id,s)).collect();
    let stored=vitals(&tx,&ids.iter().map(|i|*i as i64).collect::<Vec<_>>()).await?;
    let now=combat::now_ms();let mut valid=Vec::new();
    for row in rows {
        let id=row.get::<_,i32>(0) as i64;
        let Some(mut ship)=ships.remove(&id) else {continue};
        let d=ship.drone.as_mut().unwrap();
        let Some(record)=records.get(&id).filter(|r|d.persistence.is_some_and(|p|p.generation==r.get::<_,i64>(1))) else {continue};
        if d.owner!=row.get::<_,i32>(1) as i64 || ship.pilot.type_id!=row.get::<_,i32>(2) as i64 {continue}
        let Ok(state)=serde_json::from_str::<State>(&record.get::<_,String>(3)) else {continue};
        if !state.valid() {continue}
        ship.pos=Vec3::new(row.get::<_,Option<f64>>(3).unwrap_or(0.0),row.get::<_,Option<f64>>(4).unwrap_or(0.0),row.get::<_,Option<f64>>(5).unwrap_or(0.0));
        if !ship.pos.is_finite() {continue}
        ship.vel=Vec3::new(state.velocity[0],state.velocity[1],state.velocity[2]);
        d.persistence=Some(Identity{generation:record.get(1),revision:record.get(2)});
        d.client_id=0;d.controller_client=0;ship.client_id=0;
        d.controller=state.controller;d.controller_owner=state.controller_owner;d.assignment=state.assignment;
        d.order=state.order;d.activity=state.activity;d.mined=state.mined;
        d.restored_motion=(state.position==[ship.pos.x,ship.pos.y,ship.pos.z]).then_some(state.motion).flatten();
        apply_vitals(&mut ship,stored.get(&id),now);
        if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_damage>=f.vitals.hull_max) {continue}
        valid.push(ship);
    }
    {
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
    }
    // This transaction only reads. Releasing its locks after insertion lets
    // any waiting inventory operation observe and remove the live drone.
    tx.commit().await
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
        for (id,ty,who,location) in [(owner,1377,owner,home),(home,626,owner,SYSTEM),(id,2454,owner,SYSTEM),
            (other,1377,other,other_ship),(other_ship,626,other,SYSTEM)] {
            f.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton,x,y,z)
                VALUES($1,$2,$3,$4,0,1,1,123,234,345)"#,&[&(id as i32),&ty,&(who as i32),&(location as i32)]).await.unwrap();
        }
        for (item,attribute,value) in [(owner,attr::MAX_ACTIVE_DRONES,5.0),(other,attr::MAX_ACTIVE_DRONES,5.0),
            (home,283,100.0),(other_ship,283,100.0),(id,attr::CAPACITOR_CAPACITY,100.0),(id,attr::RECHARGE_RATE,0.0),
            (id,attr::SHIELD_CAPACITY,200.0),(id,attr::SHIELD_RECHARGE_RATE,0.0)] {
            f.db.execute(r#"INSERT INTO entity_attributes("itemID","attributeID","valueFloat") VALUES($1,$2,$3)"#,
                &[&(item as i32),&attribute,&value]).await.unwrap();
        }
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
    #[ignore="requires TRINITY_TEST_DATABASE with imported static data"]
    async fn restart_restores_drone_orders_damage_charges_and_current_control() {
        let (f,owner,home,id,other,other_ship)=fixture().await;
        prepared(&f,owner,home,id).await;
        f.world.lock().parks.get_mut(&SYSTEM).unwrap().ships.get_mut(&id).unwrap().drone.as_mut().unwrap()
            .assignment=Some(Assignment::Guard{pilot:other,ship:other_ship});
        let mut db=crate::db::connect(&f.url).await.unwrap();checkpoint(&f.world,&mut db).await;
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
        assert!(!save(&mut db,&f.world,stale).await.unwrap());
        f.world.lock().parks.get_mut(&SYSTEM).unwrap().ships.remove(&id);
        install(&ctx,SYSTEM,load(&ctx,SYSTEM).await.unwrap()).await.unwrap();
        {
            let w=f.world.lock();let d=w.parks[&SYSTEM].ships[&id].drone.as_ref().unwrap();
            assert_eq!((d.owner,d.controller,d.controller_client,d.client_id),(OWNER_SYSTEM,None,0,0));
        }
        let s2=session(other,other_ship);let ctx2=f.ctx(&s2);
        assert!(matches!(scoop(&ctx2,vec![id]).await,Outcome::Value(_)));
        assert!(!load(&ctx,SYSTEM).await.unwrap().iter().any(|s|s.id==id));
        assert_eq!(f.db.query_one("SELECT count(*) FROM launched_drone_state WHERE drone_id=$1",&[&(id as i32)]).await.unwrap().get::<_,i64>(0),0);
        assert!(matches!(launch(&ctx2,vec![id]).await,Outcome::Value(_)));
        let w=f.world.lock();let v=&w.parks[&SYSTEM].ships[&id].fight.as_ref().unwrap().vitals;
        assert_eq!((v.armor_damage,v.hull_damage),(3.0,2.0));
        assert!((v.capacitor.at(combat::now_ms())-7.0).abs()<1e-6);
    }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE with imported static data"]
    async fn delayed_drone_snapshots_cannot_undo_moves_relaunches_or_newer_snapshots() {
        let (f,owner,home,id,_,_)=fixture().await;
        prepared(&f,owner,home,id).await;
        let old=capture(&f.world,Some(&[id])).pop().unwrap();
        f.world.lock().parks.get_mut(&SYSTEM).unwrap().ships.get_mut(&id).unwrap().pos.x=999.0;
        let newer=capture(&f.world,Some(&[id])).pop().unwrap();
        let mut worker=crate::db::connect(&f.url).await.unwrap();
        assert!(save(&mut worker,&f.world,newer).await.unwrap());
        assert!(!save(&mut worker,&f.world,old.clone()).await.unwrap());
        assert_eq!(f.db.query_one(r#"SELECT x FROM entity WHERE "itemID"=$1"#,&[&(id as i32)]).await.unwrap().get::<_,f64>(0),999.0);
        let snapshot=capture(&f.world,Some(&[id])).pop().unwrap();
        let mut blocker=crate::db::connect(&f.url).await.unwrap();let blocker=blocker.transaction().await.unwrap();
        blocker.query_one(r#"SELECT "itemID" FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(id as i32)]).await.unwrap();
        let pid:i32=worker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
        let write=save(&mut worker,&f.world,snapshot.clone());
        let transition=async {
            tokio::time::timeout(std::time::Duration::from_secs(5),async {
                loop {
                    let wait:Option<String>=f.db.query_one("SELECT wait_event_type FROM pg_stat_activity WHERE pid=$1",&[&pid]).await.unwrap().get(0);
                    if wait.as_deref()==Some("Lock") {break} tokio::task::yield_now().await;
                }
            }).await.unwrap();
            // Away and back to the same owner/system is an ABA transition.
            blocker.execute(r#"UPDATE entity SET "locationID"=$2,flag=87 WHERE "itemID"=$1"#,&[&(id as i32),&(home as i32)]).await.unwrap();
            blocker.execute(r#"UPDATE entity SET "locationID"=$2,flag=0,x=777 WHERE "itemID"=$1"#,&[&(id as i32),&(SYSTEM as i32)]).await.unwrap();
            begin(&blocker,id,owner,SYSTEM,Some(home),Vec3::new(777.0,0.0,0.0),Settings::default(),20000.0).await.unwrap();
            blocker.commit().await.unwrap();
        };
        let (saved,())=tokio::join!(write,transition);assert!(!saved.unwrap());
        assert_eq!(f.db.query_one(r#"SELECT x FROM entity WHERE "itemID"=$1"#,&[&(id as i32)]).await.unwrap().get::<_,f64>(0),777.0);
        f.db.execute(r#"DELETE FROM entity WHERE "itemID"=$1"#,&[&(id as i32)]).await.unwrap();
        assert!(!save(&mut worker,&f.world,snapshot).await.unwrap());
        assert_eq!(f.db.query_one("SELECT count(*) FROM launched_drone_state WHERE drone_id=$1",&[&(id as i32)]).await.unwrap().get::<_,i64>(0),0);
    }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE with imported static data"]
    async fn drone_restore_rechecks_inventory_after_profile_loading() {
        let (f,owner,home,id,other,other_ship)=fixture().await;
        let session=session(owner,home);let ctx=f.ctx(&session);
        let loaded=load(&ctx,SYSTEM).await.unwrap();
        f.db.execute(r#"UPDATE entity SET "ownerID"=$2,"locationID"=$3,flag=87 WHERE "itemID"=$1"#,
            &[&(id as i32),&(other as i32),&(other_ship as i32)]).await.unwrap();
        install(&ctx,SYSTEM,loaded).await.unwrap();
        assert!(!f.world.lock().parks[&SYSTEM].ships.contains_key(&id));
        assert!(!load(&ctx,SYSTEM).await.unwrap().iter().any(|s|s.id==id));
    }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE with imported static data"]
    async fn corrupt_drone_state_recovers_inventory_without_reusing_obsolete_orders() {
        let (f,owner,home,id,_,_)=fixture().await;prepared(&f,owner,home,id).await;
        let mut db=crate::db::connect(&f.url).await.unwrap();checkpoint(&f.world,&mut db).await;
        let obsolete=capture(&f.world,Some(&[id])).pop().unwrap();
        f.db.execute("UPDATE launched_drone_state SET state='invalid JSON' WHERE drone_id=$1",&[&(id as i32)]).await.unwrap();
        f.world.lock().parks.get_mut(&SYSTEM).unwrap().ships.remove(&id);
        let session=session(owner,home);let ctx=f.ctx(&session);
        install(&ctx,SYSTEM,load(&ctx,SYSTEM).await.unwrap()).await.unwrap();
        {
            let w=f.world.lock();let ship=&w.parks[&SYSTEM].ships[&id];let d=ship.drone.as_ref().unwrap();
            assert_eq!(ship.pos,Vec3::new(345.0,456.0,567.0));assert_eq!(d.order,Order::Idle);
            assert_ne!(d.generation(),Some(obsolete.identity.generation));
            assert_eq!(ship.fight.as_ref().unwrap().vitals.hull_damage,2.0);
        }
        assert!(!save(&mut db,&f.world,obsolete).await.unwrap());
    }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE with imported static data"]
    async fn delayed_drone_loss_does_not_destroy_a_relaunched_generation() {
        let (f,owner,home,id,_,_)=fixture().await;prepared(&f,owner,home,id).await;
        let generation=f.world.lock().parks[&SYSTEM].ships[&id].drone.as_ref().unwrap().generation();
        let mut blocker=crate::db::connect(&f.url).await.unwrap();let tx=blocker.transaction().await.unwrap();
        tx.query_one(r#"SELECT "itemID" FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(id as i32)]).await.unwrap();
        let mut worker=crate::db::connect(&f.url).await.unwrap();let pid:i32=worker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
        let loss=destroy_lost(&mut worker,SYSTEM,owner,id,generation);
        let relaunch=async {
            tokio::time::timeout(std::time::Duration::from_secs(5),async {
                loop {
                    let wait:Option<String>=f.db.query_one("SELECT wait_event_type FROM pg_stat_activity WHERE pid=$1",&[&pid]).await.unwrap().get(0);
                    if wait.as_deref()==Some("Lock") {break} tokio::task::yield_now().await;
                }
            }).await.unwrap();
            tx.execute(r#"UPDATE entity SET "locationID"=$2,flag=87 WHERE "itemID"=$1"#,&[&(id as i32),&(home as i32)]).await.unwrap();
            tx.execute(r#"UPDATE entity SET "locationID"=$2,flag=0 WHERE "itemID"=$1"#,&[&(id as i32),&(SYSTEM as i32)]).await.unwrap();
            let next=begin(&tx,id,owner,SYSTEM,Some(home),Vec3::ZERO,Settings::default(),20000.0).await.unwrap();
            tx.commit().await.unwrap();next.generation
        };
        let (lost,current)=tokio::join!(loss,relaunch);
        assert!(lost.unwrap().is_none());assert_ne!(generation,Some(current));
        assert!(f.db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(id as i32)]).await.unwrap().is_some());
        assert!(destroy_lost(&mut worker,SYSTEM,owner+1,id,Some(current)).await.unwrap().is_none());
        assert!(destroy_lost(&mut worker,SYSTEM+1,owner,id,Some(current)).await.unwrap().is_none());
        assert!(destroy_lost(&mut worker,SYSTEM,owner,id,Some(current)).await.unwrap().is_some());
        assert!(f.db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(id as i32)]).await.unwrap().is_none());
        assert_eq!(f.db.query_one("SELECT count(*) FROM launched_drone_state WHERE drone_id=$1",&[&(id as i32)]).await.unwrap().get::<_,i64>(0),0);
    }
}
