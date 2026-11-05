use super::*;
use super::super::fixture::{self, exists, int, kills};
use crate::{session::Session,test_support::Database,world::{Presence,Push}};

#[test]
fn stale_finalization_guard_cannot_release_a_newer_attempt() {
    let marker=Marker {ship:1,pilot:2,station:60014659,source:30004971,generation:7,ship_type:587};
    let ship=Ship::new(1,1,crate::ballpark::ShipStats::new(10.0,1e6,1.0,100.0,1.0),crate::ballpark::Pilot {
        type_id:587,group_id:25,name:"Guard".into(),char_id:2,corp_id:1,alliance_id:None,security:0.0,bounty:0.0},Vec3::ZERO,Vec3::ZERO);
    let world=crate::world::World::default();let old=next_claim();let new=next_claim();
    let stale=Busy {world:world.clone(),marker,claim:old};
    world.lock().pending_station_arrivals.insert(1,Pending {marker,ship,busy:true,claim:new});
    drop(stale);assert!(world.lock().pending_station_arrivals[&1].busy);
    drop(Busy {world:world.clone(),marker,claim:new});assert!(!world.lock().pending_station_arrivals[&1].busy);
}

struct Fixture {server:Server,ship:Ship,marker:Marker,cargo:i64}
impl Fixture {
    async fn new()->Self {
        let server=fixture::server(Database::load().await);
        let (ship,marker,cargo)=docked_pilot(&server).await;
        Self {server,ship,marker,cargo}
    }
    fn store(&self)->&Store {&self.server.store}
    fn pilot(&self)->i64 {self.ship.pilot.char_id}
    fn blocked(&self) {
        assert!(self.store().read(|db| blocked(db,self.ship.id)).unwrap());
        assert!(self.store().read(|db| blocked_character(db,self.pilot())).unwrap());
    }
    /// The loss recorded with the arrival's marker.
    fn recorded(&self)->Option<String> {
        self.store().read(|db| db.table("pendingStationArrivals").get_id(self.ship.id).and_then(|p| p.text("loss").map(str::to_string)))
    }
}

/// A pilot just docked at the fixture station in a fresh hull with cargo,
/// its arrival marked: the ship as last seen in space, the marker, the cargo.
async fn docked_pilot(server:&Server)->(Ship,Marker,i64) {
    let store=&*server.store;
    let (pilot,hull,cargo)=(fixture::id(store),fixture::id(store),fixture::id(store));
    let (station,system,constellation,region)=fixture::station(store);
    store.write(|tx| {
        fixture::character(tx,pilot,"Pending arrival",1000167,(hull,station,system,constellation,region),1000.0)?;
        fixture::item(tx,(hull,587,pilot,station,4,1),true,(0.0,0.0,0.0))?;
        fixture::item(tx,(pilot,1373,pilot,station,57,1),true,(0.0,0.0,0.0))?;
        fixture::item(tx,(cargo,34,pilot,hull,5,7),false,(0.0,0.0,0.0))
    }).unwrap();
    let session=Session {char_id:Some(pilot),ship_id:Some(hull),station_id:Some(station),solar_system_id2:Some(system),..Default::default()};
    let ctx=crate::services::Ctx {bulk:&server.bulk,dogma:&server.dogma,session:&session,store:&server.store,node_id:1,world:&server.world,client_id:1};
    let mut ship=inflight::ship_for_space(&ctx,pilot,hull).await.unwrap();
    ship.pos=Vec3::new(111.0,222.0,333.0);ship.fight.as_mut().unwrap().vitals.hull_damage=17.0;
    let marker=store.write(|tx| begin(tx,pilot,hull,station,system)).unwrap();
    (ship,marker,cargo)
}

#[tokio::test]
async fn quarantined_station_page_does_not_starve_later_confirmed_arrivals() {
    let bad=Fixture::new().await;let store=bad.store();
    store.write(|tx| {
        for _ in 0..64 {
            let hull=tx.nextval("dyn_id_seq")?;
            fixture::item(tx,(hull,587,bad.pilot(),bad.marker.station,4,1),true,(0.0,0.0,0.0))?;
            tx.insert("pendingStationArrivals",&[("shipID",int(hull)),("characterID",int(bad.pilot())),("stationID",int(bad.marker.station)),
                ("sourceSystemID",int(bad.marker.source)),("shipTypeID",int(587)),("createdAt",int(0)),("loss",store::Value::Text("corrupt earlier arrival".into()))])?;
        }
        Ok::<_,StoreError>(())
    }).unwrap();
    let (mut good,marker,_)=docked_pilot(&bad.server).await;
    good.fight.as_mut().unwrap().vitals.hull_damage=good.fight.as_ref().unwrap().vitals.hull_max;
    let loss=Loss::of(&good,marker.source,false);
    let raw=serde_json::to_string(&StoredLoss::from(&loss)).unwrap();
    fixture::set(store,"pendingStationArrivals",good.id,&[("loss",store::Value::Text(raw.into()))]);
    let good_blocked=|| store.read(|db| blocked(db,good.id)).unwrap();
    retry(&bad.server,store).await;
    assert!(good_blocked());
    retry(&bad.server,store).await;
    assert!(!good_blocked(),"valid arrival beyond corrupt first page must settle");
    assert_eq!(kills(store,good.id),1);
    let quarantined=store.read(|db| db.table("pendingStationArrivals").find(&["characterID"],&[int(bad.pilot())])
        .filter(|p| p.text("loss").is_some()).filter_map(|p| p.int("shipID")).filter(|ship| db.table("entity").get_id(*ship).is_some()).count());
    assert_eq!(quarantined,64);
    retry(&bad.server,store).await;
    assert_eq!(bad.server.world.lock().pending_station_arrival_cursor,0);
}

#[tokio::test]
async fn failed_healthy_arrival_flush_keeps_marker_and_retries_latest_damage_without_a_live_park() {
    let f=Fixture::new().await;let store=f.store();
    let mut rx={let mut world=f.server.world.lock();let rx=world.connect(1);world.set_presence(1,Some(Presence {
        char_id:f.pilot(),corp_id:1000167,solar_system_id:f.marker.source,docked:false,station_id:None,role:0}));rx};
    // The pilot is not where the marker left them for now, so the final
    // snapshot cannot be saved.
    fixture::set(store,"character_",f.pilot(),&[("stationID",int(0))]);
    let result=finish(&f.server,store,f.marker,f.ship.clone()).await;
    fixture::set(store,"character_",f.pilot(),&[("stationID",int(f.marker.station))]);
    assert!(result.is_err());f.blocked();
    assert_eq!(f.server.world.lock().pending_station_arrivals.len(),1);
    assert!(f.server.world.lock().parks.is_empty());
    assert!(f.recorded().is_none());
    retry(&f.server,store).await;
    assert!(!store.read(|db| blocked_character(db,f.pilot())).unwrap());
    assert!(f.server.world.lock().pending_station_arrivals.is_empty());
    let damage=|column:&str| store.read(|db| db.table("entity_attributes").get(&[int(f.ship.id),int(3)]).and_then(|a| a.float(column)));
    assert_eq!(damage("valueFloat").or_else(|| damage("valueInt")),Some(17.0));
    while let Ok(event)=rx.try_recv(){assert!(!matches!(event,Push::Move(_)),"background completion cannot redirect a newer session; failed transition connections close and reconnect from the store");}
    // Replaying the same final snapshot cannot overwrite later station repair.
    store.write(|tx| {
        let row=tx.table("entity_attributes").get(&[int(f.ship.id),int(3)]).unwrap().id;
        tx.update("entity_attributes",row,&[("valueFloat",store::Value::Float(0.0))])
    }).unwrap();
    finish(&f.server,store,f.marker,f.ship.clone()).await.unwrap();
    assert_eq!(damage("valueFloat"),Some(0.0));
}

#[tokio::test]
async fn durable_lethal_arrival_recovers_after_restart_and_settles_insurance_and_loss_once() {
    let mut f=Fixture::new().await;f.ship.fight.as_mut().unwrap().vitals.hull_damage=f.ship.fight.as_ref().unwrap().vitals.hull_max;
    let store=f.server.store.clone();
    let now=crate::bulkdata::filetime_now();
    store.write(|tx| fixture::insure(tx,f.ship.id,f.ship.id,f.pilot(),now-1,now+600_000_000)).unwrap();
    fixture::refuse_next_kill(&store);
    let result=finish(&f.server,&store,f.marker,f.ship.clone()).await;
    assert!(result.is_err());f.blocked();
    assert!(decode(&f.recorded().unwrap(),f.marker).is_some());
    assert!(exists(&store,f.cargo));
    f.server.world=Default::default(); // No detached Ship or notification identity survives restart.
    retry(&f.server,&store).await;
    assert!(!store.read(|db| blocked_character(db,f.pilot())).unwrap());
    let capsule=fixture::character_int(&store,f.pilot(),"shipID").unwrap();
    assert_ne!(capsule,f.ship.id);assert_eq!(fixture::character_float(&store,f.pilot(),"balance"),Some(1500.0));
    retry(&f.server,&store).await;
    let repeated=finish(&f.server,&store,f.marker,f.ship.clone()).await.unwrap();assert_eq!(repeated.ship_id,capsule);
    assert_eq!(kills(&store,f.ship.id),1);
    assert_eq!(fixture::character_float(&store,f.pilot(),"balance"),Some(1500.0));
}

#[tokio::test]
async fn marker_without_a_final_snapshot_and_corrupt_loss_stay_quarantined_without_invented_outcomes() {
    let f=Fixture::new().await;let store=f.store();
    retry(&f.server,store).await;f.blocked();
    assert!(f.server.world.lock().pending_station_arrivals.is_empty());
    fixture::set(store,"pendingStationArrivals",f.ship.id,&[("loss",store::Value::Text("{}".into()))]);
    retry(&f.server,store).await;f.blocked();
    assert_eq!(fixture::character_int(store,f.pilot(),"shipID"),Some(f.ship.id));
    assert!(exists(store,f.cargo));
    assert_eq!(kills(store,f.ship.id),0);
    assert!(store.read(|db| db.table("entity_attributes").get(&[int(f.ship.id),int(3)]).is_none()),"no guessed heal or death is persisted");
}
