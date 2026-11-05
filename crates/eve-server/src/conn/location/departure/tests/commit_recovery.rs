use super::*;

/// Keep the fixture's capacitor capacity/recharge identical through the
/// arrival profile, making a mistaken refund or duplicate debit observable.
fn steady_capacitor(f:&Fixture) {
    f.db.store.write(|tx| {
        for (attribute,value) in [(482,1000.0),(55,0.0)] {
            tx.upsert("entity_attributes",&["itemID","attributeID"],
                &[("itemID",int(f.ship.id)),("attributeID",int(attribute)),("valueFloat",StoreValue::Float(value))],
                &[("valueFloat",StoreValue::Float(value)),("valueInt",StoreValue::Null)])?;
        }
        Ok::<_,crate::store::StoreError>(())
    }).unwrap();
}
fn quantity(f:&Fixture)->Option<i64> {f.saved().2}
fn request(f:&Fixture)->JumpRequest {JumpRequest::Beacon {member:f.member,beacon:f.beacon}}

#[tokio::test]
async fn committed_jump_keeps_latest_combat_pays_once_and_refuses_a_repeat() {
    let f=Fixture::new().await;steady_capacitor(&f);
    let server=f.server().await;let arrival=f.arrival();let quote=f.quote();
    {let mut w=f.db.world.lock();let ship=w.parks.get_mut(&arrival.from).unwrap().ships.get_mut(&f.ship.id).unwrap();
        let fight=ship.fight.as_mut().unwrap();fight.vitals.hull_damage=19.0;assert!(fight.vitals.capacitor.spend(crate::combat::now_ms(),50.0));}
    let arrived=super::super::reserved::run(&server,&f.session,1,request(&f),&arrival,&quote,"effects.JumpDriveIn").await.unwrap();
    assert_eq!(arrived.ship_id,f.ship.id);assert_eq!(arrived.place.system,arrival.system);assert_eq!(quantity(&f),Some(5));
    {let w=f.db.world.lock();assert!(!w.parks[&arrival.from].ships.contains_key(&f.ship.id));
        let ship=&w.parks[&arrival.system].ships[&f.ship.id];assert!(!ship.identity_reserved);
        let fight=ship.fight.as_ref().unwrap();assert_eq!(fight.vitals.hull_damage,19.0);assert_eq!(fight.vitals.capacitor.at(crate::combat::now_ms()),850.0);}
    let (damage,capacitor,receipts)=f.db.store.read(|db| {
        let attribute=|id:i64|db.table("entity_attributes").get(&[int(f.ship.id),int(id)]).and_then(|a|a.float("valueFloat"));
        let receipts=db.table("travelReceipts").rows().filter(|r|r.int("shipID")==Some(f.ship.id) && r.int("state")==Some(1)).count();
        (attribute(3),attribute(18),receipts)
    });
    assert_eq!(damage,Some(19.0));assert_eq!(capacitor,Some(850.0));assert_eq!(receipts,1);
    assert!(!f.db.store.read(|db|crate::losses::pending::blocked(db,f.ship.id)).unwrap());
    assert!(!crate::travel::blocked_in_memory(&server.world,f.ship.id));
    // A delayed callback with the old source session cannot pay again.
    assert!(super::super::reserved::run(&server,&f.session,1,request(&f),&arrival,&quote,"effects.JumpDriveIn").await.is_err());
    assert_eq!(quantity(&f),Some(5));
}

#[tokio::test]
async fn station_arrival_saves_final_combat_state_and_cannot_be_replayed() {
    let f=Fixture::new().await;
    let (station,constellation,region)=f.db.store.read(|db| {
        db.table("staStations").rows().filter(|s|s.int("solarSystemID")==Some(30000142))
            .filter_map(|s|Some((s.int("stationID")?,s.int("constellationID").unwrap_or(0),s.int("regionID").unwrap_or(0))))
            .min().unwrap()
    });
    let to=super::super::transition::Place {station:Some(station),system:30000142,constellation,region,at:Vec3::ZERO};
    let server=f.server().await;
    let mut session=f.session.clone();session.role=8;
    f.db.world.lock().parks.get_mut(&30000142).unwrap().ships.get_mut(&f.ship.id).unwrap()
        .fight.as_mut().unwrap().vitals.hull_damage=23.0;
    let result=super::super::transition::run(&server,&session,1,super::super::transition::Request::Admin {to}).await.unwrap();
    assert_eq!(result.place.station,Some(station));assert_eq!(result.place.system,30000142);assert_eq!(result.ship_id,f.ship.id);
    let saved=f.db.store.read(|db| {
        let c=db.table("character_").get_id(f.ship.pilot.char_id).unwrap();
        let e=db.table("entity").get_id(c.int("shipID").unwrap()).unwrap();
        let damage=db.table("entity_attributes").get(&[int(e.int("itemID").unwrap()),int(3)]).and_then(|a|a.float("valueFloat"));
        (c.int("stationID"),e.int("locationID"),e.int("flag"),damage)
    });
    assert_eq!(saved,(Some(station),Some(station),Some(4),Some(23.0)));
    assert!(!f.db.world.lock().parks[&30000142].ships.contains_key(&f.ship.id));
    assert!(f.db.world.lock().pending_station_arrivals.is_empty());
    assert!(!f.db.store.read(|db|crate::losses::pending::blocked_character(db,f.ship.pilot.char_id)).unwrap());
    assert_eq!(quantity(&f),Some(8),"station arrival does not consume jump fuel");
    assert!(super::super::transition::run(&server,&session,1,super::super::transition::Request::Admin {to}).await.is_err(),
        "a delayed callback with the old space session cannot repeat the station move");
    assert_eq!(quantity(&f),Some(8));
}
