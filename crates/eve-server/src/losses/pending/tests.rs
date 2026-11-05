use super::*;
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
        let f=Database::load().await;let pilot=f.id().await;let hull=f.id().await;let cargo=f.id().await;
        let row=f.db.query_one(r#"SELECT "stationID","solarSystemID","constellationID","regionID" FROM "staStations" WHERE "stationID"=60014659"#,&[]).await.unwrap();
        let (station,system,constellation,region)=(row.get::<_,i32>(0),row.get::<_,i32>(1),row.get::<_,i32>(2),row.get::<_,i32>(3));
        f.db.execute(r#"INSERT INTO "character_"("characterID","characterName","corporationID","shipID","stationID","solarSystemID","constellationID","regionID",balance)
            VALUES($1,$2,1000167,$3,$4,$5,$6,$7,1000)"#,&[&pilot,&format!("Pending arrival {pilot}"),&hull,&station,&system,&constellation,&region]).await.unwrap();
        f.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton,x,y,z) VALUES
            ($1,587,$2,$3,4,1,1,0,0,0),($2,1373,$2,$3,57,1,1,0,0,0),($4,34,$2,$1,5,7,0,0,0,0)"#,&[&hull,&pilot,&station,&cargo]).await.unwrap();
        let session=Session {char_id:Some(pilot as i64),ship_id:Some(hull as i64),station_id:Some(station as i64),solar_system_id2:Some(system as i64),..Default::default()};
        let mut ship=inflight::ship_for_space(&f.ctx(&session),pilot as i64,hull as i64).await.unwrap();
        ship.pos=Vec3::new(111.0,222.0,333.0);ship.fight.as_mut().unwrap().vitals.hull_damage=17.0;
        let server=Server {table:Default::default(),db:crate::db::Shared::new(f.db,f.url.clone()),db_url:f.url,node_id:1,bulk:f.bulk,dogma:f.dogma,world:f.world,
            services:crate::svc::registry(),dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0};
        let mut db=crate::db::connect(&server.db_url).await.unwrap();let tx=db.transaction().await.unwrap();
        let marker=begin(&tx,pilot as i64,hull as i64,station as i64,system as i64).await.unwrap();tx.commit().await.unwrap();
        Self {server,ship,marker,cargo:cargo as i64}
    }
    async fn blocked(&self) {
        assert!(blocked(&*self.server.db.snapshot(),self.ship.id).await.unwrap());
        assert!(blocked_character(&*self.server.db.snapshot(),self.ship.pilot.char_id).await.unwrap());
    }
    async fn cleanup(&self) {
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
async fn quarantined_station_page_does_not_starve_later_confirmed_arrivals() {
    let bad=Fixture::new().await;
    let mut db=crate::db::connect(&bad.server.db_url).await.unwrap();
    db.execute(r#"WITH hulls AS (
        INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton)
        SELECT nextval('dyn_id_seq')::integer,587,$1,$2,4,1,1 FROM generate_series(1,64) RETURNING "itemID"
        ) INSERT INTO "pendingStationArrivals"("shipID","characterID","stationID","sourceSystemID","shipTypeID","createdAt",loss)
        SELECT "itemID",$1,$2,$3,587,0,'corrupt earlier arrival' FROM hulls"#,
        &[&(bad.ship.pilot.char_id as i32),&(bad.marker.station as i32),&(bad.marker.source as i32)]).await.unwrap();
    let mut good=Fixture::new().await;
    good.ship.fight.as_mut().unwrap().vitals.hull_damage=good.ship.fight.as_ref().unwrap().vitals.hull_max;
    let loss=Loss::of(&good.ship,good.marker.source,false);
    let raw=serde_json::to_string(&StoredLoss::from(&loss)).unwrap();
    db.execute(r#"UPDATE "pendingStationArrivals" SET loss=$2 WHERE "shipID"=$1"#,
        &[&(good.ship.id as i32),&raw]).await.unwrap();
    retry(&good.server,&mut db).await;
    assert!(blocked(&db,good.ship.id).await.unwrap());
    retry(&good.server,&mut db).await;
    assert!(!blocked(&db,good.ship.id).await.unwrap(),"valid arrival beyond corrupt first page must settle");
    assert_eq!(db.query_one(r#"SELECT count(*) FROM "killLog" WHERE "victimItemID"=$1"#,
        &[&(good.ship.id as i32)]).await.unwrap().get::<_,i64>(0),1);
    assert_eq!(db.query_one(r#"SELECT count(*) FROM "pendingStationArrivals" p JOIN entity e ON e."itemID"=p."shipID"
        WHERE p."characterID"=$1 AND p.loss IS NOT NULL"#,
        &[&(bad.ship.pilot.char_id as i32)]).await.unwrap().get::<_,i64>(0),64);
    retry(&good.server,&mut db).await;
    assert_eq!(good.server.world.lock().pending_station_arrival_cursor,0);
    bad.cleanup().await;good.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn failed_healthy_arrival_flush_keeps_marker_and_retries_latest_damage_without_a_live_park() {
    let f=Fixture::new().await;let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    let mut rx={let mut world=f.server.world.lock();let rx=world.connect(1);world.set_presence(1,Some(Presence {
        char_id:f.ship.pilot.char_id,corp_id:1000167,solar_system_id:f.marker.source,docked:false,station_id:None,role:0}));rx};
    let name=format!("reject_pending_vitals_{}",f.ship.id);
    f.server.db.snapshot().batch_execute(&format!(r#"CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        IF NEW."itemID"={} THEN RAISE EXCEPTION 'forced final snapshot failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER {name} BEFORE INSERT OR UPDATE ON entity_attributes FOR EACH ROW EXECUTE FUNCTION {name}();"#,f.ship.id)).await.unwrap();
    let result=finish(&f.server,&mut db,f.marker,f.ship.clone()).await;
    f.server.db.snapshot().batch_execute(&format!("DROP TRIGGER {name} ON entity_attributes; DROP FUNCTION {name}();")).await.unwrap();
    assert!(result.is_err());f.blocked().await;
    assert_eq!(f.server.world.lock().pending_station_arrivals.len(),1);
    assert!(f.server.world.lock().parks.is_empty());
    assert!(f.server.db.snapshot().query_one(r#"SELECT loss IS NULL FROM "pendingStationArrivals" WHERE "shipID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,bool>(0));
    retry(&f.server,&mut db).await;
    assert!(!blocked_character(&*f.server.db.snapshot(),f.ship.pilot.char_id).await.unwrap());
    assert!(f.server.world.lock().pending_station_arrivals.is_empty());
    let damage=f.server.db.snapshot().query_one(r#"SELECT COALESCE("valueFloat","valueInt"::float8) FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,f64>(0);
    assert_eq!(damage,17.0);
    while let Ok(event)=rx.try_recv(){assert!(!matches!(event,Push::Move(_)),"background completion cannot redirect a newer session; failed transition connections close and reconnect from the database");}
    // Replaying the same final snapshot cannot overwrite later station repair.
    f.server.db.snapshot().execute(r#"UPDATE entity_attributes SET "valueFloat"=0 WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(f.ship.id as i32)]).await.unwrap();
    finish(&f.server,&mut db,f.marker,f.ship.clone()).await.unwrap();
    assert_eq!(f.server.db.snapshot().query_one(r#"SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,f64>(0),0.0);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn durable_lethal_arrival_recovers_after_restart_and_settles_insurance_and_loss_once() {
    let mut f=Fixture::new().await;f.ship.fight.as_mut().unwrap().vitals.hull_damage=f.ship.fight.as_ref().unwrap().vitals.hull_max;
    let now=crate::bulkdata::filetime_now();
    f.server.db.snapshot().execute(r#"INSERT INTO "insuranceContracts"("contractID","shipID","ownerID","startDate","endDate",fraction,amount,payout,"isCorp")
        VALUES($1,$1,$2,$3,$4,0.5,100,500,0)"#,&[&(f.ship.id as i32),&(f.ship.pilot.char_id as i32),&(now-1),&(now+600_000_000)]).await.unwrap();
    let mut db=crate::db::connect(&f.server.db_url).await.unwrap();let name=format!("reject_pending_loss_{}",f.ship.id);
    f.server.db.snapshot().batch_execute(&format!(r#"CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        IF NEW."victimItemID"={} THEN RAISE EXCEPTION 'forced pending settlement failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER {name} BEFORE INSERT ON "killLog" FOR EACH ROW EXECUTE FUNCTION {name}();"#,f.ship.id)).await.unwrap();
    let result=finish(&f.server,&mut db,f.marker,f.ship.clone()).await;
    f.server.db.snapshot().batch_execute(&format!(r#"DROP TRIGGER {name} ON "killLog"; DROP FUNCTION {name}();"#)).await.unwrap();
    assert!(result.is_err());f.blocked().await;
    let raw:String=f.server.db.snapshot().query_one(r#"SELECT loss FROM "pendingStationArrivals" WHERE "shipID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().get(0);
    assert!(decode(&raw,f.marker).is_some());
    assert!(f.server.db.snapshot().query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(f.cargo as i32)]).await.unwrap().is_some());
    f.server.world=Default::default(); // No detached Ship or notification identity survives restart.
    retry(&f.server,&mut db).await;
    assert!(!blocked_character(&*f.server.db.snapshot(),f.ship.pilot.char_id).await.unwrap());
    let row=f.server.db.snapshot().query_one(r#"SELECT "shipID",balance FROM "character_" WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32)]).await.unwrap();
    let capsule=i64::from(row.get::<_,i32>(0));assert_ne!(capsule,f.ship.id);assert_eq!(row.get::<_,f64>(1),1500.0);
    retry(&f.server,&mut db).await;
    let repeated=finish(&f.server,&mut db,f.marker,f.ship.clone()).await.unwrap();assert_eq!(repeated.ship_id,capsule);
    assert_eq!(f.server.db.snapshot().query_one(r#"SELECT COUNT(*) FROM "killLog" WHERE "victimItemID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,i64>(0),1);
    assert_eq!(f.server.db.snapshot().query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32)]).await.unwrap().get::<_,f64>(0),1500.0);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn marker_without_a_final_snapshot_and_corrupt_loss_stay_quarantined_without_invented_outcomes() {
    let f=Fixture::new().await;let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    retry(&f.server,&mut db).await;f.blocked().await;
    assert!(f.server.world.lock().pending_station_arrivals.is_empty());
    f.server.db.snapshot().execute(r#"UPDATE "pendingStationArrivals" SET loss='{}' WHERE "shipID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap();
    retry(&f.server,&mut db).await;f.blocked().await;
    assert_eq!(f.server.db.snapshot().query_one(r#"SELECT "shipID" FROM "character_" WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32)]).await.unwrap().get::<_,i32>(0),f.ship.id as i32);
    assert!(f.server.db.snapshot().query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(f.cargo as i32)]).await.unwrap().is_some());
    assert_eq!(f.server.db.snapshot().query_one(r#"SELECT count(*) FROM "killLog" WHERE "victimItemID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,i64>(0),0);
    assert!(f.server.db.snapshot().query_opt(r#"SELECT 1 FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(f.ship.id as i32)]).await.unwrap().is_none(),"no guessed heal or death is persisted");
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn cancelled_finalization_releases_its_busy_claim_and_heartbeat_can_retry() {
    let f=Fixture::new().await;
    let blocker=crate::db::connect(&f.server.db_url).await.unwrap();blocker.batch_execute("BEGIN").await.unwrap();
    blocker.query_one(r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(f.ship.id as i32)]).await.unwrap();
    let pid:i32=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    let mut future=Box::pin(finish(&f.server,&mut db,f.marker,f.ship.clone()));
    tokio::select! {
        result=&mut future=>panic!("finalization passed a held hull lock: {result:?}"),
        waited=tokio::time::timeout(std::time::Duration::from_secs(10),async {loop {
            if f.server.db.snapshot().query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",&[&pid]).await.unwrap().get::<_,bool>(0) {break;}
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }})=>{waited.expect("finalization reaches the held hull lock");}
    }
    assert!(f.server.world.lock().pending_station_arrivals[&f.ship.id].busy);
    drop(future);
    assert!(!f.server.world.lock().pending_station_arrivals[&f.ship.id].busy);
    f.blocked().await;
    blocker.batch_execute("COMMIT").await.unwrap();
    retry(&f.server,&mut db).await;
    assert!(!blocked(&*f.server.db.snapshot(),f.ship.id).await.unwrap());
    assert!(f.server.world.lock().pending_station_arrivals.is_empty());
    f.cleanup().await;
}
