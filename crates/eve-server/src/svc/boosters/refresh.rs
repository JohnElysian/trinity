use std::collections::HashSet;
use eve_marshal::Value;
use crate::store::{self, Db, Store, StoreError};

fn reserved(world:&crate::world::World,character:i64)->bool {
    world.lock().parks.values().any(|park|park.ships.values().any(|ship|
        ship.is_pilot() && ship.pilot.char_id==character && ship.identity_reserved))
}

fn travelling(db:&Db,world:&crate::world::World,character:i64)->Result<bool,StoreError> {
    // COMMIT changes character_.solarSystemID before the reserved source hull
    // is detached. Looking only in the new system would misclassify the hull
    // as offline and discard the refresh obligation without applying it.
    if crate::travel::blocked_character_in_memory(world,character)
        || crate::conn::logout::blocked_character_in_memory(world,character)
        || crate::svc::jumps::portal::blocked_character_in_memory(world,character) || reserved(world,character) {return Ok(true);}
    Ok(crate::travel::blocked_character(db,character)?
        || crate::conn::logout::blocked_character(db,character)?
        || crate::svc::jumps::portal::blocked_character(db,character)?
        || crate::travel::blocked_character_in_memory(world,character)
        || crate::conn::logout::blocked_character_in_memory(world,character)
        || crate::svc::jumps::portal::blocked_character_in_memory(world,character) || reserved(world,character))
}

/// The caller holds this character's inventory guard. A failed or deferred
/// refresh leaves the durable marker; accepted consumption is never undone.
pub(super) async fn character(store:&Store,catalog:&crate::dogma::Catalog,world:&crate::world::World,character:i64)->Result<(),StoreError> {
    let read=store.read(|db|->Result<_,StoreError> {
        if travelling(db,world,character)? {return Ok(None);}
        let Some(row)=db.table("character_").get_id(character) else {return Ok(None);};
        let ship_id=row.int("shipID").filter(|id|*id>0);
        let station=row.int("stationID").filter(|id|*id>0);
        let system=row.int("solarSystemID").unwrap_or(0);
        let live={
            let w=world.lock();
            if station.is_none() {
                w.parks.get(&system).and_then(|park|ship_id.and_then(|id|park.ships.get(&id)))
                    .filter(|ship|ship.is_pilot() && ship.pilot.char_id==character)
                    .map(|ship|(ship.client_id,ship.identity_reserved,ship.fight.as_ref().map(|fight|fight.applied.clone()).unwrap_or_default()))
            } else {None}
        };
        if live.as_ref().is_some_and(|(_,reserved,_)|*reserved) {return Ok(None);}
        let active=live.as_ref().map(|(_,_,active)|active.iter().copied().collect()).unwrap_or_else(HashSet::new);
        let snapshot=crate::dogma::Loadout::load(db,catalog,character,ship_id,&active)?.snapshot();
        let profile=if let (Some(ship),Some((_,_,active)))=(ship_id,&live) {
            crate::svc::inflight::profile(db,catalog,character,ship,active,crate::combat::now_ms())?
        } else {None};
        if travelling(db,world,character)? {return Ok(None);}
        Ok(Some((ship_id,station,system,live,snapshot,profile)))
    })?;
    let Some((ship_id,station,system,live,snapshot,profile))=read else {return Ok(());};
    {
        let mut w=world.lock();
        let mut capacitor=None;
        if let Some((client,_,expected))=&live {
            let Some(park)=w.parks.get_mut(&system) else {return Ok(());};
            let tick=park.tick;
            let Some(ship)=ship_id.and_then(|id|park.ships.get_mut(&id)) else {return Ok(());};
            if !ship.is_pilot() || ship.pilot.char_id!=character || ship.client_id!=*client || ship.identity_reserved
                || ship.fight.as_ref().map(|f|&f.applied)!=Some(expected) {return Ok(());}
            let Some(profile)=profile else {return Ok(());};
            let old_speed=ship.stats.max_velocity;
            let old_cap=ship.fight.as_ref().map(|f|f.vitals.capacitor.max);
            crate::svc::inflight::apply_profile(ship,profile,crate::combat::now_ms());
            if let Some(fight)=ship.fight.as_ref() {
                if old_cap!=Some(fight.vitals.capacitor.max) {capacitor=Some(fight.vitals.capacitor.at(crate::combat::now_ms()));}
            }
            if (old_speed-ship.stats.max_velocity).abs()>1e-9 {
                let speed=ship.stats.max_velocity;
                crate::svc::space::broadcast(&w,system,None,vec![crate::svc::space::entry(tick,"SetMaxSpeed",vec![Value::Int(ship_id.unwrap()),Value::Double(speed)])]);
            }
        }
        // Only notify a client still in the snapshotted location. Reconnecting
        // clients load the current attributes through CharGetInfo/ShipGetInfo.
        let clients=w.clients_of(character).into_iter().filter(|id|w.presence_of_client(*id).is_some_and(|p|p.solar_system_id==system && p.station_id==station)).collect::<Vec<_>>();
        let now=crate::bulkdata::filetime_now();
        // These supported boosters modify only these five attributes. Sending
        // unrelated persisted charge values would overwrite the client's live
        // capacitor/shield state after damage or regeneration since the snapshot.
        let mut changes=snapshot.into_iter().flat_map(|(item,attributes)|attributes.into_iter()
            .filter(|(attribute,_)|matches!(*attribute,37|54|158|482|654))
            .map(move |(attribute,value)|Value::Tuple(vec![
            Value::bytes("OnModuleAttributeChange"),Value::Int(character),Value::Int(item),Value::Int(i64::from(attribute)),
            Value::Long(now),if attribute==super::EXPIRY {Value::Long(value as i64)} else {Value::Double(value)},Value::None,
        ]))).collect::<Vec<_>>();
        // Capacity changes rescale the server's live charge. The client must
        // receive that current charge after the new maximum, never the older
        // persisted charge read while constructing the profile.
        if let Some(charge)=capacitor {changes.push(Value::Tuple(vec![Value::bytes("OnModuleAttributeChange"),Value::Int(character),Value::Int(ship_id.unwrap()),Value::Int(18),Value::Long(now),Value::Double(charge),Value::None]));}
        w.notify(&clients,"OnModuleAttributeChanges",Value::Tuple(vec![Value::List(changes)]));
    }
    store.write(|tx|tx.delete_where("character_booster_refresh",&["character"],&[store::Value::Int(character)]))?;
    Ok(())
}
