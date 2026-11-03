use super::*;
use crate::ballpark::{Command, Park, Vec3};
use crate::session::Session;
use std::sync::Arc;

pub(super) const FROM:i64=30000142;
pub(super) const TO:i64=30000143;

pub(super) struct Fixture {
    pub(super) db:crate::test_support::Database,
    pub(super) session:Session,
    pub(super) ship:i64,
    pub(super) module:i64,
    pub(super) fuel:i64,
    pub(super) member:i64,
    pub(super) beacon:i64,
    pub(super) fixed:i64,
}

impl Fixture {
    pub(super) async fn new(covert:bool)->Self {
        let db=crate::test_support::Database::load().await;
        let pilot=db.id().await;let corp=db.id().await;let alliance=db.id().await;let ship=db.id().await;
        let module=db.id().await;let fuel=db.id().await;let member=db.id().await;let beacon=db.id().await;let fixed=db.id().await;
        let ship_type=if covert {22428}else{11567};let module_type=if covert {28652}else{23953};
        db.db.execute(r#"INSERT INTO corporation ("corporationID","corporationName","allianceID") VALUES($1,$2,$3)"#,
            &[&corp,&format!("Portal corp {corp}"),&alliance]).await.unwrap();
        db.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","corporationID","shipID","solarSystemID","stationID") VALUES($1,$2,$3,$4,$5,0)"#,
            &[&pilot,&format!("Portal pilot {pilot}"),&corp,&ship,&(FROM as i32)]).await.unwrap();
        for (id,ty,owner,location,flag,quantity,singleton) in [
            (ship,ship_type,pilot,FROM as i32,0,1,1),(module,module_type,pilot,ship,27,1,1),(fuel,16275,pilot,ship,5,1500,0),
        ] {
            db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,$2,$3,$4,$5,$6,$7)"#,
                &[&id,&ty,&owner,&location,&flag,&quantity,&singleton]).await.unwrap();
        }
        db.db.execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt") VALUES($1,2,1)"#,&[&module]).await.unwrap();
        let fixed_type:i32=db.db.query_one(r#"SELECT "typeID" FROM "invTypes" WHERE "groupID"=838 ORDER BY "typeID" LIMIT 1"#,&[]).await.unwrap().get(0);
        db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,$2,$3,$4,0,1,1)"#,
            &[&fixed,&fixed_type,&corp,&(TO as i32)]).await.unwrap();
        db.db.execute(r#"INSERT INTO "posStructures" ("itemID",state) VALUES($1,4)"#,&[&fixed]).await.unwrap();
        let session=Session {char_id:Some(pilot as i64),corp_id:Some(corp as i64),alliance_id:Some(alliance as i64),
            ship_id:Some(ship as i64),solar_system_id:Some(FROM),solar_system_id2:Some(FROM),..Default::default()};
        let mut hull=crate::svc::inflight::ship_for_space(&db.ctx(&session),pilot as i64,ship as i64).await.unwrap();
        hull.fight.as_mut().unwrap().vitals.capacitor=Charged::full(10000.0,0.0,combat::now_ms());
        let mut source=Park::new(FROM,vec![],0);source.ships.insert(ship as i64,hull);
        let mut destination=Park::new(TO,vec![],0);
        destination.beacons.insert(beacon as i64,Beacon::new(beacon as i64,if covert {28650}else{21094},Some(member as i64),"Portal destination",Vec3::ZERO,1000.0,
            Kind::Cyno {char_id:member as i64,ship_id:member as i64+1}));
        {
            let mut w=db.world.lock();w.parks.insert(FROM,source);w.parks.insert(TO,destination);
            w.gangs.test_members(vec![pilot as i64,member as i64]);
        }
        Self {db,session,ship:ship as i64,module:module as i64,fuel:fuel as i64,member:member as i64,beacon:beacon as i64,fixed:fixed as i64}
    }
    pub(super) async fn open(&self)->Refused<()> {
        super::super::open_bridge(&self.db.ctx(&self.session),self.member,self.beacon,TO).await
    }
    pub(super) async fn fuel(&self)->i32 {
        self.db.db.query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#,&[&(self.fuel as i32)]).await.unwrap().get(0)
    }
    pub(super) fn cap(&self)->f64 {
        self.db.world.lock().parks[&FROM].ships[&self.ship].fight.as_ref().unwrap().vitals.capacitor.at(combat::now_ms())
    }
    pub(super) fn released(&self) {
        let w=self.db.world.lock();assert!(!w.pending_bridges.contains(&self.ship));assert!(!w.parks[&FROM].ships[&self.ship].identity_reserved);
    }
    async fn fuel_lock(&self)->(tokio_postgres::Client,i32) {
        let db=crate::db::connect(&self.db.url).await.unwrap();
        let pid=db.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
        db.batch_execute("BEGIN").await.unwrap();
        db.query_one(r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(self.fuel as i32)]).await.unwrap();
        (db,pid)
    }
}

pub(super) async fn blocked(db:&tokio_postgres::Client,pid:i32) {
    tokio::time::timeout(std::time::Duration::from_secs(15),async {
        loop {
            db.query_one("SELECT pg_stat_clear_snapshot()",&[]).await.unwrap();
            if db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",&[&pid]).await.unwrap().get::<_,bool>(0) {break;}
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.expect("portal reaches the held database lock");
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn portal_invalidations_during_fuel_wait_roll_back_and_release_reservations() {
    let empty=Fixture::new(false).await;
    empty.db.world.lock().parks.get_mut(&FROM).unwrap().ships.get_mut(&empty.ship).unwrap().fight.as_mut().unwrap().vitals.capacitor.set(combat::now_ms(),999.0);
    assert!(empty.open().await.is_err());assert_eq!(empty.fuel().await,1500);assert_eq!(empty.cap(),999.0);empty.released();
    for case in 0..3 {
        let f=Fixture::new(false).await;let (blocker,pid)=f.fuel_lock().await;
        let (result,())=tokio::join!(f.open(),async {
            blocked(&blocker,pid).await;
            assert_eq!(f.cap(),9000.0);
            {
                let mut w=f.db.world.lock();assert!(w.pending_bridges.contains(&f.ship));
                match case {
                    0=>{w.parks.get_mut(&TO).unwrap().beacons.remove(&f.beacon);},
                    1=>{crate::svc::gang::leave(&mut w,f.member).ok().unwrap();},
                    _=>{let fight=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap();fight.vitals.hull_damage=fight.vitals.hull_max;},
                }
            }
            blocker.batch_execute("COMMIT").await.unwrap();
        });
        assert!(result.is_err(),"invalidation case {case}");assert_eq!(f.fuel().await,1500);assert_eq!(f.cap(),10000.0);
        assert!(!f.db.world.lock().bridges.contains_key(&f.ship));f.released();
    }
    let f=Fixture::new(false).await;
    let blocker=crate::db::connect(&f.db.url).await.unwrap();let pid=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker.query_one(r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(f.module as i32)]).await.unwrap();
    let (result,())=tokio::join!(f.open(),async {
        blocked(&blocker,pid).await;
        {
            let mut w=f.db.world.lock();let p=w.parks.get_mut(&FROM).unwrap();
            p.command(f.ship,Command::Warp {target:Vec3::new(5e12,0.0,0.0),range:0.0});
            assert!(p.ships[&f.ship].warping());
        }
        blocker.batch_execute("COMMIT").await.unwrap();
    });
    assert!(result.is_err());assert_eq!(f.fuel().await,1500);assert_eq!(f.cap(),10000.0);f.released();
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn portal_commit_failure_duplicates_and_cancelled_waiter_preserve_one_payment() {
    let f=Arc::new(Fixture::new(false).await);
    let function=format!("portal_failure_{}",f.ship);let trigger=format!("portal_trigger_{}",f.ship);
    f.db.db.batch_execute(&format!(r#"CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced portal commit failure'; END $$;
        CREATE CONSTRAINT TRIGGER {trigger} AFTER UPDATE OR DELETE ON entity DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (OLD."itemID"={}) EXECUTE FUNCTION {function}()"#,f.fuel)).await.unwrap();
    assert!(f.open().await.is_err());assert_eq!(f.fuel().await,1500);assert_eq!(f.cap(),10000.0);f.released();
    f.db.db.batch_execute(&format!("DROP TRIGGER {trigger} ON entity; DROP FUNCTION {function}()")).await.unwrap();
    let (blocker,pid)=f.fuel_lock().await;
    let request={let f=f.clone();tokio::spawn(async move {f.open().await})};
    blocked(&blocker,pid).await;
    request.abort();assert!(request.await.unwrap_err().is_cancelled());
    blocker.batch_execute("COMMIT").await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(15),async {
        loop {if f.db.world.lock().bridges.contains_key(&f.ship) {break;}tokio::time::sleep(std::time::Duration::from_millis(10)).await;}
    }).await.unwrap();
    assert_eq!(f.fuel().await,1000);assert_eq!(f.cap(),9000.0);f.released();
    assert!(f.open().await.is_err());assert_eq!(f.fuel().await,1000);
    f.db.world.lock().bridges.remove(&f.ship);
    f.db.world.lock().portal_cycles.remove(&f.ship);
    let (a,b)=tokio::join!(f.open(),f.open());
    assert_ne!(a.is_ok(),b.is_ok());assert_eq!(f.fuel().await,500);assert_eq!(f.cap(),8000.0);f.released();
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn portal_revocation_during_commit_closes_accepted_start_and_fixed_beacon_locks_recheck_state() {
    let f=Fixture::new(false).await;
    let gate=crate::db::connect(&f.db.url).await.unwrap();let pid=gate.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    let key=f.ship as i32;
    gate.query_one("SELECT pg_advisory_lock(712,$1)",&[&key]).await.unwrap();
    let function=format!("portal_gate_{}",f.ship);let trigger=format!("portal_gate_trigger_{}",f.ship);
    f.db.db.batch_execute(&format!(r#"CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock(712,{key}); RETURN NEW; END $$;
        CREATE CONSTRAINT TRIGGER {trigger} AFTER UPDATE ON entity DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (OLD."itemID"={}) EXECUTE FUNCTION {function}()"#,f.fuel)).await.unwrap();
    let (result,())=tokio::join!(f.open(),async {
        blocked(&gate,pid).await;
        f.db.world.lock().parks.get_mut(&TO).unwrap().beacons.remove(&f.beacon);
        gate.query_one("SELECT pg_advisory_unlock(712,$1)",&[&key]).await.unwrap();
    });
    f.db.db.batch_execute(&format!("DROP TRIGGER {trigger} ON entity; DROP FUNCTION {function}()")).await.unwrap();
    assert!(result.is_ok(),"the accepted startup commits, but revoked destination cannot be published");
    assert_eq!(f.fuel().await,1000);assert_eq!(f.cap(),9000.0);
    assert!(!f.db.world.lock().bridges.contains_key(&f.ship));f.released();
    let blocker=crate::db::connect(&f.db.url).await.unwrap();let pid=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker.execute(r#"UPDATE "posStructures" SET state=1 WHERE "itemID"=$1"#,&[&(f.fixed as i32)]).await.unwrap();
    let ctx=f.db.ctx(&f.session);
    let (result,())=tokio::join!(super::super::open_bridge_structure(&ctx,f.fixed,TO),async {
        // The wrapper's early read sees the old state; the locked final read
        // must wait and recheck the committed offline state.
        blocked(&blocker,pid).await;blocker.batch_execute("COMMIT").await.unwrap();
    });
    assert!(result.is_err());assert_eq!(f.fuel().await,1000);assert_eq!(f.cap(),9000.0);f.released();
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn covert_portals_use_original_cost_duration_and_traveler_harmonics() {
    let normal=Fixture::new(false).await;
    normal.db.world.lock().parks.get_mut(&TO).unwrap().beacons.get_mut(&normal.beacon).unwrap().type_id=28650;
    assert!(normal.open().await.is_err());assert_eq!(normal.fuel().await,1500);assert_eq!(normal.cap(),10000.0);
    let f=Fixture::new(true).await;let before=combat::now_ms();
    f.open().await.unwrap();assert_eq!(f.cap(),9900.0);assert_eq!(f.fuel().await,1500,"original covert module has no activation-fuel attributes");
    {
        let w=f.db.world.lock();let bridge=&w.bridges[&f.ship];assert_eq!(bridge.harmonics,2);
        assert!(bridge.until_ms>=before+20000 && bridge.until_ms<=combat::now_ms()+20000);
    }
    let traveler=f.db.id().await;let ship=f.db.id().await;
    f.db.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","corporationID","shipID","solarSystemID","stationID") VALUES($1,$2,$3,$4,$5,0)"#,
        &[&traveler,&format!("Traveler {traveler}"),&(f.session.corp_id.unwrap() as i32),&ship,&(FROM as i32)]).await.unwrap();
    f.db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,587,$2,$3,0,1,1)"#,&[&ship,&traveler,&(FROM as i32)]).await.unwrap();
    let session=Session {char_id:Some(traveler as i64),ship_id:Some(ship as i64),..f.session.clone()};
    let hull=crate::svc::inflight::ship_for_space(&f.db.ctx(&session),traveler as i64,ship as i64).await.unwrap();
    {
        let mut w=f.db.world.lock();w.parks.get_mut(&FROM).unwrap().ships.insert(ship as i64,hull);
        let gang=w.gangs.of(f.session.char_id.unwrap()).unwrap();w.gangs.gangs.get_mut(&gang).unwrap().members.push(traveler as i64);
    }
    assert!(super::super::prepare_jump_through(&f.db.ctx(&session),traveler as i64,ship as i64,FROM,f.session.char_id.unwrap(),f.ship,false).await.is_err());
    // Original covertops hull 11182 has jumpHarmonics2; the ordinary frigate
    // must not inherit the portal's own permission.
    f.db.db.execute(r#"UPDATE entity SET "typeID"=11182 WHERE "itemID"=$1"#,&[&ship]).await.unwrap();
    let hull=crate::svc::inflight::ship_for_space(&f.db.ctx(&session),traveler as i64,ship as i64).await.unwrap();
    f.db.world.lock().parks.get_mut(&FROM).unwrap().ships.insert(ship as i64,hull);
    // Transit requires source isotope fuel, independently of activation.
    let drive=super::super::drive(&f.db.ctx(&f.session),f.session.char_id.unwrap(),f.ship).await.unwrap();
    let id=f.db.id().await;
    f.db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,$2,$3,$4,5,100000,0)"#,
        &[&id,&(drive.fuel as i32),&(f.session.char_id.unwrap() as i32),&(f.ship as i32)]).await.unwrap();
    super::super::prepare_jump_through(&f.db.ctx(&session),traveler as i64,ship as i64,FROM,f.session.char_id.unwrap(),f.ship,false).await.unwrap();
}
