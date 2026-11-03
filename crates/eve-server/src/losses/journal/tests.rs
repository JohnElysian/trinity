use super::*;
mod arrival;

struct Fixture {server:Server,ship:Ship,cargo:i64,station:i64,system:i64}
impl Fixture {
    async fn new(pod:bool)->Self {
        let f=crate::test_support::Database::load().await;
        let (pilot,hull,cargo)=(f.id().await,f.id().await,f.id().await);
        let row=f.db.query_one(r#"SELECT "stationID","solarSystemID","constellationID","regionID" FROM "staStations" WHERE "stationID"=60014659"#,&[]).await.unwrap();
        let (station,system,constellation,region)=(row.get::<_,i32>(0),row.get::<_,i32>(1),row.get::<_,i32>(2),row.get::<_,i32>(3));
        f.db.execute(r#"INSERT INTO "character_"("characterID","characterName","corporationID","shipID","stationID","solarSystemID","constellationID","regionID",balance)
            VALUES($1,$2,1000167,$3,0,$4,$5,$6,1000)"#,&[&pilot,&format!("Space loss {pilot}"),&hull,&system,&constellation,&region]).await.unwrap();
        let kind=if pod{670}else{587};
        for (id,ty,location,flag) in [(pilot,1373,system,57),(hull,kind,system,0),(cargo,34,hull,5)] {
            f.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton,x,y,z)
                VALUES($1,$2,$3,$4,$5,1,1,11,22,33)"#,&[&id,&ty,&pilot,&location,&flag]).await.unwrap();
        }
        let profile=inflight::profile(&f.db,&f.dogma,pilot as i64,hull as i64,&BTreeSet::new(),combat::now_ms()).await.unwrap().unwrap();
        let mut ship=Ship::new(hull as i64,1,profile.stats,profile.pilot,Vec3::new(11.0,22.0,33.0),Vec3::ZERO);ship.fight=Some(profile.fighter);
        let server=Server {table:Default::default(),db:crate::db::Shared::new(f.db,f.url.clone()),db_url:f.url,node_id:1,bulk:f.bulk,dogma:f.dogma,
            services:crate::svc::registry(),world:f.world,dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0};
        Self{server,ship,cargo:cargo as i64,station:station as i64,system:system as i64}
    }
    fn loss(&self)->Loss {Loss::of(&self.ship,self.system,self.ship.pilot.type_id==670)}
    async fn insured(&self) {
        let db=self.server.db.snapshot();let now=crate::bulkdata::filetime_now();
        db.execute(r#"INSERT INTO "insuranceContracts"("contractID","shipID","ownerID","startDate","endDate",fraction,amount,payout,"isCorp")
            VALUES($1,$1,$2,$3,$4,0.5,100,500,0)"#,&[&(self.ship.id as i32),&(self.ship.pilot.char_id as i32),&(now-1),&(now+600_000_000)]).await.unwrap();
    }
    async fn cleanup(&self) {
        let db=self.server.db.snapshot();let pilot=self.ship.pilot.char_id as i32;
        db.execute(r#"DELETE FROM "killLogParticipants" WHERE "killID" IN(SELECT "killID" FROM "killLog" WHERE "victimCharID"=$1)"#,&[&pilot]).await.unwrap();
        db.execute(r#"DELETE FROM "killLog" WHERE "victimCharID"=$1"#,&[&pilot]).await.unwrap();
        db.execute(r#"DELETE FROM "insuranceContracts" WHERE "ownerID"=$1"#,&[&pilot]).await.unwrap();
        db.execute(r#"DELETE FROM entity_attributes WHERE "itemID" IN(SELECT "itemID" FROM entity WHERE "ownerID"=$1)"#,&[&pilot]).await.unwrap();
        db.execute(r#"DELETE FROM entity WHERE "ownerID"=$1"#,&[&pilot]).await.unwrap();
        db.execute(r#"DELETE FROM "character_" WHERE "characterID"=$1"#,&[&pilot]).await.unwrap();
    }
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn lethal_checkpoint_preserves_original_threshold_and_loss_after_restart() {
    let mut f=Fixture::new(false).await;f.insured().await;
    let inactive_max=f.ship.fight.as_ref().unwrap().vitals.hull_max;
    assert!(inactive_max>20.0);
    f.ship.fight.as_mut().unwrap().vitals.hull_max=10.0;f.ship.fight.as_mut().unwrap().vitals.hull_damage=11.0;
    let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    assert!(crate::space_state::save_detached(&mut db,f.system,&f.ship,crate::destiny::tick_now()).await.unwrap());
    assert!(super::super::pending::blocked_character(&db,f.ship.pilot.char_id).await.unwrap());
    f.server.world=Default::default();
    retry(&f.server,&mut db).await;
    let row=db.query_one(r#"SELECT "shipID",balance FROM "character_" WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32)]).await.unwrap();
    assert_ne!(i64::from(row.get::<_,i32>(0)),f.ship.id);assert_eq!(row.get::<_,f64>(1),1500.0);
    assert_eq!(db.query_one(r#"SELECT count(*) FROM "killLog" WHERE "victimItemID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,i64>(0),1);
    assert!(!super::super::pending::blocked_character(&db,f.ship.pilot.char_id).await.unwrap());
    assert!(f.server.world.lock().pending_ship_losses.is_empty());
    handle(&f.server,&mut db,f.loss()).await;
    assert_eq!(db.query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32)]).await.unwrap().get::<_,f64>(0),1500.0);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn failed_kill_record_keeps_hull_contents_insurance_and_durable_loss_for_retry() {
    let mut f=Fixture::new(false).await;f.insured().await;
    let mut db=crate::db::connect(&f.server.db_url).await.unwrap();let trigger=format!("reject_space_kill_{}",f.ship.id);
    db.batch_execute(&format!(r#"CREATE FUNCTION {trigger}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        IF NEW."victimItemID"={} THEN RAISE EXCEPTION 'forced space kill record failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER {trigger} BEFORE INSERT ON "killLog" FOR EACH ROW EXECUTE FUNCTION {trigger}();"#,f.ship.id)).await.unwrap();
    handle(&f.server,&mut db,f.loss()).await;
    db.batch_execute(&format!(r#"DROP TRIGGER {trigger} ON "killLog"; DROP FUNCTION {trigger}();"#)).await.unwrap();
    assert!(blocked_in_memory(&f.server.world,f.ship.id));
    assert!(super::super::pending::blocked_character(&db,f.ship.pilot.char_id).await.unwrap());
    assert_eq!(db.query_one(r#"SELECT count(*) FROM entity WHERE "itemID"=ANY($1)"#,&[&vec![f.ship.id as i32,f.cargo as i32]]).await.unwrap().get::<_,i64>(0),2);
    assert_eq!(db.query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32)]).await.unwrap().get::<_,f64>(0),1000.0);
    assert!(db.query_opt(r#"SELECT 1 FROM "insuranceContracts" WHERE "shipID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().is_some());
    f.server.world=Default::default();retry(&f.server,&mut db).await;
    assert!(completed(&db,&f.loss()).await.unwrap());
    assert_eq!(db.query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32)]).await.unwrap().get::<_,f64>(0),1500.0);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn capsule_loss_with_unavailable_home_rolls_back_then_recovers_without_orphaned_cargo() {
    let f=Fixture::new(true).await;let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    crate::svc::medical::replace_clone(&db,f.ship.pilot.char_id,TYPE_CLONE_ALPHA,Some(-1)).await.unwrap();
    handle(&f.server,&mut db,f.loss()).await;
    assert!(super::super::pending::blocked_character(&db,f.ship.pilot.char_id).await.unwrap());
    assert_eq!(db.query_one(r#"SELECT count(*) FROM entity WHERE "itemID"=ANY($1)"#,&[&vec![f.ship.id as i32,f.cargo as i32]]).await.unwrap().get::<_,i64>(0),2);
    crate::svc::medical::replace_clone(&db,f.ship.pilot.char_id,TYPE_CLONE_ALPHA,Some(f.station)).await.unwrap();
    retry(&f.server,&mut db).await;
    assert!(completed(&db,&f.loss()).await.unwrap());
    assert!(db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(f.cargo as i32)]).await.unwrap().is_none());
    assert_eq!(db.query_one(r#"SELECT "stationID" FROM "character_" WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32)]).await.unwrap().get::<_,i32>(0),f.station as i32);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn delayed_capsule_publication_preserves_reconnected_motion_and_newer_boarding() {
    let f=Fixture::new(false).await;let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    handle(&f.server,&mut db,f.loss()).await;
    let capsule=i64::from(db.query_one(r#"SELECT "shipID" FROM "character_" WHERE "characterID"=$1"#,
        &[&(f.ship.pilot.char_id as i32)]).await.unwrap().get::<_,i32>(0));
    let profile=inflight::profile(&db,&f.server.dogma,f.ship.pilot.char_id,capsule,&BTreeSet::new(),combat::now_ms()).await.unwrap().unwrap();
    let pos=Vec3::new(7000.0,8000.0,9000.0);let vel=Vec3::new(20.0,30.0,40.0);
    let mut active=Ship::new(capsule,55,profile.stats,profile.pilot,pos,vel);active.fight=Some(profile.fighter);
    let mut rx={
        let mut world=f.server.world.lock();let rx=world.connect(55);
        world.set_presence(55,Some(crate::world::Presence {char_id:f.ship.pilot.char_id,corp_id:1000167,solar_system_id:f.system,docked:false,station_id:None,role:0}));
        let mut park=crate::ballpark::Park::new(f.system,vec![],crate::destiny::tick_now());park.ships.insert(capsule,active);world.parks.insert(f.system,park);rx
    };
    super::super::publish_capsule(&f.server,&mut db,&f.loss(),capsule).await.unwrap();
    {
        let world=f.server.world.lock();let current=&world.parks[&f.system].ships[&capsule];
        assert_eq!(current.client_id,55);assert_eq!(current.pos,pos);assert_eq!(current.vel,vel);
    }
    assert!(rx.try_recv().is_err(),"already reconnected capsule receives no stale relocation or reset");
    let replacement=db.query_one("SELECT nextval('dyn_id_seq')::integer",&[]).await.unwrap().get::<_,i32>(0);
    db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,587,$2,$3,0,1,1)"#,
        &[&replacement,&(f.ship.pilot.char_id as i32),&(f.system as i32)]).await.unwrap();
    let mut writer=crate::db::connect(&f.server.db_url).await.unwrap();let tx=writer.transaction().await.unwrap();
    tx.execute(r#"UPDATE "character_" SET "shipID"=$2 WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32),&replacement]).await.unwrap();
    {
        let loss=f.loss();let mut publishing=std::pin::pin!(super::super::publish_capsule(&f.server,&mut db,&loss,capsule));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50),&mut publishing).await.is_err());
        f.server.world.lock().parks.get_mut(&f.system).unwrap().ships.remove(&capsule);
        tx.commit().await.unwrap();publishing.await.unwrap();
    }
    assert!(!f.server.world.lock().parks[&f.system].ships.contains_key(&capsule));
    assert!(rx.try_recv().is_err(),"newer boarding receives no captured old loss movement");
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn first_loss_attribution_is_immutable_and_corrupt_payloads_remain_quarantined() {
    let mut f=Fixture::new(false).await;
    let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    let mut original=f.loss();
    original.final_blow=Some(-587);
    original.attackers=vec![Attacker {who:-587,ship_type:587,weapon_type:34,damage:42.0}];
    assert!(capture(&mut db,&original).await.unwrap().is_some());
    let mut conflicting=original.clone();
    conflicting.final_blow=Some(-601);conflicting.attackers[0].damage=99.0;
    f.server.world=Default::default();
    handle(&f.server,&mut db,conflicting).await;
    let report=db.query_one(r#"SELECT "finalBlowID","totalDamage" FROM "killLog" WHERE "victimItemID"=$1"#,
        &[&(f.ship.id as i32)]).await.unwrap();
    assert_eq!(report.get::<_,i32>(0),-587);assert_eq!(report.get::<_,f64>(1),42.0);
    f.cleanup().await;

    let mut f=Fixture::new(false).await;
    let original=f.loss();
    assert!(capture(&mut db,&original).await.unwrap().is_some());
    db.execute(r#"UPDATE "pendingShipLosses" SET loss='broken journal payload' WHERE "shipID"=$1"#,
        &[&(f.ship.id as i32)]).await.unwrap();
    f.server.world=Default::default();
    retry(&f.server,&mut db).await;
    handle(&f.server,&mut db,original.clone()).await;
    assert!(blocked_in_memory(&f.server.world,f.ship.id));
    assert!(super::super::pending::blocked_character(&db,f.ship.pilot.char_id).await.unwrap());
    assert_eq!(db.query_one(r#"SELECT count(*) FROM entity WHERE "itemID"=ANY($1)"#,
        &[&vec![f.ship.id as i32,f.cargo as i32]]).await.unwrap().get::<_,i64>(0),2);
    assert_eq!(db.query_one(r#"SELECT loss FROM "pendingShipLosses" WHERE "shipID"=$1"#,
        &[&(f.ship.id as i32)]).await.unwrap().get::<_,String>(0),"broken journal payload");
    assert_eq!(db.query_one(r#"SELECT count(*) FROM "killLog" WHERE "victimItemID"=$1"#,
        &[&(f.ship.id as i32)]).await.unwrap().get::<_,i64>(0),0);
    db.execute(r#"UPDATE "pendingShipLosses" SET loss=$2 WHERE "shipID"=$1"#,
        &[&(f.ship.id as i32),&serde_json::to_string(&original).unwrap()]).await.unwrap();
    retry(&f.server,&mut db).await;
    assert!(completed(&db,&original).await.unwrap());
    assert!(!blocked_in_memory(&f.server.world,f.ship.id));
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn quarantined_page_does_not_starve_later_durable_losses() {
    let bad=Fixture::new(false).await;
    let mut db=crate::db::connect(&bad.server.db_url).await.unwrap();
    db.execute(r#"WITH hulls AS (
        INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton)
        SELECT nextval('dyn_id_seq')::integer,587,$1,$2,0,1,1 FROM generate_series(1,64) RETURNING "itemID"
        ) INSERT INTO "pendingShipLosses"("shipID","characterID","solarSystemID","shipTypeID",occupied,"createdAt",loss)
        SELECT "itemID",$1,$2,587,false,0,'corrupt earlier record' FROM hulls"#,
        &[&(bad.ship.pilot.char_id as i32),&(bad.system as i32)]).await.unwrap();
    let good=Fixture::new(false).await;
    capture(&mut db,&good.loss()).await.unwrap().unwrap();
    retry(&good.server,&mut db).await;
    assert!(!completed(&db,&good.loss()).await.unwrap(),"first page is quarantined older hulls");
    retry(&good.server,&mut db).await;
    assert!(completed(&db,&good.loss()).await.unwrap(),"next page must reach the valid loss");
    assert_eq!(db.query_one(r#"SELECT count(*) FROM "pendingShipLosses" WHERE "characterID"=$1"#,
        &[&(bad.ship.pilot.char_id as i32)]).await.unwrap().get::<_,i64>(0),64);
    assert_eq!(db.query_one(r#"SELECT count(*) FROM entity e JOIN "pendingShipLosses" p ON p."shipID"=e."itemID" WHERE p."characterID"=$1"#,
        &[&(bad.ship.pilot.char_id as i32)]).await.unwrap().get::<_,i64>(0),64);
    retry(&good.server,&mut db).await;
    assert_eq!(good.server.world.lock().pending_ship_loss_cursor,0,"exhausted scan wraps for new older IDs");
    bad.cleanup().await;good.cleanup().await;
}
