use std::sync::Arc;

use super::*;
use super::super::fixture::{self, exists, int, kills};
mod arrival;

struct Fixture {server:Server,ship:Ship,cargo:i64,station:i64,system:i64}
impl Fixture {
    async fn new(pod:bool)->Self {
        let f=crate::test_support::Database::load().await;
        let (station,system,_,_)=fixture::station(&f.store);
        let (ship,cargo)=pilot_in_space(&f.store,&f.dogma,pod);
        Self{server:fixture::server(f),ship,cargo,station,system}
    }
    fn loss(&self)->Loss {Loss::of(&self.ship,self.system,self.ship.pilot.type_id==670)}
    fn insured(&self) {
        let now=crate::bulkdata::filetime_now();
        self.server.store.write(|tx| fixture::insure(tx,self.ship.id,self.ship.id,self.ship.pilot.char_id,now-1,now+600_000_000)).unwrap();
    }
    fn store(&self)->Arc<Store> {self.server.store.clone()}
    fn pilot(&self)->i64 {self.ship.pilot.char_id}
}

/// A pilot in a fresh hull (a capsule if `pod`) in space in the fixture
/// station's system, with cargo: the ship as the heartbeat sees it, and the
/// cargo.
fn pilot_in_space(store:&Store,dogma:&crate::dogma::Catalog,pod:bool)->(Ship,i64) {
    let (pilot,hull,cargo)=(fixture::id(store),fixture::id(store),fixture::id(store));
    let (_,system,constellation,region)=fixture::station(store);
    let kind=if pod{670}else{587};
    store.write(|tx| {
        fixture::character(tx,pilot,"Space loss",1000167,(hull,0,system,constellation,region),1000.0)?;
        for (id,ty,location,flag) in [(pilot,1373,system,57),(hull,kind,system,0),(cargo,34,hull,5)] {
            fixture::item(tx,(id,ty,pilot,location,flag,1),true,(11.0,22.0,33.0))?;
        }
        Ok::<_,StoreError>(())
    }).unwrap();
    let profile=store.read(|db| inflight::profile(db,dogma,pilot,hull,&BTreeSet::new(),combat::now_ms())).unwrap().unwrap();
    let mut ship=Ship::new(hull,1,profile.stats,profile.pilot,Vec3::new(11.0,22.0,33.0),Vec3::ZERO);ship.fight=Some(profile.fighter);
    (ship,cargo)
}

fn blocked_character(store:&Store,pilot:i64)->bool {
    store.read(|db| super::super::pending::blocked_character(db,pilot)).unwrap()
}

fn is_completed(store:&Store,loss:&Loss)->bool {
    store.read(|db| completed(db,loss))
}

#[tokio::test]
async fn lethal_checkpoint_preserves_original_threshold_and_loss_after_restart() {
    let mut f=Fixture::new(false).await;f.insured();
    let store=f.store();
    let inactive_max=f.ship.fight.as_ref().unwrap().vitals.hull_max;
    assert!(inactive_max>20.0);
    f.ship.fight.as_mut().unwrap().vitals.hull_max=10.0;f.ship.fight.as_mut().unwrap().vitals.hull_damage=11.0;
    assert!(crate::space_state::save_detached(&store,f.system,&f.ship,crate::destiny::tick_now()).await.unwrap());
    assert!(blocked_character(&store,f.pilot()));
    f.server.world=Default::default();
    retry(&f.server,&store).await;
    assert_ne!(fixture::character_int(&store,f.pilot(),"shipID"),Some(f.ship.id));
    assert_eq!(fixture::character_float(&store,f.pilot(),"balance"),Some(1500.0));
    assert_eq!(kills(&store,f.ship.id),1);
    assert!(!blocked_character(&store,f.pilot()));
    assert!(f.server.world.lock().pending_ship_losses.is_empty());
    handle(&f.server,&store,f.loss()).await;
    assert_eq!(fixture::character_float(&store,f.pilot(),"balance"),Some(1500.0));
}

#[tokio::test]
async fn failed_kill_record_keeps_hull_contents_insurance_and_durable_loss_for_retry() {
    let mut f=Fixture::new(false).await;f.insured();
    let store=f.store();
    fixture::refuse_next_kill(&store);
    handle(&f.server,&store,f.loss()).await;
    assert!(blocked_in_memory(&f.server.world,f.ship.id));
    assert!(blocked_character(&store,f.pilot()));
    assert!(exists(&store,f.ship.id) && exists(&store,f.cargo));
    assert_eq!(fixture::character_float(&store,f.pilot(),"balance"),Some(1000.0));
    assert!(store.read(|db| db.table("insuranceContracts").find_one(&["shipID"],&[int(f.ship.id)]).is_some()));
    f.server.world=Default::default();retry(&f.server,&store).await;
    assert!(is_completed(&store,&f.loss()));
    assert_eq!(fixture::character_float(&store,f.pilot(),"balance"),Some(1500.0));
}

#[tokio::test]
async fn capsule_loss_with_unavailable_home_rolls_back_then_recovers_without_orphaned_cargo() {
    let f=Fixture::new(true).await;let store=f.store();
    store.write(|tx| crate::svc::medical::replace_clone(tx,f.pilot(),TYPE_CLONE_ALPHA,Some(-1))).unwrap();
    handle(&f.server,&store,f.loss()).await;
    assert!(blocked_character(&store,f.pilot()));
    assert!(exists(&store,f.ship.id) && exists(&store,f.cargo));
    store.write(|tx| crate::svc::medical::replace_clone(tx,f.pilot(),TYPE_CLONE_ALPHA,Some(f.station))).unwrap();
    retry(&f.server,&store).await;
    assert!(is_completed(&store,&f.loss()));
    assert!(!exists(&store,f.cargo));
    assert_eq!(fixture::character_int(&store,f.pilot(),"stationID"),Some(f.station));
}

#[tokio::test]
async fn delayed_capsule_publication_preserves_reconnected_motion_and_newer_boarding() {
    let f=Fixture::new(false).await;let store=f.store();
    handle(&f.server,&store,f.loss()).await;
    let capsule=fixture::character_int(&store,f.pilot(),"shipID").unwrap();
    let profile=store.read(|db| inflight::profile(db,&f.server.dogma,f.pilot(),capsule,&BTreeSet::new(),combat::now_ms())).unwrap().unwrap();
    let pos=Vec3::new(7000.0,8000.0,9000.0);let vel=Vec3::new(20.0,30.0,40.0);
    let mut active=Ship::new(capsule,55,profile.stats,profile.pilot,pos,vel);active.fight=Some(profile.fighter);
    let mut rx={
        let mut world=f.server.world.lock();let rx=world.connect(55);
        world.set_presence(55,Some(crate::world::Presence {char_id:f.pilot(),corp_id:1000167,solar_system_id:f.system,docked:false,station_id:None,role:0}));
        let mut park=crate::ballpark::Park::new(f.system,vec![],crate::destiny::tick_now());park.ships.insert(capsule,active);world.parks.insert(f.system,park);rx
    };
    super::super::publish_capsule(&f.server,&store,&f.loss(),capsule).await.unwrap();
    {
        let world=f.server.world.lock();let current=&world.parks[&f.system].ships[&capsule];
        assert_eq!(current.client_id,55);assert_eq!(current.pos,pos);assert_eq!(current.vel,vel);
    }
    assert!(rx.try_recv().is_err(),"already reconnected capsule receives no stale relocation or reset");
    // The pilot boards another hull, and the capsule leaves space, before the
    // delayed publication runs.
    let replacement=fixture::id(&store);
    store.write(|tx| {
        fixture::item(tx,(replacement,587,f.pilot(),f.system,0,1),true,(0.0,0.0,0.0))?;
        update_id(tx,"character_",f.pilot(),&[("shipID",int(replacement))])
    }).unwrap();
    f.server.world.lock().parks.get_mut(&f.system).unwrap().ships.remove(&capsule);
    super::super::publish_capsule(&f.server,&store,&f.loss(),capsule).await.unwrap();
    assert!(!f.server.world.lock().parks[&f.system].ships.contains_key(&capsule));
    assert!(rx.try_recv().is_err(),"newer boarding receives no captured old loss movement");
}

#[tokio::test]
async fn first_loss_attribution_is_immutable_and_corrupt_payloads_remain_quarantined() {
    let mut f=Fixture::new(false).await;let store=f.store();
    let mut original=f.loss();
    original.final_blow=Some(-587);
    original.attackers=vec![Attacker {who:-587,ship_type:587,weapon_type:34,damage:42.0}];
    assert!(capture(&store,&original).unwrap().is_some());
    let mut conflicting=original.clone();
    conflicting.final_blow=Some(-601);conflicting.attackers[0].damage=99.0;
    f.server.world=Default::default();
    handle(&f.server,&store,conflicting).await;
    let report=store.read(|db| db.table("killLog").rows().find(|k| k.int("victimItemID")==Some(f.ship.id)).map(|k| (k.int("finalBlowID"),k.float("totalDamage"))));
    assert_eq!(report,Some((Some(-587),Some(42.0))));

    let mut f=Fixture::new(false).await;let store=f.store();
    let original=f.loss();
    assert!(capture(&store,&original).unwrap().is_some());
    let ship=f.ship.id;
    let payload=|raw:&str| fixture::set(&store,"pendingShipLosses",ship,&[("loss",store::Value::Text(raw.into()))]);
    payload("broken journal payload");
    f.server.world=Default::default();
    retry(&f.server,&store).await;
    handle(&f.server,&store,original.clone()).await;
    assert!(blocked_in_memory(&f.server.world,f.ship.id));
    assert!(blocked_character(&store,f.pilot()));
    assert!(exists(&store,f.ship.id) && exists(&store,f.cargo));
    let kept=store.read(|db| db.table("pendingShipLosses").get_id(f.ship.id).and_then(|p| p.text("loss").map(str::to_string)));
    assert_eq!(kept.as_deref(),Some("broken journal payload"));
    assert_eq!(kills(&store,f.ship.id),0);
    payload(&serde_json::to_string(&original).unwrap());
    retry(&f.server,&store).await;
    assert!(is_completed(&store,&original));
    assert!(!blocked_in_memory(&f.server.world,f.ship.id));
}

#[tokio::test]
async fn quarantined_page_does_not_starve_later_durable_losses() {
    let bad=Fixture::new(false).await;let store=bad.store();
    store.write(|tx| {
        for _ in 0..64 {
            let hull=tx.nextval("dyn_id_seq")?;
            fixture::item(tx,(hull,587,bad.pilot(),bad.system,0,1),true,(0.0,0.0,0.0))?;
            tx.insert("pendingShipLosses",&[("shipID",int(hull)),("characterID",int(bad.pilot())),("solarSystemID",int(bad.system)),
                ("shipTypeID",int(587)),("occupied",store::Value::Bool(false)),("createdAt",int(0)),("loss",store::Value::Text("corrupt earlier record".into()))])?;
        }
        Ok::<_,StoreError>(())
    }).unwrap();
    let (good,_)=pilot_in_space(&store,&bad.server.dogma,false);
    let good=Loss::of(&good,bad.system,false);
    capture(&store,&good).unwrap().unwrap();
    retry(&bad.server,&store).await;
    assert!(!is_completed(&store,&good),"first page is quarantined older hulls");
    retry(&bad.server,&store).await;
    assert!(is_completed(&store,&good),"next page must reach the valid loss");
    let quarantined=store.read(|db| db.table("pendingShipLosses").find(&["characterID"],&[int(bad.pilot())])
        .filter_map(|p| p.int("shipID")).filter(|ship| db.table("entity").get_id(*ship).is_some()).count());
    assert_eq!(quarantined,64);
    retry(&bad.server,&store).await;
    assert_eq!(bad.server.world.lock().pending_ship_loss_cursor,0,"exhausted scan wraps for new older IDs");
}
