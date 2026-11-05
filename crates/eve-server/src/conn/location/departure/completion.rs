//! Finish a committed move without replaying payment or guessing a refund.
use super::*;
use crate::{conn::Server,travel,losses,combat};
use super::transition::{Arrived,Failure,Place};

pub(super) fn destination(place:Place)->travel::Place {
    travel::Place {station:place.station,system:place.system,constellation:place.constellation,region:place.region,at:place.at}
}

pub(super) fn source(db:&Db,system:i64,station:Option<i64>,at:Vec3)->std::result::Result<travel::Place,String> {
    let row=db.table("mapSolarSystems").get_id(system).ok_or_else(unsaved)?;
    Ok(travel::Place {system,station,at,constellation:row.int("constellationID").unwrap_or(0),region:row.int("regionID").unwrap_or(0)})
}

/// The accepted source capture is detached until its final checkpoint is
/// saved, so the live hull cannot take newer damage than that snapshot.
pub(super) async fn finish_space(server:&Server,marker:travel::Marker,
    mut ship:Ship,profile:inflight::Profile,through:Option<i64>)->std::result::Result<Arrived,Failure> {
    let lethal=ship.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6);
    if !lethal {inflight::apply_profile(&mut ship,profile,combat::now_ms());}
    let place=Place {station:None,system:marker.destination.system,constellation:marker.destination.constellation,
        region:marker.destination.region,at:marker.destination.at};
    if lethal || ship.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {
        let mut loss=losses::Loss::of(&ship,marker.source.system,ship.pilot.type_id==670);
        loss.arrival_custody=Some(losses::SpaceArrivalCustody {system:place.system,at:place.at});
        travel::finalize(server,&server.store,marker,travel::Final::Lost(loss.clone())).await.map_err(Failure::Pending)?;
        losses::journal::handle(server,&server.store,loss).await;
        if server.store.read(|db|losses::pending::blocked_character(db,marker.pilot)).map_err(|e|Failure::Pending(travel_failed(e)))? {
            return Err(Failure::Pending("The accepted arrival's hull loss is awaiting settlement.".into()));
        }
        return current_arrival(&server.store,marker.pilot);
    }
    ship.arrive(place.at);ship.arrived_through=through;ship.identity_reserved=false;
    travel::finalize(server,&server.store,marker,travel::Final::Healthy {ship:ship.clone(),tick:destiny::tick_now()}).await.map_err(Failure::Pending)?;
    server.store.read(|db| {
        let at_place=db.table("character_").get_id(marker.pilot).is_some_and(|c|c.int("shipID")==Some(marker.ship)
            && c.int("solarSystemID")==Some(place.system) && c.int("stationID").unwrap_or(0)==0)
            && db.table("entity").get_id(marker.ship).is_some_and(|e|e.int("ownerID")==Some(marker.pilot) && e.int("typeID")==Some(marker.ship_type)
                && e.int("locationID")==Some(place.system) && e.int("flag")==Some(0) && e.int("quantity")==Some(1) && e.int("singleton").is_some_and(|s|s!=0));
        if !at_place {
            return Err(Failure::Pending("The completed arrival was superseded; reconnect to the current ship.".into()));
        }
        // A later same-system trip can retain the same hull/custody. Its receipt,
        // or any later motion checkpoint, still supersedes this captured object.
        let receipts=db.table("travelReceipts");
        let newer=receipts.rows().any(|r|r.int("generation").is_some_and(|g|g>marker.generation)
            && (r.int("shipID")==Some(marker.ship) || r.int("characterID")==Some(marker.pilot)));
        // The receipt's completion wrote the arrival checkpoint just before
        // it: a checkpoint saved after the completion is a later one.
        let completed=receipts.get_id(marker.generation).filter(|r|r.int("state")==Some(1)).and_then(|r|r.int("completedAt"));
        let checkpoint=completed.is_some_and(|done|db.table("shipSpaceMotion").get_id(marker.ship)
            .is_some_and(|m|m.int("solarSystemID")==Some(place.system) && m.int("savedAt").is_some_and(|saved|saved<=done)));
        if newer || !checkpoint || losses::pending::blocked(db,marker.ship).map_err(|e|Failure::Pending(travel_failed(e)))? {
            return Err(Failure::Pending("The arrival snapshot has been superseded; reconnect to the current state.".into()));
        }
        // Expiry/skill changes can finish while the accepted ship is detached.
        // Re-read current modifiers instead of publishing a profile cached
        // before the move.
        let current_profile=inflight::profile(db,&server.dogma,marker.pilot,marker.ship,&Default::default(),combat::now_ms())
            .map_err(|e|Failure::Pending(travel_failed(e)))?.ok_or_else(||Failure::Pending("The current arrival profile is unavailable.".into()))?;
        inflight::apply_profile(&mut ship,current_profile,combat::now_ms());
        let mut world=server.world.lock();
        if !world.parks.get(&place.system).is_some_and(|p|p.ships.contains_key(&marker.ship)) {
            space::add_ship(&mut world,place.system,ship);
        }
        Ok(())
    })?;
    Ok(Arrived {place,ship_id:marker.ship,docking:None})
}

fn current_arrival(store:&Store,pilot:i64)->std::result::Result<Arrived,Failure> {
    store.read(|db| {
        let c=db.table("character_").get_id(pilot).ok_or_else(||Failure::Pending(unsaved()))?;
        let id=|column:&str|c.int(column).unwrap_or(0);
        Ok(Arrived {ship_id:id("shipID"),place:Place {station:Some(id("stationID")).filter(|id|*id!=0),
            system:id("solarSystemID"),constellation:id("constellationID"),region:id("regionID"),at:Vec3::ZERO},docking:None})
    })
}
