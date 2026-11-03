//! Resolve COMMIT outcomes without replaying payment or guessing a refund.
use super::*;
use crate::{conn::Server,travel,losses,combat};
use super::transition::{Arrived,Failure,Place};

pub(super) fn destination(place:Place)->travel::Place {
    travel::Place {station:place.station,system:place.system,constellation:place.constellation,region:place.region,at:place.at}
}

pub(super) async fn source(tx:&tokio_postgres::Transaction<'_>,system:i64,station:Option<i64>,at:Vec3)->std::result::Result<travel::Place,String> {
    let row=tx.query_one(r#"SELECT "constellationID","regionID" FROM "mapSolarSystems" WHERE "solarSystemID"=$1"#,
        &[&(system as i32)]).await.map_err(travel_failed)?;
    Ok(travel::Place {system,station,at,constellation:i64::from(row.get::<_,i32>(0)),region:i64::from(row.get::<_,i32>(1))})
}

/// Fresh locks wait for the original transaction's definitive outcome. A
/// transport failure never resends the mutation or releases its reservation.
pub(super) async fn resolve_space(url:&str,marker:&travel::Marker)->(tokio_postgres::Client,travel::Resolution) {
    loop {
        if let Ok(Ok(mut fresh))=tokio::time::timeout(Duration::from_secs(10),crate::db::connect(url)).await {
            match tokio::time::timeout(Duration::from_secs(10),travel::resolve(&mut fresh,marker)).await {
                Ok(Ok(state @ (travel::Resolution::Accepted|travel::Resolution::Finalized|travel::Resolution::RolledBack)))=>return(fresh,state),
                _=>{},
            }
        }
        tracing::warn!(generation=marker.generation,"travel COMMIT outcome remains unresolved; retaining reservation");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

pub(super) async fn resolve_station(url:&str,marker:losses::pending::Marker,source:travel::Place)->(tokio_postgres::Client,bool) {
    loop {
        if let Ok(Ok(mut fresh))=tokio::time::timeout(Duration::from_secs(10),crate::db::connect(url)).await {
            let outcome=tokio::time::timeout(Duration::from_secs(10),async {
                let tx=fresh.transaction().await?;
                if !crate::svc::login::lock_character_lifecycle(&tx,marker.pilot as i32).await? {return Ok(None);}
                let pilot=tx.query_opt(r#"SELECT "shipID","solarSystemID",COALESCE("stationID",0) FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#,&[&(marker.pilot as i32)]).await?;
                let hull=tx.query_opt(r#"SELECT "ownerID","typeID","locationID",flag,quantity,singleton::integer FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(marker.ship as i32)]).await?;
                let pending=tx.query_opt(r#"SELECT generation,"characterID","stationID","sourceSystemID","shipTypeID" FROM "pendingStationArrivals" WHERE "shipID"=$1 FOR UPDATE"#,&[&(marker.ship as i32)]).await?;
                if let Some(row)=pending {
                    return Ok(Some(row.get::<_,i64>(0)==marker.generation && row.get::<_,i32>(1)==marker.pilot as i32
                        && row.get::<_,i32>(2)==marker.station as i32 && row.get::<_,i32>(3)==marker.source as i32 && row.get::<_,i32>(4)==marker.ship_type as i32).filter(|v|*v));
                }
                let at_source=pilot.is_some_and(|r|r.get::<_,Option<i32>>(0)==Some(marker.ship as i32)
                    && r.get::<_,Option<i32>>(1)==Some(source.system as i32) && r.get::<_,i32>(2)==source.station.unwrap_or(0) as i32)
                    && hull.is_some_and(|r|r.get::<_,Option<i32>>(0)==Some(marker.pilot as i32)
                        && r.get::<_,Option<i32>>(1)==Some(marker.ship_type as i32) && r.get::<_,Option<i32>>(2)==Some(source.station.unwrap_or(source.system) as i32)
                        && r.get::<_,Option<i32>>(3)==Some(if source.station.is_some(){4}else{0}) && r.get::<_,Option<i32>>(4)==Some(1) && r.get::<_,Option<i32>>(5).is_some_and(|n|n!=0));
                Ok::<_,tokio_postgres::Error>(at_source.then_some(false))
            }).await;
            if let Ok(Ok(Some(accepted)))=outcome {return(fresh,accepted);}
        }
        tracing::warn!(ship=marker.ship,"station COMMIT outcome remains unresolved; retaining reservation");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Accepted source capture is detached until its final checkpoint is durable.
/// A failed write remains retryable without exposing a live hull that could
/// take newer damage than the immutable pending snapshot.
pub(super) async fn finish_space(server:&Server,db:&mut tokio_postgres::Client,marker:travel::Marker,
    mut ship:Ship,profile:inflight::Profile,through:Option<i64>)->std::result::Result<Arrived,Failure> {
    let lethal=ship.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6);
    if !lethal {inflight::apply_profile(&mut ship,profile,combat::now_ms());}
    let place=Place {station:None,system:marker.destination.system,constellation:marker.destination.constellation,
        region:marker.destination.region,at:marker.destination.at};
    if lethal || ship.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {
        let mut loss=losses::Loss::of(&ship,marker.source.system,ship.pilot.type_id==670);
        loss.arrival_custody=Some(losses::SpaceArrivalCustody {system:place.system,at:place.at});
        travel::finalize(server,db,marker,travel::Final::Lost(loss.clone())).await.map_err(Failure::Pending)?;
        losses::journal::handle(server,db,loss).await;
        if losses::pending::blocked_character(db,marker.pilot).await.map_err(|e|Failure::Pending(travel_failed(e)))? {
            return Err(Failure::Pending("The accepted arrival's hull loss is awaiting settlement.".into()));
        }
        return current_arrival(db,marker.pilot).await;
    }
    ship.arrive(place.at);ship.arrived_through=through;ship.identity_reserved=false;
    travel::finalize(server,db,marker,travel::Final::Healthy {ship:ship.clone(),tick:destiny::tick_now()}).await.map_err(Failure::Pending)?;
    let tx=db.transaction().await.map_err(|e|Failure::Pending(travel_failed(e)))?;
    if !crate::svc::login::lock_character_lifecycle(&tx,marker.pilot as i32).await.map_err(|e|Failure::Pending(travel_failed(e)))?
        || tx.query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2 AND "solarSystemID"=$3 AND COALESCE("stationID",0)=0 FOR UPDATE"#,
            &[&(marker.pilot as i32),&(marker.ship as i32),&(place.system as i32)]).await.map_err(|e|Failure::Pending(travel_failed(e)))?.is_none()
        || tx.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1 AND "ownerID"=$2 AND "typeID"=$3 AND "locationID"=$4 AND flag=0 AND quantity=1 AND singleton<>0 FOR UPDATE"#,
            &[&(marker.ship as i32),&(marker.pilot as i32),&(marker.ship_type as i32),&(place.system as i32)]).await.map_err(|e|Failure::Pending(travel_failed(e)))?.is_none() {
        return Err(Failure::Pending("The completed arrival was superseded; reconnect to the current ship.".into()));
    }
    // A later same-system trip can retain the same hull/custody. Its receipt,
    // or any later motion checkpoint, still supersedes this captured object.
    let still_current=tx.query_one(r#"SELECT NOT EXISTS(SELECT 1 FROM "travelReceipts"
        WHERE generation>$1 AND ("shipID"=$2 OR "characterID"=$3))
        AND EXISTS(SELECT 1 FROM "shipSpaceMotion" m JOIN "travelReceipts" r ON r.generation=$1
            WHERE m."itemID"=$2 AND r.state=1 AND m.xmin=r.xmin)"#,
        &[&marker.generation,&(marker.ship as i32),&(marker.pilot as i32)]).await.map_err(|e|Failure::Pending(travel_failed(e)))?.get::<_,bool>(0);
    if !still_current || losses::pending::blocked(&tx,marker.ship).await.map_err(|e|Failure::Pending(travel_failed(e)))? {
        return Err(Failure::Pending("The arrival snapshot has been superseded; reconnect to the current state.".into()));
    }
    // Expiry/skill changes can finish while the accepted ship is detached.
    // Re-read current modifiers under custody locks instead of publishing a
    // profile cached before the first COMMIT wait.
    let current_profile=inflight::profile(&tx,&server.dogma,marker.pilot,marker.ship,&Default::default(),combat::now_ms()).await
        .map_err(|e|Failure::Pending(travel_failed(e)))?.ok_or_else(||Failure::Pending("The current arrival profile is unavailable.".into()))?;
    inflight::apply_profile(&mut ship,current_profile,combat::now_ms());
    {
        let mut world=server.world.lock();
        if !world.parks.get(&place.system).is_some_and(|p|p.ships.contains_key(&marker.ship)) {
            space::add_ship(&mut world,place.system,ship);
        }
    }
    // Only read locks were acquired after finalization; publication has not
    // performed another database mutation whose acknowledgement could be lost.
    drop(tx);
    Ok(Arrived {place,ship_id:marker.ship,docking:None})
}

async fn current_arrival(db:&tokio_postgres::Client,pilot:i64)->std::result::Result<Arrived,Failure> {
    let row=db.query_one(r#"SELECT "shipID",COALESCE("stationID",0),"solarSystemID","constellationID","regionID" FROM "character_" WHERE "characterID"=$1"#,&[&(pilot as i32)]).await.map_err(|e|Failure::Pending(travel_failed(e)))?;
    Ok(Arrived {ship_id:i64::from(row.get::<_,i32>(0)),place:Place {station:Some(i64::from(row.get::<_,i32>(1))).filter(|id|*id!=0),
        system:i64::from(row.get::<_,i32>(2)),constellation:i64::from(row.get::<_,i32>(3)),region:i64::from(row.get::<_,i32>(4)),at:Vec3::ZERO},docking:None})
}
