use super::*;
use crate::ballpark::{Command,Park,Vec3};
use crate::combat::{self,Charged};
use crate::services::Call;
use crate::session::Session;

struct Fixture {
    db: crate::test_support::Database,
    session: Session,
    system: i64,
    ship: i64,
    shield: i64,
    ab: i64,
    cyno: i64,
    doom: i64,
    fuel: i64,
}

impl Fixture {
    async fn new() -> Self {
        let db=crate::test_support::Database::load().await;
        let pilot=db.id().await; let ship=db.id().await;
        let shield=db.id().await;let ab=db.id().await;let cyno=db.id().await;let doom=db.id().await;let fuel=db.id().await;
        let system:i32=db.db.query_one(r#"SELECT "solarSystemID" FROM "mapSolarSystems" WHERE security<0.45 ORDER BY "solarSystemID" LIMIT 1"#,&[]).await.unwrap().get(0);
        db.db.execute(r#"INSERT INTO "character_"("characterID","characterName","shipID","solarSystemID","stationID") VALUES($1,$2,$3,$4,0)"#,&[&pilot,&format!("Warp effects {pilot}"),&ship,&system]).await.unwrap();
        db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES
            ($1,587,$2,$3,0,1,1),($4,399,$2,$1,19,1,1),($5,439,$2,$1,20,1,1),
            ($6,21096,$2,$1,11,1,1),($7,24550,$2,$1,12,1,1),($8,16274,$2,$1,5,150000,0)"#,
            &[&ship,&pilot,&system,&shield,&ab,&cyno,&doom,&fuel]).await.unwrap();
        db.db.execute(r#"INSERT INTO entity_attributes("itemID","attributeID","valueInt") SELECT unnest($1::integer[]),2,1"#,&[&vec![shield,ab,cyno,doom]]).await.unwrap();
        let session=Session {char_id:Some(pilot as i64),ship_id:Some(ship as i64),solar_system_id:Some(system as i64),solar_system_id2:Some(system as i64),..Default::default()};
        let mut hull=super::super::ship_for_space(&db.ctx(&session),pilot as i64,ship as i64).await.unwrap();
        hull.stats.warp_capacitor_need=0.0;
        hull.fight.as_mut().unwrap().vitals.capacitor=Charged::full(100000.0,0.0,combat::now_ms());
        let mut park=Park::new(system as i64,vec![],0);park.ships.insert(ship as i64,hull);
        db.world.lock().parks.insert(system as i64,park);
        Self {db,session,system:system as i64,ship:ship as i64,shield:shield as i64,ab:ab as i64,cyno:cyno as i64,doom:doom as i64,fuel:fuel as i64}
    }

    fn align(&self) {
        let mut world=self.db.world.lock();let park=world.parks.get_mut(&self.system).unwrap();let hull=park.ships.get_mut(&self.ship).unwrap();
        hull.arrive(Vec3::ZERO);hull.vel=Vec3::new(hull.stats.max_velocity,0.0,0.0);
        park.command(self.ship,Command::Warp {target:Vec3::new(5e12,0.0,0.0),range:0.0});
    }

    fn advance(&self) {
        let mut world=self.db.world.lock();let park=world.parks.get_mut(&self.system).unwrap();park.advance(park.tick+1);
    }

    fn call(&self,item:i64,name:&str)->Call {
        Call {service:"dogmaIM".into(),method:"Activate".into(),args:vec![eve_marshal::Value::Int(item),eve_marshal::Value::bytes(name),eve_marshal::Value::None,eve_marshal::Value::Int(0)],kwargs:vec![]}
    }

    async fn fuel(&self)->i32 {
        self.db.db.query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#,&[&(self.fuel as i32)]).await.unwrap().get(0)
    }

    fn capacitor(&self)->f64 {
        self.db.world.lock().parks[&self.system].ships[&self.ship].fight.as_ref().unwrap().vitals.capacitor.at(combat::now_ms())
    }
}

async fn wait_blocked(blocker:&tokio_postgres::Client,pid:i32) {
    tokio::time::timeout(std::time::Duration::from_secs(10),async {
        loop {
            blocker.simple_query("SELECT pg_stat_clear_snapshot()").await.unwrap();
            if blocker.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",&[&pid]).await.unwrap().get::<_,bool>(0) {break;}
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await.unwrap();
}

#[tokio::test]
#[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
async fn original_warp_safe_flags_allow_repairs_and_recheck_warp_after_database_wait() {
    let f=Fixture::new().await;
    let ctx=f.db.ctx(&f.session);
    for (name,safe) in [("shieldBoosting",true),("armorRepair",true),("powerBooster",true),("cloakingWarpSafe",true),("cloaking",false),("speedBoostMassAddition",false)] {
        assert_eq!(super::super::effect_by_name(&ctx,name).await.unwrap().warp_safe,safe);
    }
    f.align();f.advance();assert!(f.db.world.lock().parks[&f.system].ships[&f.ship].in_warp());
    assert!(matches!(super::super::activate(&ctx,&f.call(f.shield,"shieldBoosting")).await,Outcome::Value(_)));
    assert!(f.db.world.lock().parks[&f.system].ships[&f.ship].fight.as_ref().unwrap().module(f.shield).unwrap().warp_safe);
    assert!(matches!(super::super::activate(&ctx,&f.call(f.ab,"speedBoostMassAddition")).await,Outcome::UserError{..}));
    assert!(matches!(super::super::activate(&ctx,&f.call(f.cyno,"cynosuralGeneration")).await,Outcome::UserError{..}));
    assert_eq!(f.fuel().await,150000);
    f.align();
    let blocker=crate::db::connect(&f.db.url).await.unwrap();
    let pid:i32=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    blocker.batch_execute("BEGIN; LOCK TABLE \"dgmTypeAttributes\" IN ACCESS EXCLUSIVE MODE").await.unwrap();
    let call=f.call(f.ab,"speedBoostMassAddition");
    let (outcome,())=tokio::join!(super::super::activate(&ctx,&call),async {
        wait_blocked(&blocker,pid).await;f.advance();
        assert!(f.db.world.lock().parks[&f.system].ships[&f.ship].in_warp());
        blocker.batch_execute("ROLLBACK").await.unwrap();
    });
    assert!(matches!(outcome,Outcome::UserError{..}));
    assert!(f.db.world.lock().parks[&f.system].ships[&f.ship].fight.as_ref().unwrap().module(f.ab).is_none());
}

#[tokio::test]
#[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
async fn original_cyno_effect_burns_fuel_once_creates_field_and_refuses_alignment() {
    let f=Fixture::new().await;let ctx=f.db.ctx(&f.session);
    let fuel=f.db.id().await;
    f.db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,16273,$2,$3,5,1000,0)"#,
        &[&fuel,&(f.session.char_id.unwrap() as i32),&(f.ship as i32)]).await.unwrap();
    let call=f.call(f.cyno,"cynosuralGeneration");
    assert_eq!(super::super::effect_by_name(&ctx,"cynosuralGeneration").await.unwrap().id,2857);
    f.align();
    assert!(matches!(super::super::activate(&ctx,&call).await,Outcome::UserError{..}));
    let quantity=||async {f.db.db.query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#,&[&fuel]).await.unwrap().get::<_,i32>(0)};
    assert_eq!(quantity().await,1000);
    f.db.world.lock().parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap().arrive(Vec3::ZERO);
    assert!(matches!(super::super::activate(&ctx,&call).await,Outcome::Value(_)));
    assert_eq!(quantity().await,500);
    {
        let world=f.db.world.lock();let park=&world.parks[&f.system];
        let active=park.ships[&f.ship].fight.as_ref().unwrap().module(f.cyno).unwrap();
        assert_eq!(active.action,crate::combat::Action::Cyno);assert!(active.disallow_warp_alignment);assert_eq!(active.cycles,1);
        assert_eq!(park.beacons.values().filter(|b|matches!(b.kind,crate::beacons::Kind::Cyno {ship_id,..} if ship_id==f.ship)).count(),1);
    }
    assert!(matches!(super::super::activate(&ctx,&call).await,Outcome::Value(_)));
    assert_eq!(quantity().await,500);
    f.align();f.advance();
    assert!(!f.db.world.lock().parks[&f.system].ships[&f.ship].warping());
}

#[tokio::test]
#[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
async fn funded_activation_reserves_motion_and_capacitor_rolls_back_failures_and_fires_once() {
    let f=Fixture::new().await;let ctx=f.db.ctx(&f.session);let call=f.call(f.doom,"empWaveGrid");
    {
        let mut world=f.db.world.lock();let ship=world.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap();
        ship.fight.as_mut().unwrap().vitals.capacitor.set(combat::now_ms(),1000.0);
    }
    assert!(matches!(super::super::activate(&ctx,&call).await,Outcome::UserError{..}));
    assert_eq!(f.capacitor(),1000.0);assert_eq!(f.fuel().await,150000);
    {
        let mut world=f.db.world.lock();world.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.capacitor.set(combat::now_ms(),60000.0);
    }
    f.align();
    let blocker=crate::db::connect(&f.db.url).await.unwrap();
    let pid:i32=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker.query_one(r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(f.fuel as i32)]).await.unwrap();
    let (outcome,())=tokio::join!(super::super::activate(&ctx,&call),async {
        wait_blocked(&blocker,pid).await;
        assert_eq!(f.capacitor(),32500.0);
        assert!(f.db.world.lock().parks[&f.system].ships[&f.ship].identity_reserved);
        f.advance();
        assert!(!f.db.world.lock().parks[&f.system].ships[&f.ship].in_warp());
        {
            let mut world=f.db.world.lock();let fight=world.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap();
            fight.vitals.capacitor.add(combat::now_ms(),-1000.0);
            fight.vitals.armor_damage=7.0;fight.vitals.hull_damage=fight.vitals.hull_max;
        }
        blocker.batch_execute("COMMIT").await.unwrap();
    });
    assert!(matches!(outcome,Outcome::UserError{..}));
    assert_eq!(f.capacitor(),59000.0);assert_eq!(f.fuel().await,150000);
    {
        let mut world=f.db.world.lock();let ship=world.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap();
        assert!(!ship.identity_reserved);let fight=ship.fight.as_mut().unwrap();
        assert!(fight.modules.is_empty());assert_eq!(fight.vitals.armor_damage,7.0);fight.vitals.hull_damage=0.0;
    }
    f.align();
    // A deferred trigger fails COMMIT, after all live checks and staged debit.
    // With no intervening action, rollback must restore the entire original
    // recharge curve, including energy that recharged during the SQL wait.
    let recharge_curve = {
        let mut curve=Charged::full(100000.0,1000.0,combat::now_ms());
        curve.set(combat::now_ms(),59000.0);
        f.db.world.lock().parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.capacitor=curve;
        curve
    };
    let function=format!("test_funded_failure_{}",f.fuel);
    let trigger=format!("test_funded_trigger_{}",f.fuel);
    f.db.db.batch_execute(&format!(r#"CREATE FUNCTION {function}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(0.15); RAISE EXCEPTION 'forced funded commit failure'; END $$;
        CREATE CONSTRAINT TRIGGER {trigger} AFTER UPDATE OR DELETE ON entity DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN (OLD."itemID"={}) EXECUTE FUNCTION {function}()"#,f.fuel)).await.unwrap();
    assert!(matches!(super::super::activate(&ctx,&call).await,Outcome::UserError{..}));
    assert_eq!(f.db.world.lock().parks[&f.system].ships[&f.ship].fight.as_ref().unwrap().vitals.capacitor,recharge_curve);
    assert!(f.capacitor()>recharge_curve.at(combat::now_ms()-100));
    assert_eq!(f.fuel().await,150000);
    assert!(!f.db.world.lock().parks[&f.system].ships[&f.ship].identity_reserved);
    f.db.db.batch_execute(&format!("DROP TRIGGER {trigger} ON entity; DROP FUNCTION {function}()")).await.unwrap();
    let victim=f.ship+1000000000;
    let mut notifications=f.db.world.lock().connect(1);
    {
        let mut world=f.db.world.lock();let park=world.parks.get_mut(&f.system).unwrap();
        std::sync::Arc::make_mut(&mut park.wildlife).security=0.3;
        let mut curve=Charged::full(100000.0,0.0,combat::now_ms());curve.set(combat::now_ms(),59000.0);
        park.ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.capacitor=curve;
        let mut target=park.ships[&f.ship].clone();target.id=victim;target.client_id=2;target.pilot.char_id=victim;target.pilot.corp_id=2;target.arrive(Vec3::new(1000.0,0.0,0.0));
        let fight=target.fight.as_mut().unwrap();fight.vitals.shield=Charged::full(100000.0,0.0,combat::now_ms());fight.vitals.resonance=[[1.0;4];3];
        park.ships.insert(victim,target);
    }
    assert!(matches!(super::super::activate(&ctx,&call).await,Outcome::Value(_)));
    assert_eq!(f.capacitor(),31500.0);assert_eq!(f.fuel().await,75000);
    let started;
    {
        let world=f.db.world.lock();let park=&world.parks[&f.system];let ship=&park.ships[&f.ship];
        assert!(!ship.identity_reserved);let module=ship.fight.as_ref().unwrap().module(f.doom).unwrap();
        assert_eq!(module.cycles,1);assert!(!module.first_cycle_prepaid);started=module.clone();
        assert_eq!(park.ships[&victim].fight.as_ref().unwrap().vitals.shield.at(combat::now_ms()),53125.0);
        assert!(world.pending_jobs.iter().any(|job| matches!(job,crate::heartbeat::Job::Security {char_id,delta} if *char_id==f.session.char_id.unwrap() && *delta == -0.1)));
    }
    let mut damage_notice=false;let mut capacitor_notice=false;
    while let Ok(push)=notifications.try_recv() {
        if let crate::world::Push::Notify(note)=push {
            damage_notice |= note.event=="OnDamageMessage";
            capacitor_notice |= note.event=="OnModuleAttributeChanges";
        }
    }
    assert!(damage_notice);assert!(capacitor_notice);
    f.advance(); // The aligned warp proceeds only after the paid volley fired.
    {
        let mut world=f.db.world.lock();let park=world.parks.get_mut(&f.system).unwrap();let mut out=crate::heartbeat::Out::default();
        crate::fight::funded_first_cycle(park,f.ship,f.doom,started.next_ms,&mut out);
        assert!(park.ships[&f.ship].fight.as_ref().unwrap().module(f.doom).is_none());
        assert_eq!(park.ships[&victim].fight.as_ref().unwrap().vitals.shield.at(combat::now_ms()),53125.0);
    }
    assert_eq!(f.capacitor(),31500.0);
}
