use super::*;
use crate::store::Value as Cell;

use super::super::tests::{Fixture,FROM,TO,insert,phase,req,reserve,seed,seed_for,set};

fn server(f:&Fixture)->Server {
    Server {table:Default::default(),store:f.db.store.clone(),node_id:1,
        bulk:crate::bulkdata::BulkData {entries:Default::default(),node_id:1},dogma:Catalog::new(),world:f.db.world.clone(),services:crate::svc::registry(),
        dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0}
}
/// The ship's newest receipt.
fn receipt(f:&Fixture)->Receipt {
    f.db.store.read(|db|db.table("portalReceipts").rows().filter(|r|r.int("shipID")==Some(f.ship)).max_by_key(|r|r.int("generation")).and_then(|r|decode(&r))).unwrap()
}
/// A stored attribute of the ship, as its float value.
fn attribute(f:&Fixture,attribute:i64)->f64 {
    f.db.store.read(|db|db.table("entity_attributes").get(&[Cell::Int(f.ship),Cell::Int(attribute)]).and_then(|a|a.float("valueFloat"))).unwrap()
}
/// An unresolved station arrival of the fixture's hull, attributed to
/// another character: the portal can still be paid, but every final
/// checkpoint of the hull fails until it is gone.
async fn block_checkpoints(f:&Fixture) {
    let other=i64::from(f.db.id().await);
    insert(f,"pendingStationArrivals",&[("shipID",Cell::Int(f.ship)),("characterID",Cell::Int(other)),("stationID",Cell::Int(60014689)),
        ("sourceSystemID",Cell::Int(FROM)),("shipTypeID",Cell::Int(11567)),("createdAt",Cell::Int(combat::now_ms()))]);
}
fn unblock_checkpoints(f:&Fixture) {
    f.db.store.write(|tx|tx.delete_where("pendingStationArrivals",&["shipID"],&[Cell::Int(f.ship)])).unwrap();
}
/// Until the portal's payment is accepted (its receipt unresolved).
async fn accepted(f:&Fixture) {
    loop {if f.db.store.read(|db|blocked(db,f.ship)).unwrap(){break;}tokio::time::sleep(std::time::Duration::from_millis(10)).await;}
}

#[tokio::test]
async fn portal_payment_happens_once_for_standard_and_covert_with_current_combat() {
    for covert in [false,true] {
        let f=Fixture::new(covert).await;
        // 73 differs deliberately: original effect2152 chooses attribute1002.
        f.db.store.write(|tx|{
            for (attribute,value) in [(73,123),(1002,45000)] {
                tx.upsert("entity_attributes",&["itemID","attributeID"],&[("itemID",Cell::Int(f.module)),("attributeID",Cell::Int(attribute)),("valueInt",Cell::Int(value))],
                    &[("valueInt",Cell::Int(value)),("valueFloat",Cell::Null)])?;
            }
            Ok::<_,StoreError>(())
        }).unwrap();
        // Combat before the payment is what finalization records with it.
        {let mut w=f.db.world.lock();let fighter=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap();
            fighter.vitals.hull_damage=17.0;assert!(fighter.vitals.capacitor.spend(combat::now_ms(),50.0));}
        run(req(&f),f.db.store.clone()).await.unwrap();
        assert_eq!(f.fuel().await,if covert{1500}else{1000});
        let expected=if covert{9850.0}else{8950.0};assert_eq!(f.cap(),expected);f.released();assert_eq!(phase(&f),1);
        let r=receipt(&f);assert_eq!(r.intent.expires-r.intent.started,45000);
        {let w=f.db.world.lock();let b=&w.bridges[&f.ship];assert_eq!((b.generation,b.started_ms,b.until_ms),(r.generation,r.intent.started,r.intent.expires));}
        assert_eq!(attribute(&f,3),17.0);assert_eq!(attribute(&f,18),expected);
        assert!(!f.db.store.read(|db|blocked(db,f.ship)).unwrap());assert!(!blocked_in_memory(&f.db.world,f.ship));
        assert!(f.open().await.is_err());assert_eq!(f.fuel().await,if covert{1500}else{1000});
    }
}

#[tokio::test]
async fn refused_payment_restores_exact_recharge_curve_without_paying() {
    let f=Fixture::new(false).await;
    let before={let mut w=f.db.world.lock();let fight=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap();
        fight.vitals.hull_damage=21.0;let cap=&mut fight.vitals.capacitor;
        *cap=Charged::full(10000.0,5000.0,combat::now_ms());cap.set(combat::now_ms(),5000.0);*cap};
    // Too little fuel: the payment is refused after the capacitor is reserved.
    set(&f,"entity",f.fuel,&[("quantity",Cell::Int(1))]);
    assert!(f.open().await.is_err());
    assert_eq!(f.fuel().await,1);f.released();let w=f.db.world.lock();let fighter=w.parks[&FROM].ships[&f.ship].fight.as_ref().unwrap();
    assert_eq!(fighter.vitals.capacitor,before);assert_eq!(fighter.vitals.hull_damage,21.0);assert!(w.bridges.is_empty());
}

#[tokio::test]
async fn accepted_portal_expired_before_finalization_does_not_start_a_fresh_cycle() {
    let f=Fixture::new(false).await;
    let r=seed_for(&f,100);reserve(&f,&r);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    complete_owned(&req(&f),&f.db.store,&r,Vec::new()).await.unwrap();
    assert_eq!(f.cap(),9000.0);assert_eq!(f.fuel().await,1500,"finalization never pays");assert_eq!(phase(&f),2);
    assert!(f.db.world.lock().bridges.is_empty());assert!(f.db.world.lock().portal_cycles.is_empty());f.released();
}

#[tokio::test]
async fn failed_final_checkpoint_retries_fresh_live_state_and_lethal_source_keeps_its_loss() {
    let f=Fixture::new(false).await;
    f.db.world.lock().parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.hull_damage=1.0;
    block_checkpoints(&f).await;
    let (result,())=tokio::time::timeout(std::time::Duration::from_secs(30),async {tokio::join!(f.open(),async {
        accepted(&f).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(attribute(&f,18),9000.0,"initial accepted payment already persisted charge while final checkpoint is blocked");
        {let mut w=f.db.world.lock();let fight=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap();
            fight.vitals.hull_damage=31.0;fight.vitals.capacitor.spend(combat::now_ms(),80.0);}
        unblock_checkpoints(&f);
    })}).await.unwrap();result.unwrap();assert_eq!(f.cap(),8920.0);assert_eq!(f.fuel().await,1000);f.released();
    assert_eq!(attribute(&f,3),31.0);
    // A source that turns lethal before its final checkpoint closes with
    // its loss staged, rather than a checkpoint of a dead hull.
    let f=Fixture::new(false).await;block_checkpoints(&f).await;
    let (result,())=tokio::time::timeout(std::time::Duration::from_secs(30),async {tokio::join!(f.open(),async {
        accepted(&f).await;
        {let mut w=f.db.world.lock();let ship=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap();
            ship.pos=Vec3::new(123.0,456.0,789.0);let f=ship.fight.as_mut().unwrap();f.vitals.hull_damage=f.vitals.hull_max;}
        unblock_checkpoints(&f);
    })}).await.unwrap();result.unwrap();
    assert_eq!(phase(&f),2);assert_eq!(f.fuel().await,1000);assert_eq!(f.cap(),9000.0);f.released();assert!(f.db.world.lock().bridges.is_empty());
    let raw=f.db.store.read(|db|db.table("pendingShipLosses").get_id(f.ship).and_then(|r|r.text("loss").map(str::to_string))).unwrap();
    let loss:Loss=serde_json::from_str(&raw).unwrap();assert_eq!(loss.system,FROM);assert_eq!(loss.pos,Vec3::new(123.0,456.0,789.0));assert!(loss.arrival_custody.is_none());
    assert!(f.db.world.lock().pending_ship_losses.contains_key(&f.ship));
}

#[tokio::test]
async fn restart_keeps_unknown_marker_quarantined_and_never_relinks_a_reused_member_field_id() {
    let f=Fixture::new(false).await;
    insert(&f,"pendingShipLogouts",&[("shipID",Cell::Int(f.ship)),("characterID",Cell::Int(f.session.char_id.unwrap())),("solarSystemID",Cell::Int(FROM)),
        ("createdAt",Cell::Int(combat::now_ms()))]);
    assert!(run(req(&f),f.db.store.clone()).await.is_err(),"direct portal runner obeys shared logout quarantine");assert_eq!(f.cap(),10000.0);assert_eq!(f.fuel().await,1500);
    f.db.store.write(|tx|tx.delete_where("pendingShipLogouts",&["shipID"],&[Cell::Int(f.ship)])).unwrap();
    let r=seed(&f);assert!(run(req(&f),f.db.store.clone()).await.is_err(),"direct runner cannot bypass an earlier pending operation");
    let mut s=server(&f);s.world=World::default();let store=f.db.store.clone();
    retry(&s,&store).await;assert!(store.read(|db|blocked(db,f.ship)).unwrap());assert_eq!(phase(&f),0);
    // A completed checkpoint is not authority to rebuild a disappeared old
    // module/field; even an identical numeric field ID in a new World is not it.
    set(&f,"portalReceipts",r.generation,&[("state",Cell::Int(1))]);
    s.world.lock().parks=std::mem::take(&mut f.db.world.lock().parks);
    assert!(s.world.lock().parks[&TO].beacons.contains_key(&f.beacon),"reused numeric field exists in reconstructed world");
    retry(&s,&store).await;assert_eq!(phase(&f),2);assert!(s.world.lock().bridges.is_empty());
    assert_eq!(f.fuel().await,1500,"recovery never executes the payment recipe");
    // A stale historical receipt cannot overwrite a newer generation.
    let newer=seed(&f);assert_eq!(store.read(|db|locked(db,&r)).unwrap().resolution,Resolution::Superseded);
    assert!(store.read(|db|blocked(db,f.ship)).unwrap());assert!(newer.generation>r.generation);
    let current={let mut w=s.world.lock();let hull=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap();hull.identity_reserved=true;
        let cap=hull.fight.as_ref().unwrap().vitals.capacitor;w.pending_bridges.insert(f.ship);
        let mut old=Pending::owned(r.clone(),1,Charged::full(10000.0,0.0,0),Charged::full(9000.0,0.0,0));old.busy=0;
        w.pending_portals.insert(r.generation,old);w.pending_portals.insert(newer.generation,Pending::owned(newer.clone(),1,cap,cap));
        w.portal_cycles.insert(f.ship,bridge(&newer));cap};
    retry(&s,&store).await;
    {let w=s.world.lock();assert!(!w.pending_portals.contains_key(&r.generation));assert!(w.pending_portals.contains_key(&newer.generation));
        assert!(w.pending_bridges.contains(&f.ship));assert!(w.parks[&FROM].ships[&f.ship].identity_reserved);
        assert_eq!(w.parks[&FROM].ships[&f.ship].fight.as_ref().unwrap().vitals.capacitor,current);assert_eq!(w.portal_cycles[&f.ship].generation,newer.generation);}
}

#[tokio::test]
async fn durable_known_portal_loss_recovers_after_restart_and_corrupt_rows_cannot_starve_it() {
    let f=Fixture::new(false).await;
    let mut corrupt=Vec::new();
    for _ in 0..64 {
        let ship=i64::from(f.db.id().await);
        corrupt.push(f.db.store.write(|tx|{
            let row=tx.insert("portalReceipts",&[("characterID",Cell::Int(ship)),("shipID",Cell::Int(ship)),("intent",Cell::Text("invalid".into()))])?;
            Ok::<_,StoreError>(tx.table("portalReceipts").row(row).and_then(|r|r.int("generation")).unwrap())
        }).unwrap());
    }
    let r=seed(&f);let loss={let w=f.db.world.lock();Loss::of(&w.parks[&FROM].ships[&f.ship],FROM,false)};
    set(&f,"portalReceipts",r.generation,&[("loss",Cell::Text(serde_json::to_string(&loss).unwrap().into()))]);
    let mut s=server(&f);s.world=World::default();s.world.lock().portal_retry_cursor=*corrupt.first().unwrap()-1;
    for _ in 0..3 {retry(&s,&f.db.store).await;}
    assert_eq!(phase(&f),2);assert!(f.db.store.read(|db|db.table("pendingShipLosses").get_id(f.ship).is_some()));
    assert!(s.world.lock().bridges.is_empty());assert_eq!(f.fuel().await,1500);
}

#[tokio::test]
async fn ordinary_loss_settlement_between_portal_phases_closes_only_proven_original_receipt() {
    for retained_payload in [true,false] {
    let f=Fixture::new(false).await;let r=seed(&f);let mut s=server(&f);let store=f.db.store.clone();
    let loss={let mut w=s.world.lock();let ship=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap();
        ship.identity_reserved=true;ship.pos=Vec3::new(101.0,202.0,303.0);let fighter=ship.fight.as_mut().unwrap();fighter.vitals.hull_damage=fighter.vitals.hull_max;
        Loss::of(ship,FROM,false)};
    let cap=f.db.world.lock().parks[&FROM].ships[&f.ship].fight.as_ref().unwrap().vitals.capacitor;
    let mut p=Pending::owned(r.clone(),1,cap,cap);p.loss=Some(loss.clone());p.busy=0;
    s.world.lock().pending_portals.insert(r.generation,p);s.world.lock().pending_bridges.insert(f.ship);
    if retained_payload {set(&f,"portalReceipts",r.generation,&[("loss",Cell::Text(serde_json::to_string(&loss).unwrap().into()))]);}
    // Force the interleaving: a generic recovery owner settles the retained
    // source loss after receipt retention but before portal loss staging.
    crate::losses::journal::handle(&s,&store,loss.clone()).await;
    assert!(store.read(|db|db.table("entity").get_id(f.ship).is_none()));
    let capsule=store.read(|db|db.table("character_").get_id(r.intent.pilot).and_then(|c|c.int("shipID"))).unwrap();
    assert_ne!(capsule,f.ship);assert_eq!(phase(&f),0);
    if !retained_payload {s.world=World::default();}
    // A different attribution is not sufficient proof to release quarantine.
    let attribute_kill=|system:i64| store.write(|tx|{
        let rows:Vec<_>=tx.table("killLog").rows().filter(|k|k.int("victimItemID")==Some(f.ship)).map(|k|k.id).collect();
        for row in rows {tx.update("killLog",row,&[("solarSystemID",Cell::Int(system))])?;}
        Ok::<_,StoreError>(())
    }).unwrap();
    attribute_kill(TO);
    s.world.lock().portal_retry_cursor=r.generation-1;
    retry(&s,&store).await;assert_eq!(phase(&f),0);assert!(store.read(|db|blocked_character(db,r.intent.pilot)).unwrap());
    attribute_kill(FROM);
    s.world.lock().portal_retry_cursor=r.generation-1;
    retry(&s,&store).await;assert_eq!(phase(&f),2);assert!(!store.read(|db|blocked_character(db,r.intent.pilot)).unwrap());
    assert!(!blocked_character_in_memory(&s.world,r.intent.pilot));assert!(!s.world.lock().pending_bridges.contains(&f.ship));assert!(s.world.lock().bridges.is_empty());
    assert_eq!(store.read(|db|db.table("character_").get_id(r.intent.pilot).and_then(|c|c.int("shipID"))).unwrap(),capsule);
    assert_eq!(store.read(|db|db.table("killLog").rows().filter(|k|k.int("victimItemID")==Some(f.ship)).count()),1);
    }
}
