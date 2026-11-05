use super::*;
use super::super::fixture::{self, exists, int, kills};

struct Fixture {server:Server,ship:Ship,station:i64,system:i64,cargo:i64,spare:i64}
impl Fixture {
    async fn new(capsule:bool)->Self {
        let f=crate::test_support::Database::load().await;
        let store=f.store.clone();
        let (pilot,hull,cargo,spare)=(fixture::id(&store),fixture::id(&store),fixture::id(&store),fixture::id(&store));
        let (station,system,constellation,region)=fixture::station(&store);
        let kind=if capsule{670}else{587};
        store.write(|tx| {
            fixture::character(tx,pilot,"Arrival loss",1000167,(hull,station,system,constellation,region),1000.0)?;
            for (id,ty,location,flag) in [(pilot,1373,station,57),(hull,kind,station,4),(cargo,34,hull,5),(spare,587,station,4)] {
                fixture::item(tx,(id,ty,pilot,location,flag,1),true,(0.0,0.0,0.0))?;
            }
            Ok::<_,StoreError>(())
        }).unwrap();
        let profile=store.read(|db| inflight::profile(db,&f.dogma,pilot,hull,&BTreeSet::new(),combat::now_ms())).unwrap().unwrap();
        let mut ship=Ship::new(hull,1,profile.stats,profile.pilot,Vec3::new(1000.0,2000.0,3000.0),Vec3::ZERO);
        ship.fight=Some(profile.fighter);
        let fight=ship.fight.as_mut().unwrap();fight.vitals.hull_damage=fight.vitals.hull_max;
        Self {server:fixture::server(f),ship,station,system,cargo,spare}
    }
    fn insured(&self) {
        let now=crate::bulkdata::filetime_now();
        self.server.store.write(|tx| fixture::insure(tx,self.ship.id,self.ship.id,self.ship.pilot.char_id,now-1,now+600_000_000)).unwrap();
    }
    fn policy(&self)->bool {
        self.server.store.read(|db| db.table("insuranceContracts").find_one(&["shipID"],&[int(self.ship.id)]).is_some())
    }
}

#[tokio::test]
async fn docked_loss_rolls_back_on_kill_record_failure_and_retries_without_duplicate_insurance() {
    let f=Fixture::new(false).await;f.insured();
    let store=&*f.server.store;let pilot=f.ship.pilot.char_id;
    fixture::refuse_next_kill(store);
    let failed=settle_docked_arrival(&f.server,store,f.station,f.system,&f.ship).await;
    assert!(failed.is_err());
    assert_eq!(fixture::character_int(store,pilot,"shipID"),Some(f.ship.id));assert_eq!(fixture::character_float(store,pilot,"balance"),Some(1000.0));
    for id in [f.ship.id,f.cargo,f.spare] {assert!(exists(store,id));}
    assert!(f.policy());
    let settled=settle_docked_arrival(&f.server,store,f.station,f.system,&f.ship).await.unwrap();
    assert_eq!(settled.station_id,f.station);assert_eq!(settled.solar_system_id,f.system);assert_ne!(settled.ship_id,f.ship.id);
    assert_eq!(settle_docked_arrival(&f.server,store,f.station,f.system,&f.ship).await.unwrap(),settled);
    assert_eq!(fixture::character_int(store,pilot,"shipID"),Some(settled.ship_id));assert_eq!(fixture::character_float(store,pilot,"balance"),Some(1500.0));
    assert_eq!(kills(store,f.ship.id),1);
    assert!(!exists(store,f.ship.id));
    assert!(exists(store,f.spare));
    let pod=["typeID","locationID","flag"].map(|c| fixture::entity_int(store,settled.ship_id,c));
    assert_eq!(pod,[Some(670),Some(f.station),Some(4)]);
}

#[tokio::test]
async fn docked_capsule_loss_uses_medical_home_removes_implants_and_does_not_publish_a_move() {
    let f=Fixture::new(true).await;
    let store=&*f.server.store;let pilot=f.ship.pilot.char_id;
    let home=store.read(|db| db.table("staStations").rows().filter(|s| s.int("solarSystemID")!=Some(f.system)).filter_map(|s| s.int("stationID")).min()).unwrap();
    store.write(|tx| medical::replace_clone(tx,pilot,TYPE_CLONE_ALPHA,Some(home))).unwrap();
    let implant=fixture::id(store);
    let implant_type=i64::from(crate::sde::get().types_in_group(300)[0]);
    store.write(|tx| fixture::item(tx,(implant,implant_type,pilot,pilot,89,1),true,(0.0,0.0,0.0))).unwrap();
    let mut rx=f.server.world.lock().connect(1);
    let settled=settle_docked_arrival(&f.server,store,f.station,f.system,&f.ship).await.unwrap();
    assert_eq!(settled.station_id,home);assert_ne!(settled.solar_system_id,f.system);
    for id in [implant,f.ship.id,f.cargo] {assert!(!exists(store,id));}
    let pod_kill=store.read(|db| db.table("killLog").rows().find(|k| k.int("victimItemID")==Some(f.ship.id)).and_then(|k| k.int("isPodKill")));
    assert_eq!(pod_kill,Some(1));
    while let Ok(event)=rx.try_recv() {assert!(!matches!(event,crate::world::Push::Move(_)));}
    assert_eq!(settle_docked_arrival(&f.server,store,f.station,f.system,&f.ship).await.unwrap(),settled);
}

#[tokio::test]
async fn docked_settlement_rejects_alive_stale_or_changed_custody_without_touching_items() {
    let f=Fixture::new(false).await;
    let store=&*f.server.store;
    let mut alive=f.ship.clone();alive.fight.as_mut().unwrap().vitals.hull_damage=0.0;
    assert!(settle_docked_arrival(&f.server,store,f.station,f.system,&alive).await.is_err());
    fixture::set(store,"character_",f.ship.pilot.char_id,&[("shipID",int(f.spare))]);
    assert!(settle_docked_arrival(&f.server,store,f.station,f.system,&f.ship).await.is_err());
    assert!([f.ship.id,f.cargo,f.spare].iter().all(|id| exists(store,*id)));
    assert_eq!(kills(store,f.ship.id),0);
}

#[tokio::test]
async fn completed_docked_loss_retry_accepts_newer_boarding_and_undock_without_repaying() {
    let f=Fixture::new(false).await;f.insured();
    let store=&*f.server.store;let pilot=f.ship.pilot.char_id;
    let marker=store.write(|tx| crate::losses::pending::begin(tx,pilot,f.ship.id,f.station,f.system)).unwrap();
    let mut rx=f.server.world.lock().connect(1);
    let settled=crate::losses::pending::finish(&f.server,store,marker,f.ship.clone()).await.unwrap();
    fixture::set(store,"character_",pilot,&[("shipID",int(f.spare))]);
    let boarded=crate::losses::pending::finish(&f.server,store,marker,f.ship.clone()).await.unwrap();
    assert_eq!(boarded.ship_id,f.spare);assert_eq!(boarded.station_id,f.station);
    assert!(f.server.world.lock().pending_station_arrivals.is_empty());
    store.write(|tx| {
        update_id(tx,"character_",pilot,&[("stationID",int(0))])?;
        update_id(tx,"entity",f.spare,&[("locationID",int(f.system)),("flag",int(0))])
    }).unwrap();
    let undocked=crate::losses::pending::finish(&f.server,store,marker,f.ship.clone()).await.unwrap();
    assert_eq!(undocked.ship_id,f.spare);assert_eq!(undocked.station_id,0);assert_eq!(undocked.solar_system_id,f.system);
    assert!(f.server.world.lock().pending_station_arrivals.is_empty());
    fixture::set(store,"entity",f.spare,&[("ownerID",int(1))]);
    assert!(crate::losses::pending::finish(&f.server,store,marker,f.ship.clone()).await.is_err(),"newer active hull still needs valid custody");
    assert!(f.server.world.lock().pending_station_arrivals.contains_key(&f.ship.id));
    fixture::set(store,"entity",f.spare,&[("ownerID",int(pilot))]);
    crate::losses::pending::retry(&f.server,store).await;
    assert!(f.server.world.lock().pending_station_arrivals.is_empty());
    while let Ok(event)=rx.try_recv() {assert!(!matches!(event,crate::world::Push::Move(_)),"retry cannot relocate a newer session");}
    assert_eq!(fixture::character_float(store,pilot,"balance"),Some(1500.0));
    assert_eq!(kills(store,f.ship.id),1);
    assert_eq!(fixture::entity_int(store,settled.ship_id,"typeID"),Some(670));
}
