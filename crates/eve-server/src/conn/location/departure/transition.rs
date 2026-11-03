//! Gate, station and administrative transitions share the paid jump's live
//! identity reservation. Only successful COMMIT removes a hull from combat.
use super::*;
use super::reserved::{Mobility,Reservation,CommitState};
use crate::{combat,conn::Server,items::ItemTxn,session::Session};
#[cfg(test)] mod tests;

#[derive(Clone,Copy,Debug)]
pub(in crate::conn::location) struct Place {
    pub station:Option<i64>,pub system:i64,pub constellation:i64,pub region:i64,pub at:Vec3,
}

#[derive(Clone,Copy)]
pub(in crate::conn::location) enum Request {
    Gate {from_gate:i64,to_gate:i64,to:Place},
    Dock {station:i64,towed:bool},
    Admin {to:Place},
}

#[derive(Debug)]
pub(in crate::conn::location) struct Arrived {
    pub place:Place,
    pub ship_id:i64,
    pub docking:Option<(Vec3,Vec3,i64)>,
}

#[derive(Debug)]
pub(in crate::conn::location) enum Failure {Refused(String),Pending(String)}
impl From<String> for Failure {fn from(reason:String)->Self {Self::Refused(reason)}}
impl From<&str> for Failure {fn from(reason:&str)->Self {Self::Refused(reason.into())}}
impl std::fmt::Display for Failure {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {match self {Self::Refused(s)|Self::Pending(s)=>f.write_str(s)}}
}

fn validate_live(w:&WorldState,reservation:&Reservation,request:Request,from:i64,ship:i64)->std::result::Result<(),String> {
    reservation.current(w,true)?;
    let park=&w.parks[&from];
    match request {
        Request::Gate {from_gate,..}=>{
            if !park.surface_distance(ship,from_gate).is_some_and(|d|d.is_finite() && d<=JUMP_RANGE*RANGE_SLACK) {
                return Err("Your ship is no longer within range of that stargate.".into());
            }
        }
        Request::Dock {station,towed}=>{
            if !space::is_station(park,station) {return Err("That station is not in this solar system.".into());}
            if !towed && !park.surface_distance(ship,station).is_some_and(|d|d.is_finite() && d<=DOCK_RANGE) {
                return Err("Your ship is no longer within docking range.".into());
            }
        }
        Request::Admin {..}=>{},
    }
    Ok(())
}

/// Kept separate so a future cancellation before COMMIT releases the source
/// reservation. The connection calls it in an owned task through publication.
pub(in crate::conn::location) async fn run(server:&Server,session:&Session,client:i64,request:Request)->std::result::Result<Arrived,Failure> {
    let lease=server.db.lease().await.map_err(|error|Failure::Refused(format!("The departure database is unavailable: {error}")))?;
    let ctx=crate::services::Ctx {bulk:&server.bulk,dogma:&server.dogma,session,db:&lease,
        db_url:&server.db_url,node_id:server.node_id,world:&server.world,client_id:client};
    let pilot=session.char_id.ok_or("You have no active character.")?;
    let ship=session.ship_id.ok_or("You have no active ship.")?;
    let from=session.solar_system_id2.or(session.solar_system_id).ok_or("Your current system is unknown.")?;
    let source_station=session.station_id;
    if !matches!(request,Request::Admin{..}) && source_station.is_some() {return Err("You must be in space.".into());}
    match request {
        Request::Dock {towed:true,..} if session.role&(8|16|262144)==0=>return Err("Only a game master may request a remote tow.".into()),
        Request::Admin {..} if session.role&(8|262144)==0=>return Err("You are not allowed to relocate ships.".into()),
        Request::Admin {to} if source_station.is_some() && to.station.is_none()=>return Err("Undock before relocating into space.".into()),
        _=>{},
    }
    let mut db=crate::db::connect(&server.db_url).await.map_err(travel_failed)?;
    let mut txn=ItemTxn::begin(&mut db).await.map_err(travel_failed)?;
    if !crate::svc::login::lock_character_lifecycle(txn.tx(),pilot as i32).await.map_err(travel_failed)? {
        return Err("Your character is no longer available.".into());
    }
    let current=txn.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2
        AND "solarSystemID"=$3 AND COALESCE("stationID",0)=$4 FOR UPDATE"#,
        &[&(pilot as i32),&(ship as i32),&(from as i32),&(source_station.unwrap_or(0) as i32)]).await.map_err(travel_failed)?;
    if current.is_none() {return Err("Your active ship or location changed before departure.".into());}
    if crate::losses::pending::blocked_character(txn.tx(),pilot).await.map_err(travel_failed)?
        || crate::travel::blocked_character_in_memory(&server.world,pilot)
        || crate::svc::jumps::portal::blocked_character_in_memory(&server.world,pilot) {return Err("Your earlier travel, portal activation or hull loss is still pending.".into());}
    let hull=txn.item(ship).await.map_err(travel_failed)?.ok_or("Your ship no longer exists.")?;
    if hull.owner_id!=pilot || hull.location_id!=source_station.unwrap_or(from) || hull.flag!=if source_station.is_some(){4}else{0}
        || hull.quantity!=1 || !hull.singleton || hull.category_id!=6 {
        return Err("Your active ship is no longer available here.".into());
    }
    let mut reservation=if source_station.is_none() {
        let mut w=server.world.lock();
        w.parks.get_mut(&from).ok_or("Your ship is no longer in space.")?.advance(destiny::tick_now());
        let mobility=match request {Request::Gate{..}=>Mobility::Jump,Request::Dock{..}=>Mobility::Dock,Request::Admin{..}=>Mobility::Admin};
        Some(Reservation::reserve(&ctx,&mut w,from,mobility,0.0)?)
    }else{None};
    if let Some(r)=&reservation {validate_live(&server.world.lock(),r,request,from,ship)?;}
    let place=match request {
        Request::Gate {from_gate,to_gate,to}=>{
            let row=txn.tx().query_opt(r#"SELECT d."solarSystemID",d."constellationID",d."regionID",d.x,d.y,d.z,COALESCE(d.radius,0)::float8
                FROM "mapJumps" j JOIN "mapDenormalize" s ON s."itemID"=j."stargateID" JOIN "mapDenormalize" d ON d."itemID"=j."celestialID"
                WHERE j."stargateID"=$1 AND j."celestialID"=$2 AND s."solarSystemID"=$3 AND s."groupID"=10 AND d."groupID"=10 FOR SHARE OF j,s,d"#,
                &[&(from_gate as i32),&(to_gate as i32),&(from as i32)]).await.map_err(travel_failed)?.ok_or("That stargate does not lead there.")?;
            if i64::from(row.get::<_,i32>(0))!=to.system {return Err("That gate's destination changed.".into());}
            Place {station:None,system:to.system,constellation:i64::from(row.get::<_,i32>(1)),region:i64::from(row.get::<_,i32>(2)),
                at:space::arrival_point(Vec3::new(row.get(3),row.get(4),row.get(5)),row.get(6))}
        }
        Request::Dock {station,..}=>{
            let row=txn.tx().query_opt(r#"SELECT "solarSystemID","constellationID","regionID" FROM "staStations" WHERE "stationID"=$1 FOR SHARE"#,
                &[&(station as i32)]).await.map_err(travel_failed)?.ok_or("That station no longer exists.")?;
            if i64::from(row.get::<_,i32>(0))!=from {return Err("That station is not in this solar system.".into());}
            if !crate::svc::outposts::may_use_locked(txn.tx(),station,2097152,pilot).await.map_err(travel_failed)? {return Err("The station's owners do not allow you to dock here.".into());}
            Place {station:Some(station),system:from,constellation:i64::from(row.get::<_,i32>(1)),region:i64::from(row.get::<_,i32>(2)),at:Vec3::ZERO}
        }
        Request::Admin {to}=>{
            if !to.at.is_finite() {return Err("The relocation position is invalid.".into());}
            let row=txn.tx().query_opt(r#"SELECT "constellationID","regionID" FROM "mapSolarSystems" WHERE "solarSystemID"=$1 FOR SHARE"#,
                &[&(to.system as i32)]).await.map_err(travel_failed)?.ok_or("The destination system does not exist.")?;
            if let Some(station)=to.station {
                if txn.tx().query_opt(r#"SELECT 1 FROM "staStations" WHERE "stationID"=$1 AND "solarSystemID"=$2 FOR SHARE"#,
                    &[&(station as i32),&(to.system as i32)]).await.map_err(travel_failed)?.is_none() {return Err("The station is not in that destination system.".into());}
            }
            Place {constellation:i64::from(row.get::<_,i32>(0)),region:i64::from(row.get::<_,i32>(1)),..to}
        }
    };
    if place.station.is_none() && !server.world.lock().parks.contains_key(&place.system) {return Err("The destination system is not ready.".into());}
    let profile=inflight::profile(txn.tx(),&server.dogma,pilot,ship,&Default::default(),combat::now_ms()).await.map_err(travel_failed)?
        .ok_or("Your ship could not be prepared for arrival.")?;
    let snapshot=if let Some(r)=&reservation {r.current(&server.world.lock(),true)?.clone()}else{
        let row=txn.tx().query_one(r#"SELECT x,y,z FROM entity WHERE "itemID"=$1"#,&[&(ship as i32)]).await.map_err(travel_failed)?;
        let mut loaded=Ship::new(ship,client,profile.stats,profile.pilot.clone(),Vec3::new(row.get(0),row.get(1),row.get(2)),Vec3::ZERO);
        loaded.fight=Some(profile.fighter.clone());loaded
    };
    let source=super::completion::source(txn.tx(),from,source_station,snapshot.pos).await?;
    txn.tx().execute(r#"UPDATE "character_" SET "stationID"=$2,"solarSystemID"=$3,"constellationID"=$4,"regionID"=$5 WHERE "characterID"=$1"#,
        &[&(pilot as i32),&(place.station.unwrap_or(0) as i32),&(place.system as i32),&(place.constellation as i32),&(place.region as i32)]).await.map_err(travel_failed)?;
    txn.tx().execute(r#"UPDATE entity SET "locationID"=$2,flag=$3,x=$4,y=$5,z=$6 WHERE "itemID"=$1"#,
        &[&(ship as i32),&(place.station.unwrap_or(place.system) as i32),&if place.station.is_some(){4i32}else{0},&place.at.x,&place.at.y,&place.at.z]).await.map_err(travel_failed)?;
    txn.tx().execute(r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,
        &[&(pilot as i32),&(place.station.unwrap_or(place.system) as i32)]).await.map_err(travel_failed)?;
    if let Some(f)=&snapshot.fight {inflight::write_vitals(txn.tx(),ship,f,place.station.is_some()).await.map_err(travel_failed)?;}
    crate::cloak::durability::save_history(txn.tx(),&snapshot,combat::now_ms()).await.map_err(travel_failed)?;
    let marker=if let Some(station)=place.station {
        Some(crate::losses::pending::begin(txn.tx(),pilot,ship,station,from).await.map_err(travel_failed)?)
    }else{None};
    let space_marker=if place.station.is_none() {
        Some(crate::travel::begin(txn.tx(),pilot,ship,source,super::completion::destination(place)).await.map_err(travel_failed)?)
    }else{None};
    let _events=txn.prepare_events().await.map_err(travel_failed)?;
    if let Some(r)=&reservation {validate_live(&server.world.lock(),r,request,from,ship)?;}
    if let Some(m)=space_marker {crate::travel::track_uncertain(&server.world,&m)?;}
    if let Some(r)=reservation.as_mut() {r.receipt=space_marker;r.commit_state=CommitState::Uncertain;}
    if let Err(error)=txn.commit_prepared().await {
        tracing::warn!(ship,"transition COMMIT reply failed: {}",crate::logging::chain(&error));
        let accepted=if let Some(m)=space_marker {
            let(fresh,outcome)=super::completion::resolve_space(&server.db_url,&m).await;db=fresh;
            match outcome {
                crate::travel::Resolution::Accepted=>true,
                crate::travel::Resolution::RolledBack=>false,
                crate::travel::Resolution::Finalized=>{
                    if let Some(r)=reservation.as_mut(){r.commit_state=CommitState::Accepted;}
                    return Err(Failure::Pending("This transition already completed; reconnect to the current result.".into()));
                }
                crate::travel::Resolution::Unknown=>unreachable!("resolver retains unknown outcomes"),
            }
        }else{
            let(fresh,accepted)=super::completion::resolve_station(&server.db_url,marker.unwrap(),source).await;db=fresh;accepted
        };
        if !accepted {
            if let Some(r)=reservation.as_mut(){r.commit_state=CommitState::Prepared;}
            return Err("The travel transaction rolled back; the source state was preserved.".into());
        }
    }
    if let Some(r)=reservation.as_mut() {r.commit_state=CommitState::Accepted;}
    let (latest,docking,configuration)={
        let mut w=server.world.lock();
        if let Some(r)=&reservation {r.current(&w,false).map_err(Failure::Pending)?;}
        let docking=match request {Request::Dock{station,..}=>{
            let park=&w.parks[&from];Some((park.ships[&ship].pos,park.ball(station).map(|v|v.0).unwrap_or(snapshot.pos),station))
        },_=>None};
        let configuration=w.maintenance.get(&ship).copied();
        let latest=if reservation.is_some() {space::remove_ship(&mut w,from,ship)
            .ok_or_else(||Failure::Pending("Your committed transition lost its ship and needs reconciliation.".into()))?}else{snapshot};
        (latest,docking,configuration)
    };
    if let Some(marker)=marker {
        let mut latest=latest;
        let lethal=latest.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6);
        if !lethal {inflight::apply_profile(&mut latest,profile,combat::now_ms());}
        let final_state=crate::losses::pending::finish(server,&mut db,marker,latest).await.map_err(Failure::Pending)?;
        drop(reservation);
        return Ok(Arrived {place:Place {station:Some(final_state.station_id),system:final_state.solar_system_id,
            constellation:final_state.constellation_id,region:final_state.region_id,at:Vec3::ZERO},ship_id:final_state.ship_id,
            docking:if final_state.ship_id==ship {docking}else{None}});
    }
    let through=if let Request::Gate{to_gate,..}=request {Some(to_gate)}else{None};
    let arrived=super::completion::finish_space(server,&mut db,space_marker.unwrap(),latest,profile,through).await?;
    if let Some(r)=reservation.as_mut(){r.system=arrived.place.system;}
    if arrived.ship_id==ship && arrived.place.station.is_none() {
        let mut w=server.world.lock();
        if matches!(request,Request::Admin{..}) && from==place.system {
            if let Some(config)=configuration {w.maintenance.insert(ship,config);}
        }
        if let Some(gate)=through {
            let tick=w.parks[&place.system].tick;let flash=space::special_fx(tick,ship,Some(gate),"effects.JumpIn");
            for other in w.in_space(place.system).into_iter().filter(|c|*c!=client) {space::broadcast(&w,place.system,Some(other),vec![flash.clone()]);}
        }
    }
    drop(reservation);
    Ok(arrived)
}

