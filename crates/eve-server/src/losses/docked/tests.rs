use super::*;

struct Fixture {server:Server,ship:Ship,station:i64,system:i64,cargo:i64,spare:i64}
impl Fixture {
    async fn new(capsule:bool)->Self {
        let f=crate::test_support::Database::load().await;
        let (pilot,hull,cargo,spare)=(f.id().await,f.id().await,f.id().await,f.id().await);
        let row=f.db.query_one(r#"SELECT "stationID","solarSystemID","constellationID","regionID" FROM "staStations" WHERE "stationID"=60014659"#,&[]).await.unwrap();
        let (station,system,constellation,region)=(row.get::<_,i32>(0),row.get::<_,i32>(1),row.get::<_,i32>(2),row.get::<_,i32>(3));
        f.db.execute(r#"INSERT INTO "character_"("characterID","characterName","corporationID","shipID","stationID","solarSystemID","constellationID","regionID",balance)
            VALUES($1,$2,1000167,$3,$4,$5,$6,$7,1000)"#,&[&pilot,&format!("Arrival loss {pilot}"),&hull,&station,&system,&constellation,&region]).await.unwrap();
        let kind=if capsule{670}else{587};
        for (id,ty,location,flag) in [(pilot,1373,station,57),(hull,kind,station,4),(cargo,34,hull,5),(spare,587,station,4)] {
            f.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton,x,y,z)
                VALUES($1,$2,$3,$4,$5,1,1,0,0,0)"#,&[&id,&ty,&pilot,&location,&flag]).await.unwrap();
        }
        let profile=inflight::profile(&f.db,&f.dogma,pilot as i64,hull as i64,&BTreeSet::new(),combat::now_ms()).await.unwrap().unwrap();
        let mut ship=Ship::new(hull as i64,1,profile.stats,profile.pilot,Vec3::new(1000.0,2000.0,3000.0),Vec3::ZERO);
        ship.fight=Some(profile.fighter);
        let fight=ship.fight.as_mut().unwrap();fight.vitals.hull_damage=fight.vitals.hull_max;
        let server=Server {table:Default::default(),db:crate::db::Shared::new(f.db,f.url.clone()),db_url:f.url,node_id:1,bulk:f.bulk,dogma:f.dogma,services:crate::svc::registry(),world:f.world,
            dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0};
        Self {server,ship,station:station as i64,system:system as i64,cargo:cargo as i64,spare:spare as i64}
    }
    async fn insured(&self) {
        let now=crate::bulkdata::filetime_now();
        self.server.db.snapshot().execute(r#"INSERT INTO "insuranceContracts"("contractID","shipID","ownerID","startDate","endDate",fraction,amount,payout,"isCorp")
            VALUES($1,$1,$2,$3,$4,0.5,100,500,0)"#,&[&(self.ship.id as i32),&(self.ship.pilot.char_id as i32),&(now-1),&(now+600_000_000)]).await.unwrap();
    }
    async fn cleanup(self) {
        let pilot=self.ship.pilot.char_id as i32;
        self.server.db.snapshot().execute(r#"DELETE FROM "killLogParticipants" WHERE "killID" IN(SELECT "killID" FROM "killLog" WHERE "victimCharID"=$1)"#,&[&pilot]).await.unwrap();
        self.server.db.snapshot().execute(r#"DELETE FROM "killLog" WHERE "victimCharID"=$1"#,&[&pilot]).await.unwrap();
        self.server.db.snapshot().execute(r#"DELETE FROM "insuranceContracts" WHERE "ownerID"=$1"#,&[&pilot]).await.unwrap();
        self.server.db.snapshot().execute(r#"DELETE FROM entity_attributes WHERE "itemID" IN(SELECT "itemID" FROM entity WHERE "ownerID"=$1)"#,&[&pilot]).await.unwrap();
        self.server.db.snapshot().execute(r#"DELETE FROM entity WHERE "ownerID"=$1"#,&[&pilot]).await.unwrap();
        self.server.db.snapshot().execute(r#"DELETE FROM "character_" WHERE "characterID"=$1"#,&[&pilot]).await.unwrap();
    }
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn docked_loss_rolls_back_on_kill_record_failure_and_retries_without_duplicate_insurance() {
    let f=Fixture::new(false).await;f.insured().await;
    let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    let trigger=format!("reject_arrival_kill_{}",f.ship.id);
    f.server.db.snapshot().batch_execute(&format!(r#"CREATE FUNCTION {trigger}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        IF NEW."victimItemID"={} THEN RAISE EXCEPTION 'forced arrival kill record failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER {trigger} BEFORE INSERT ON "killLog" FOR EACH ROW EXECUTE FUNCTION {trigger}();"#,f.ship.id)).await.unwrap();
    let failed=settle_docked_arrival(&f.server,&mut db,f.station,f.system,&f.ship).await;
    f.server.db.snapshot().batch_execute(&format!(r#"DROP TRIGGER {trigger} ON "killLog"; DROP FUNCTION {trigger}();"#)).await.unwrap();
    assert!(failed.is_err());
    let row=f.server.db.snapshot().query_one(r#"SELECT "shipID",balance FROM "character_" WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32)]).await.unwrap();
    assert_eq!(i64::from(row.get::<_,i32>(0)),f.ship.id);assert_eq!(row.get::<_,f64>(1),1000.0);
    for id in [f.ship.id,f.cargo,f.spare] {assert!(f.server.db.snapshot().query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(id as i32)]).await.unwrap().is_some());}
    assert!(f.server.db.snapshot().query_opt(r#"SELECT 1 FROM "insuranceContracts" WHERE "shipID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().is_some());
    let settled=settle_docked_arrival(&f.server,&mut db,f.station,f.system,&f.ship).await.unwrap();
    assert_eq!(settled.station_id,f.station);assert_eq!(settled.solar_system_id,f.system);assert_ne!(settled.ship_id,f.ship.id);
    assert_eq!(settle_docked_arrival(&f.server,&mut db,f.station,f.system,&f.ship).await.unwrap(),settled);
    let row=f.server.db.snapshot().query_one(r#"SELECT "shipID",balance FROM "character_" WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32)]).await.unwrap();
    assert_eq!(i64::from(row.get::<_,i32>(0)),settled.ship_id);assert_eq!(row.get::<_,f64>(1),1500.0);
    assert_eq!(f.server.db.snapshot().query_one(r#"SELECT count(*) FROM "killLog" WHERE "victimItemID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,i64>(0),1);
    assert!(f.server.db.snapshot().query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().is_none());
    assert!(f.server.db.snapshot().query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(f.spare as i32)]).await.unwrap().is_some());
    let pod=f.server.db.snapshot().query_one(r#"SELECT "typeID","locationID",flag FROM entity WHERE "itemID"=$1"#,&[&(settled.ship_id as i32)]).await.unwrap();
    assert_eq!((pod.get::<_,i32>(0),pod.get::<_,i32>(1),pod.get::<_,i32>(2)),(670,f.station as i32,4));
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn docked_capsule_loss_uses_medical_home_removes_implants_and_does_not_publish_a_move() {
    let f=Fixture::new(true).await;
    let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    let home=f.server.db.snapshot().query_one(r#"SELECT "stationID" FROM "staStations" WHERE "solarSystemID"<>$1 ORDER BY "stationID" LIMIT 1"#,&[&(f.system as i32)]).await.unwrap().get::<_,i32>(0);
    let tx=db.transaction().await.unwrap();medical::replace_clone(&tx,f.ship.pilot.char_id,TYPE_CLONE_ALPHA,Some(home as i64)).await.unwrap();tx.commit().await.unwrap();
    let implant=f.server.db.snapshot().query_one("SELECT nextval('dyn_id_seq')::integer",&[]).await.unwrap().get::<_,i32>(0);
    let implant_type=f.server.db.snapshot().query_one(r#"SELECT "typeID" FROM "invTypes" WHERE "groupID"=300 ORDER BY "typeID" LIMIT 1"#,&[]).await.unwrap().get::<_,i32>(0);
    f.server.db.snapshot().execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,$2,$3,$3,89,1,1)"#,
        &[&implant,&implant_type,&(f.ship.pilot.char_id as i32)]).await.unwrap();
    let mut rx=f.server.world.lock().connect(1);
    let settled=settle_docked_arrival(&f.server,&mut db,f.station,f.system,&f.ship).await.unwrap();
    assert_eq!(settled.station_id,home as i64);assert_ne!(settled.solar_system_id,f.system);
    assert!(f.server.db.snapshot().query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=ANY($1)"#,&[&vec![implant,f.ship.id as i32,f.cargo as i32]]).await.unwrap().is_none());
    assert_eq!(f.server.db.snapshot().query_one(r#"SELECT "isPodKill" FROM "killLog" WHERE "victimItemID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,i16>(0),1);
    while let Ok(event)=rx.try_recv() {assert!(!matches!(event,crate::world::Push::Move(_)));}
    assert_eq!(settle_docked_arrival(&f.server,&mut db,f.station,f.system,&f.ship).await.unwrap(),settled);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn docked_settlement_rejects_alive_stale_or_changed_custody_without_touching_items() {
    let f=Fixture::new(false).await;let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    let mut alive=f.ship.clone();alive.fight.as_mut().unwrap().vitals.hull_damage=0.0;
    assert!(settle_docked_arrival(&f.server,&mut db,f.station,f.system,&alive).await.is_err());
    f.server.db.snapshot().execute(r#"UPDATE "character_" SET "shipID"=$2 WHERE "characterID"=$1"#,
        &[&(f.ship.pilot.char_id as i32),&(f.spare as i32)]).await.unwrap();
    assert!(settle_docked_arrival(&f.server,&mut db,f.station,f.system,&f.ship).await.is_err());
    assert_eq!(f.server.db.snapshot().query_one(r#"SELECT count(*) FROM entity WHERE "itemID"=ANY($1)"#,
        &[&vec![f.ship.id as i32,f.cargo as i32,f.spare as i32]]).await.unwrap().get::<_,i64>(0),3);
    assert_eq!(f.server.db.snapshot().query_one(r#"SELECT count(*) FROM "killLog" WHERE "victimItemID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,i64>(0),0);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn completed_docked_loss_retry_accepts_newer_boarding_and_undock_without_repaying() {
    let f=Fixture::new(false).await;f.insured().await;
    let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    let tx=db.transaction().await.unwrap();
    let marker=crate::losses::pending::begin(&tx,f.ship.pilot.char_id,f.ship.id,f.station,f.system).await.unwrap();
    tx.commit().await.unwrap();
    let mut rx=f.server.world.lock().connect(1);
    let settled=crate::losses::pending::finish(&f.server,&mut db,marker,f.ship.clone()).await.unwrap();
    let pilot=f.ship.pilot.char_id as i32;
    f.server.db.snapshot().execute(r#"UPDATE "character_" SET "shipID"=$2 WHERE "characterID"=$1"#,&[&pilot,&(f.spare as i32)]).await.unwrap();
    let boarded=crate::losses::pending::finish(&f.server,&mut db,marker,f.ship.clone()).await.unwrap();
    assert_eq!(boarded.ship_id,f.spare);assert_eq!(boarded.station_id,f.station);
    assert!(f.server.world.lock().pending_station_arrivals.is_empty());
    let tx=db.transaction().await.unwrap();
    tx.execute(r#"UPDATE "character_" SET "stationID"=0 WHERE "characterID"=$1"#,&[&pilot]).await.unwrap();
    tx.execute(r#"UPDATE entity SET "locationID"=$2,flag=0 WHERE "itemID"=$1"#,&[&(f.spare as i32),&(f.system as i32)]).await.unwrap();
    tx.commit().await.unwrap();
    let undocked=crate::losses::pending::finish(&f.server,&mut db,marker,f.ship.clone()).await.unwrap();
    assert_eq!(undocked.ship_id,f.spare);assert_eq!(undocked.station_id,0);assert_eq!(undocked.solar_system_id,f.system);
    assert!(f.server.world.lock().pending_station_arrivals.is_empty());
    f.server.db.snapshot().execute(r#"UPDATE entity SET "ownerID"=1 WHERE "itemID"=$1"#,&[&(f.spare as i32)]).await.unwrap();
    assert!(crate::losses::pending::finish(&f.server,&mut db,marker,f.ship.clone()).await.is_err(),"newer active hull still needs valid custody");
    assert!(f.server.world.lock().pending_station_arrivals.contains_key(&f.ship.id));
    f.server.db.snapshot().execute(r#"UPDATE entity SET "ownerID"=$2 WHERE "itemID"=$1"#,&[&(f.spare as i32),&pilot]).await.unwrap();
    crate::losses::pending::retry(&f.server,&mut db).await;
    assert!(f.server.world.lock().pending_station_arrivals.is_empty());
    while let Ok(event)=rx.try_recv() {assert!(!matches!(event,crate::world::Push::Move(_)),"retry cannot relocate a newer session");}
    assert_eq!(f.server.db.snapshot().query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,&[&pilot]).await.unwrap().get::<_,f64>(0),1500.0);
    assert_eq!(f.server.db.snapshot().query_one(r#"SELECT count(*) FROM "killLog" WHERE "victimItemID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,i64>(0),1);
    assert!(f.server.db.snapshot().query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1 AND "typeID"=670"#,&[&(settled.ship_id as i32)]).await.unwrap().is_some());
    f.cleanup().await;
}
