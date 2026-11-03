//! Trinity ship maintenance inventory and custody. See boarding-45017.json for
//! original RPCs and the explicitly unresolved previous-hull boarding rules.
use std::collections::BTreeSet;
use eve_marshal::Value;
use tokio_postgres::GenericClient;
use crate::ballpark::{Ship, Vec3};
use crate::items::{Item, ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::world::{World, WorldState};
use super::{embark, inflight, space, starbase, moves::{MoveError, refuse}};
mod repair;
mod inventory;
pub(crate) use inventory::{InventoryWrite, Bay, pending_mutation, pending_tree_mutation};
pub use repair::{Repair,ensure,retry_repairs};

pub const FLAG: i64 = 90;
const CAPSULE: i64 = 670;
const CONFIGURE_RANGE: f64 = 5000.0;
const SCOOP_RANGE: f64 = 1500.0;

pub async fn is_array(db:&impl GenericClient,id:i64)->Result<bool,tokio_postgres::Error>{
    Ok(db.query_opt(r#"SELECT 1 FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID" WHERE e."itemID"=$1 AND t."groupID"=363"#,&[&(id as i32)]).await?.is_some())
}

#[derive(Debug, Clone, Copy)]
pub struct Configuration { pub pilot: i64, pub system: i64, pub allow_fleet: bool }

#[derive(Clone)]
struct Host {
    item: Item,
    system: i64,
    capacity: f64,
    range: f64,
    users: usize,
    structure: Option<crate::pos::Structure>,
    permission: i16,
    owner_corp: i64,
}

pub async fn handle(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let result = match call.method.as_str() {
        "GetShipConfiguration" => configuration(ctx, None).await,
        "ConfigureShip" => {
            let Some(Value::Dict(values)) = call.args.first() else { return Some(Outcome::custom("Supply a ship configuration dictionary.")); };
            if values.len()!=1 || values[0].0.as_string().as_deref()!=Some("allowGangSMBUsage") {
                return Some(Outcome::custom("That ship configuration option is not supported."));
            }
            let enabled=match &values[0].1 { Value::Bool(x)=>*x,Value::Int(0)=>false,Value::Int(1)=>true,_=>return Some(Outcome::custom("Fleet access must be true or false.")) };
            configuration(ctx, Some(enabled)).await
        }
        "ScoopToSMA" => operate(ctx, ctx.session.ship_id.unwrap_or(0), Operation::Scoop(call.arg_id(0))).await.map(|_|Value::None),
        "LaunchFromContainer" => {
            let Some(ids)=call.args.get(1).and_then(Value::seq) else {return Some(Outcome::custom("Supply the stored ships to launch."));};
            let Some(ids)=ids.iter().map(Value::as_i64).collect::<Option<Vec<_>>>() else {return Some(Outcome::custom("Invalid stored ship identifier."));};
            operate(ctx,call.arg_id(0),Operation::Launch(ids)).await.map(|_|Value::None)
        }
        _=>return None,
    };
    Some(outcome(result))
}

fn outcome(result:Result<Value,MoveError>)->Outcome {
    match result {Ok(v)=>Outcome::Value(v),Err(e)=>Outcome::custom(embark::error_text(e))}
}

async fn attribute(db:&impl GenericClient,type_id:i64,id:i32)->Result<f64,MoveError> {
    let r=db.query_opt(r#"SELECT COALESCE(a."valueFloat",a."valueInt"::float8,d."defaultValue",0)::float8
        FROM "dgmAttributeTypes" d LEFT JOIN "dgmTypeAttributes" a ON a."attributeID"=d."attributeID" AND a."typeID"=$1
        WHERE d."attributeID"=$2"#,&[&(type_id as i32),&id]).await?;
    Ok(r.map_or(0.0,|r|r.get(0)))
}

async fn host(db:&impl GenericClient,ctx:&Ctx<'_>,item:Item,access:starbase::Access)->Result<Host,MoveError> {
    let system=ctx.session.solar_system_id.ok_or_else(||MoveError::Refused("Maintenance operations require a ship in space.".into()))?;
    if item.location_id!=system || item.flag!=0 || !item.singleton || item.quantity!=1 {return refuse("That maintenance bay is not here in space.");}
    let row=db.query_one(r#"SELECT "groupID",COALESCE(capacity,0)::float8 FROM "invTypes" WHERE "typeID"=$1"#,&[&(item.type_id as i32)]).await?;
    let group:i32=row.get(0);
    let structure=if group==363 {
        Some(ctx.world.lock().parks.get(&system).and_then(|p|p.structures.get(&item.id)).cloned().ok_or_else(||MoveError::Refused("That maintenance array is not in space.".into()))?)
    } else {None};
    let capacity=if structure.is_some() {row.get(1)} else {
        if item.category_id!=6 || attribute(db,item.type_id,907).await?<=0.0 {return refuse("That ship has no maintenance bay.");}
        attribute(db,item.type_id,908).await?
    };
    let range=attribute(db,item.type_id,715).await?;
    let users=attribute(db,item.type_id,716).await?;
    if !capacity.is_finite() || capacity<=0.0 || !range.is_finite() || range<=0.0 || !users.is_finite() || users<1.0 {return refuse("That maintenance bay has no usable capacity or operational range.");}
    let permission=if structure.is_some() {
        let column=match access {starbase::Access::ViewPut=>"useViewPut",starbase::Access::Take=>"useViewPutTake",starbase::Access::Use=>"useUse"};
        db.query_opt(&format!("SELECT \"{column}\" FROM \"posStructures\" WHERE \"itemID\"=$1 FOR SHARE"),&[&(item.id as i32)]).await?.map_or(-1,|r|r.get(0))
    } else {0};
    let owner_corp=if structure.is_none() {
        let owner=db.query_opt(r#"SELECT COALESCE("corporationID",0) FROM "character_" WHERE "characterID"=$1 FOR SHARE"#,&[&(item.owner_id as i32)]).await?
            .ok_or_else(||MoveError::Refused("The maintenance ship's pilot is unavailable.".into()))?;
        i64::from(owner.get::<_,i32>(0))
    }else{0};
    if let Some(actor)=db.query_opt(r#"SELECT COALESCE("corporationID",0) FROM "character_" WHERE "characterID"=$1 FOR SHARE"#,&[&(ctx.session.char_id.unwrap_or(0) as i32)]).await? {
        if i64::from(actor.get::<_,i32>(0))!=ctx.session.corp_id.unwrap_or(0){return refuse("Your corporation membership changed; refresh the session before using this bay.");}
    }
    Ok(Host {item,system,capacity,range,users:users as usize,structure,permission,owner_corp})
}

fn alive(ship:&Ship)->bool {
    !ship.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6)
}

fn pilot<'a>(ctx:&Ctx<'_>,w:&'a WorldState,system:i64,reserved:&BTreeSet<i64>)->Result<&'a Ship,MoveError> {
    let ship=w.parks.get(&system).and_then(|p|p.ships.get(&ctx.session.ship_id.unwrap_or(0)))
        .filter(|s|s.is_player() && s.client_id==ctx.client_id && Some(s.pilot.char_id)==ctx.session.char_id)
        .ok_or_else(||MoveError::Refused("You are no longer piloting that ship here.".into()))?;
    if ship.identity_reserved!=reserved.contains(&ship.id) || !alive(ship) || ship.warping() || ship.cloaked {
        return refuse("The ship is busy, destroyed, warping or cloaked.");
    }
    Ok(ship)
}

fn check(ctx:&Ctx<'_>,w:&WorldState,h:&Host,reserved:&BTreeSet<i64>,admission:bool)->Result<Vec3,MoveError> {
    let actor=pilot(ctx,w,h.system,reserved)?;
    let park=&w.parks[&h.system];
    let (pos,radius)=if let Some(expected)=&h.structure {
        let s=park.structures.get(&h.item.id).ok_or_else(||MoveError::Refused("The maintenance array left space.".into()))?;
        if s.owner_id!=h.item.owner_id || s.type_id!=h.item.type_id || s.state!=expected.state || s.tower!=expected.tower
            || !matches!(s.state,crate::pos::state::ANCHORED|crate::pos::state::ONLINE|crate::pos::state::OPERATING)
            || s.vitals.hull_damage>=s.vitals.hull_max || h.permission<0 || !starbase::allowed(ctx,s,h.permission) {
            return refuse("The maintenance array is unavailable or its settings deny access.");
        }
        (s.pos,s.radius)
    } else {
        let s=park.ships.get(&h.item.id).filter(|s|s.is_pilot()).ok_or_else(||MoveError::Refused("The maintenance ship has no active pilot.".into()))?;
        if s.pilot.char_id!=h.item.owner_id || s.pilot.corp_id!=h.owner_corp || s.pilot.type_id!=h.item.type_id || s.identity_reserved!=reserved.contains(&s.id)
            || !alive(s) || s.warping() || s.cloaked {return refuse("The maintenance ship is no longer available.");}
        let mine=Some(s.pilot.char_id)==ctx.session.char_id;
        let corp=ctx.session.corp_id.is_some_and(|c|c!=0 && c==s.pilot.corp_id);
        let fleet=w.maintenance.get(&s.id).is_some_and(|c|c.system==h.system && c.pilot==s.pilot.char_id && c.allow_fleet)
            && w.gangs.of(s.pilot.char_id).is_some_and(|g|Some(g)==w.gangs.of(actor.pilot.char_id));
        if !mine && !corp && !fleet {return refuse("The pilot must be in your corporation or allow your fleet to use this maintenance bay.");}
        (s.pos,s.stats.radius)
    };
    let gap=(actor.pos-pos).len()-actor.stats.radius-radius;
    if !gap.is_finite() || gap>h.range.min(CONFIGURE_RANGE) {return refuse("Move within the maintenance bay's operational range.");}
    if admission {
        // Original text counts piloted ships physically present, not windows.
        let count=park.ships.values().filter(|s|s.is_pilot() && (s.pos-pos).len()-s.stats.radius-radius<=h.range).count();
        if count>h.users {return refuse(format!("The maintenance bay permits at most {} nearby piloted ships; {count} are present.",h.users));}
    }
    Ok(pos)
}

async fn configuration(ctx:&Ctx<'_>,change:Option<bool>)->Result<Value,MoveError> {
    let id=ctx.session.ship_id.ok_or_else(||MoveError::Refused("You have no active ship.".into()))?;
    let mut db=crate::db::connect(ctx.db_url).await?;let mut tx=ItemTxn::begin(&mut db).await?;
    let item=tx.item(id).await?.ok_or_else(||MoveError::Refused("Your ship no longer exists.".into()))?;
    let h=host(tx.tx(),ctx,item,starbase::Access::Use).await?;
    if h.structure.is_some() {return refuse("Configure the ship you are piloting.");}
    let mut w=ctx.world.lock();check(ctx,&w,&h,&BTreeSet::new(),false)?;
    let pilot=ctx.session.char_id.unwrap();
    if let Some(allow_fleet)=change {w.maintenance.insert(id,Configuration {pilot,system:h.system,allow_fleet});}
    let enabled=w.maintenance.get(&id).is_some_and(|c|c.system==h.system && c.pilot==pilot && c.allow_fleet);
    Ok(if change.is_some() {Value::None} else {Value::Dict(vec![(Value::bytes("allowGangSMBUsage"),Value::Bool(enabled))])})
}

/// Authorized bay-only inventory view; never grants access to the host's cargo.
pub async fn view(ctx:&Ctx<'_>,id:i64)->Option<(f64,i64)> {
    let mut db=crate::db::connect(ctx.db_url).await.ok()?;
    let mut tx=ItemTxn::begin(&mut db).await.ok()?;
    let item=tx.item(id).await.ok()??;
    if let Some(station)=ctx.session.station_id {
        if Some(item.owner_id)!=ctx.session.char_id || item.location_id!=station || item.flag!=4 || item.category_id!=6 || !item.singleton
            || attribute(tx.tx(),item.type_id,907).await.ok()?<=0.0 {return None;}
        return Some((attribute(tx.tx(),item.type_id,908).await.ok()?,FLAG));
    }
    let h=host(tx.tx(),ctx,item,starbase::Access::ViewPut).await.ok()?;
    check(ctx,&ctx.world.lock(),&h,&BTreeSet::new(),false).ok()?;
    Some((h.capacity,if h.structure.is_some(){0}else{FLAG}))
}

struct Reservation {world:World,system:i64,ids:BTreeSet<i64>}
impl Reservation {
    fn new(ctx:&Ctx<'_>,h:&Host,extra:&[i64])->Result<Self,MoveError> {
        let mut w=ctx.world.lock();check(ctx,&w,h,&BTreeSet::new(),true)?;
        let mut ids=BTreeSet::from([ctx.session.ship_id.unwrap()]);
        if h.structure.is_none(){ids.insert(h.item.id);}
        ids.extend(extra.iter().copied());
        let park=w.parks.get_mut(&h.system).unwrap();
        for id in &ids {if park.ships.get(id).is_none_or(|s|s.identity_reserved || !alive(s)){return refuse("A ship is already changing identity.");}}
        for id in &ids {park.ships.get_mut(id).unwrap().identity_reserved=true;}
        Ok(Self{world:ctx.world.clone(),system:h.system,ids})
    }
}
impl Drop for Reservation {
    fn drop(&mut self){if let Some(p)=self.world.lock().parks.get_mut(&self.system){for id in &self.ids{if let Some(s)=p.ships.get_mut(id){s.identity_reserved=false;}}}}
}

#[derive(Clone)]
enum Operation {Store,Scoop(i64),Launch(Vec<i64>),Board(i64)}
pub async fn store(ctx:&Ctx<'_>,host:i64)->Result<i64,MoveError>{operate(ctx,host,Operation::Store).await}
pub async fn board_stored(ctx:&Ctx<'_>,host:i64,hull:i64)->Result<i64,MoveError>{operate(ctx,host,Operation::Board(hull)).await}

async fn cargo_allowed(tx:&mut ItemTxn<'_>,h:&Host,item:&Item,quantity:i64)->Result<(),MoveError> {
    stored_allowed(tx,h,item,quantity).await?;
    let flag=if h.structure.is_some(){0}else{FLAG};
    let used=used(tx.tx(),h.item.id,flag,Some(item.id)).await?;
    let group:i32=tx.tx().query_one(r#"SELECT "groupID" FROM "invTypes" WHERE "typeID"=$1"#,&[&(item.type_id as i32)]).await?.get(0);
    let volume=unit_volume(item.type_id,group,item.singleton,item.volume)*quantity as f64;
    if !volume.is_finite() || volume<=0.0 || used+volume>h.capacity+1e-6 {return refuse("There is not enough room in the maintenance bay.");}
    Ok(())
}

async fn stored_allowed(tx:&mut ItemTxn<'_>,h:&Host,item:&Item,quantity:i64)->Result<(),MoveError> {
    if item.category_id!=6 || item.type_id==CAPSULE || quantity<=0 || quantity>item.quantity || (item.singleton && item.quantity!=1) {return refuse("Only valid ship items can be stored in this maintenance bay.");}
    if matches!(h.item.type_id,28352|28712) {
        let g:i32=tx.tx().query_one(r#"SELECT "groupID" FROM "invTypes" WHERE "typeID"=$1"#,&[&(item.type_id as i32)]).await?.get(0);
        if ![28,380,463,543].contains(&g){return refuse("This bay accepts industrial ships, transports, mining barges and exhumers.");}
    }
    let rows=tx.tx().query(r#"SELECT "itemID" FROM entity WHERE "locationID"=$1 ORDER BY "itemID""#,&[&(item.id as i32)]).await?;
    if !item.singleton && !rows.is_empty(){return refuse("A packaged ship cannot contain other items.");}
    for row in rows {
        let Some(child)=tx.item(i64::from(row.get::<_,i32>(0))).await? else {continue;};
        if child.location_id!=item.id {continue;}
        if child.owner_id!=item.owner_id || matches!(child.flag,7|36|57|61|88|89) {return refuse("That ship contains property which cannot be stored.");}
        if child.flag==5 && child.category_id!=8 {return refuse("Remove cargo other than charges before storing this ship.");}
        if child.flag==FLAG {return refuse("Empty the ship's maintenance bay before storing it.");}
    }
    Ok(())
}

async fn empty_ship(db:&impl GenericClient,ctx:&Ctx<'_>,item:&Item,pos:Vec3)->Result<Ship,MoveError> {
    let p=inflight::profile(db,ctx.dogma,0,item.id,&BTreeSet::new(),crate::combat::now_ms()).await?
        .ok_or_else(||MoveError::Refused("The stored ship profile could not be loaded.".into()))?;
    let mut s=Ship::new(item.id,0,p.stats,p.pilot,pos,Vec3::ZERO);s.fight=Some(p.fighter);s.unpiloted=true;s.pilot.char_id=item.owner_id;
    if let Some(r)=db.query_opt(r#"SELECT c."corporationID",co."allianceID" FROM "character_" c LEFT JOIN corporation co ON co."corporationID"=c."corporationID" WHERE c."characterID"=$1"#,&[&(item.owner_id as i32)]).await? {
        s.pilot.corp_id=r.get::<_,Option<i32>>(0).map_or(0,i64::from);s.pilot.alliance_id=r.get::<_,Option<i32>>(1).map(i64::from);
    }
    Ok(s)
}

async fn actor_lock(ctx:&Ctx<'_>,tx:&ItemTxn<'_>)->Result<(),MoveError> {
    let (Some(pilot),Some(ship),Some(system),None)=(ctx.session.char_id,ctx.session.ship_id,ctx.session.solar_system_id,ctx.session.station_id) else{return refuse("You must be in space.");};
    if !super::login::lock_character_lifecycle(tx.tx(),pilot as i32).await? {return refuse("The character is unavailable.");}
    if tx.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2 AND "solarSystemID"=$3 AND "stationID"=0 FOR UPDATE"#,
        &[&(pilot as i32),&(ship as i32),&(system as i32)]).await?.is_none(){return refuse("Your ship or location changed.");}
    Ok(())
}

async fn operate(ctx:&Ctx<'_>,host_id:i64,op:Operation)->Result<i64,MoveError> {
    let actor=ctx.session.ship_id.unwrap_or(0);let me=ctx.session.char_id.unwrap_or(0);
    if host_id==actor && matches!(op,Operation::Store|Operation::Board(_)){return refuse("You cannot store or board your own current ship through itself.");}
    let mut db=crate::db::connect(ctx.db_url).await?;let mut tx=ItemTxn::begin(&mut db).await?;actor_lock(ctx,&tx).await?;
    let mut ids=BTreeSet::from([actor,host_id]);
    match &op {Operation::Scoop(id)|Operation::Board(id)=>{ids.insert(*id);},Operation::Launch(v)=>{if v.is_empty()||v.len()>100{return refuse("Select between one and one hundred stored ships.");}ids.extend(v);},_=>{}}
    for id in ids {if tx.item(id).await?.is_none(){return refuse("A ship or bay no longer exists.");}}
    let old=tx.item(actor).await?.unwrap();
    if old.owner_id!=me || old.location_id!=ctx.session.solar_system_id.unwrap() || old.flag!=0 || !old.singleton || old.quantity!=1{return refuse("You no longer control that ship.");}
    let access=if matches!(op,Operation::Launch(_)|Operation::Board(_)){starbase::Access::Take}else{starbase::Access::ViewPut};
    let host_item=tx.item(host_id).await?.unwrap();
    let h=host(tx.tx(),ctx,host_item,access).await?;
    let mut expected=None;
    if let Operation::Scoop(id)=op {
        if id==actor || id==host_id{return refuse("The ship cannot scoop itself.");}
        let hull=tx.item(id).await?.unwrap();
        if hull.location_id!=h.system || hull.flag!=0 || hull.owner_id!=me {return refuse("You can scoop your own abandoned ships in this system.");}
        if tx.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "shipID"=$1"#,&[&(id as i32)]).await?.is_some(){return refuse("That ship has a pilot.");}
        expected=Some(hull);
    }
    if matches!(op,Operation::Board(_)) && old.type_id!=CAPSULE {return refuse("Eject into a capsule before boarding a stored ship.");}
    let reserve=Reservation::new(ctx,&h,&expected.as_ref().map(|i|vec![i.id]).unwrap_or_default())?;
    let validate=|w:&WorldState|->Result<Vec3,MoveError>{
        let pos=check(ctx,w,&h,&reserve.ids,true)?;
        if let Some(item)=&expected {
            let p=&w.parks[&h.system];let hull=p.ships.get(&item.id).ok_or_else(||MoveError::Refused("The abandoned ship left space.".into()))?;
            if !hull.unpiloted || hull.pilot.char_id!=item.owner_id || hull.warping() || hull.cloaked || !alive(hull)
                || !hull.identity_reserved || p.surface_distance(actor,item.id).is_none_or(|d|d>SCOOP_RANGE || !d.is_finite()) {return refuse("Move within 1500 metres of your abandoned ship.");}
        }
        Ok(pos)
    };
    validate(&ctx.world.lock())?;
    let mut removed=vec![];let mut added=vec![];let mut next=actor;let mut stored_prepared=None;
    let mut stored_generation=None;
    let mut recipients=vec![ctx.client_id];recipients.extend(ctx.world.lock().clients_of(h.item.owner_id));
    let flag=if h.structure.is_some(){0}else{FLAG};
    match op {
        Operation::Store|Operation::Scoop(_)=>{
            let item=expected.clone().unwrap_or_else(||old.clone());cargo_allowed(&mut tx,&h,&item,1).await?;
            let live=ctx.world.lock().parks[&h.system].ships[&item.id].clone();
            if expected.is_none() {
                for module in live.fight.iter().flat_map(|f|&f.modules) {
                    if attribute(tx.tx(),module.type_id,906).await?>0.0{return refuse("An active module prevents ejection; wait for its cycle to end before storing the ship.");}
                }
            }
            let empty=embark::empty_profile(tx.tx(),ctx,&live).await?;
            if let Some(f)=&empty.fight{inflight::write_vitals(tx.tx(),item.id,f,false).await?;}
            tx.move_to(item.id,host_id,flag).await?;stored_prepared=Some(empty);removed.push(item.id);
            stored_generation=Some(repair::begin(tx.tx(),item.id,host_id,item.owner_id,flag).await?);
            if expected.is_none(){
                next=tx.create(NewItem{type_id:CAPSULE,owner_id:me,location_id:h.system,flag:0,quantity:1,singleton:true}).await?;
                tx.tx().execute(r#"UPDATE "character_" SET "shipID"=$2 WHERE "characterID"=$1"#,&[&(me as i32),&(next as i32)]).await?;
                let p=inflight::profile(tx.tx(),ctx.dogma,me,next,&BTreeSet::new(),crate::combat::now_ms()).await?.unwrap();
                let mut capsule=Ship::new(next,ctx.client_id,p.stats,p.pilot,live.pos,Vec3::ZERO);capsule.fight=Some(p.fighter);added.push(capsule);
            }
        }
        Operation::Launch(ids)=>{
            for id in ids {
                let mut item=tx.item(id).await?.ok_or_else(||MoveError::Refused("That stored ship no longer exists.".into()))?;
                repair::ready(ctx.world,id)?;
                if item.location_id!=host_id || item.flag!=flag || item.category_id!=6 || item.type_id==CAPSULE || item.quantity<1 || (item.singleton && item.quantity!=1){return refuse("That item is not a stored ship in this bay.");}
                if tx.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "shipID"=$1"#,&[&(id as i32)]).await?.is_some(){return refuse("A stored ship is already piloted.");}
                if !item.singleton {
                    if item.quantity>1 {tx.set_quantity(id,item.quantity-1).await?;item.id=tx.create(NewItem{type_id:item.type_id,owner_id:item.owner_id,location_id:host_id,flag,quantity:1,singleton:true}).await?;}
                    else{tx.set_singleton(id,true).await?;}
                }
                tx.move_to(item.id,h.system,0).await?;
                let pos=validate(&ctx.world.lock())?;
                let ship=empty_ship(tx.tx(),ctx,&item,pos).await?;added.push(ship);
                recipients.extend(ctx.world.lock().clients_of(item.owner_id));
            }
        }
        Operation::Board(id)=>{
            let item=tx.item(id).await?.unwrap();
            repair::ready(ctx.world,id)?;
            if item.location_id!=host_id || item.flag!=flag || item.category_id!=6 || !item.singleton || item.type_id==CAPSULE{return refuse("Select an assembled stored ship in this bay.");}
            if tx.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "shipID"=$1"#,&[&(id as i32)]).await?.is_some(){return refuse("That ship already has a pilot.");}
            embark::can_fly(tx.tx(),me,item.type_id).await?;
            if !tx.transfer_tree(id,item.owner_id,me).await?{return refuse("That ship's contents cannot be transferred to the new pilot.");}
            tx.move_to(id,h.system,0).await?;tx.destroy(actor).await?;
            tx.tx().execute(r#"UPDATE "character_" SET "shipID"=$2 WHERE "characterID"=$1"#,&[&(me as i32),&(id as i32)]).await?;
            let p=inflight::profile(tx.tx(),ctx.dogma,me,id,&BTreeSet::new(),crate::combat::now_ms()).await?.unwrap();
            let mut ship=Ship::new(id,ctx.client_id,p.stats,p.pilot,ctx.world.lock().parks[&h.system].ships[&actor].pos,Vec3::ZERO);ship.fight=Some(p.fighter);
            if !alive(&ship){return refuse("The stored ship is too damaged to board.");}
            added.push(ship);removed.push(actor);next=id;recipients.extend(ctx.world.lock().clients_of(item.owner_id));
        }
    }
    {
        let mut w=ctx.world.lock();if let Some(p)=w.parks.get_mut(&h.system){p.advance(crate::destiny::tick_now());}
        let pos=validate(&w)?;let park=&w.parks[&h.system];
        if let Some(s)=&mut stored_prepared{*s=embark::with_profile(&park.ships[&s.id],s);}
        for (index,s) in added.iter_mut().enumerate(){
            let at=if next!=actor{park.ships[&actor].pos}else{let radius=park.ball(host_id).map_or(0.0,|b|b.1);pos+Vec3::new(radius+s.stats.radius+100.0+index as f64*1000.0,0.0,0.0)};
            s.arrive(at);
        }
    }
    if let Some(s)=&stored_prepared{if let Some(f)=&s.fight{inflight::write_vitals(tx.tx(),s.id,f,false).await?;}}
    for s in &added{tx.set_position(s.id,s.pos.tuple()).await?;if let Some(f)=&s.fight{inflight::write_vitals(tx.tx(),s.id,f,false).await?;}}
    validate(&ctx.world.lock())?;
    let events=tx.commit().await?;
    let pending={
    let mut w=ctx.world.lock();
    let mut pending=None;
    for id in removed{
        if let Some(live)=space::remove_ship(&mut w,h.system,id) {
            if let (Some(prepared),Some(generation))=(&stored_prepared,stored_generation) {
                let final_hull=embark::with_profile(&live,prepared);
                if let Some(fighter)=final_hull.fight {
                    let snapshot=Repair{id,host:host_id,owner:final_hull.pilot.char_id,flag,generation,at_ms:crate::combat::now_ms(),fighter};
                    w.maintenance_repairs.insert(id,snapshot.clone());pending=Some(snapshot);
                }
            }
        }
    }
    for s in added{space::add_ship(&mut w,h.system,s);}
    recipients.sort_unstable();recipients.dedup();for e in events{w.notify(&recipients,"OnItemChange",e);}
    pending
    };
    if let Some(snapshot)=pending {repair::attempt(ctx.world,ctx.db_url,snapshot).await;}
    drop(reserve);Ok(next)
}

#[cfg(test)] mod tests;

// Original45017 common/script/sys/cfg.py GetItemVolume and __containerVolumes__.
pub(crate) fn unit_volume(type_id:i64,group:i32,singleton:bool,assembled:f64)->f64 {
    if singleton || type_id==11019 {return assembled;}
    match group {
        324|830|893|25|831|237|834=>2500.0,
        419|540=>15000.0,
        27|898|381|900=>50000.0,
        883|547|485|513|902|659=>1000000.0,
        29|31=>500.0,
        906|26|833|358|894|832=>10000.0,
        420|541=>5000.0,
        543|463=>3750.0,
        28|380=>20000.0,
        30=>10000000.0,
        _=>assembled,
    }
}

pub async fn used(db:&impl GenericClient,host:i64,flag:i64,except:Option<i64>)->Result<f64,MoveError> {
    let rows=db.query(r#"SELECT e."typeID",t."groupID",e.singleton,e.quantity,COALESCE(t.volume,0)::float8
        FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID"
        WHERE e."locationID"=$1 AND e.flag=$2 AND e."itemID"<>$3"#,&[&(host as i32),&(flag as i32),&(except.unwrap_or(0) as i32)]).await?;
    Ok(rows.iter().map(|r|unit_volume(i64::from(r.get::<_,i32>(0)),r.get(1),r.get::<_,i32>(2)!=0,r.get(4))*f64::from(r.get::<_,i32>(3))).sum())
}

