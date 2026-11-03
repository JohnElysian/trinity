use super::*;
use crate::combat::{Active,Action,Lock};

fn arm(f:&Fixture,enemy:i64) {
    let module=|id,target| Active {module_id:id,type_id:3651,effect_id:4,guid:"effects.Test".into(),offensive:true,
        warp_safe:false,disallow_warp_alignment:false,action:Action::Passive,target,ammo:None,duration_ms:1000,
        cap_need:0.0,first_cycle_prepaid:false,started_ms:crate::combat::now_ms(),next_ms:i64::MAX,cycles:1,
        repeat:1000,stopping:false,name:"Regression weapon".into(),reach:None,landed:true};
    let mut w=f.db.world.lock();let p=w.parks.get_mut(&30000142).unwrap();
    let mut attacker=f.ship.clone();attacker.id=enemy;attacker.pilot.char_id=enemy;attacker.client_id=0;
    let offense=attacker.fight.as_mut().unwrap();offense.locks.insert(f.ship.id,Lock::Locked);offense.modules.push(module(enemy+1,Some(f.ship.id)));offense.applied.insert(enemy+1);
    p.ships.insert(enemy,attacker);
    let defense=p.ships.get_mut(&f.ship.id).unwrap().fight.as_mut().unwrap();defense.locks.insert(enemy,Lock::Locked);defense.modules.push(module(enemy+2,None));defense.applied.insert(enemy+2);
    p.missiles.push(crate::npc::Missile {attacker:crate::npc::combat_ref::Attacker {ship_id:enemy,char_id:enemy,client_id:None,ship_type:587,weapon_type:3651},
        target:f.ship.id,impact_ms:i64::MAX,damage:[1.0,0.0,0.0,0.0],cloud:100.0,aoe_velocity:100.0,reaches:true,weapon:"Regression missile".into()});
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn refused_reserved_jump_keeps_live_combat_and_refunds_only_the_reserved_capacitor() {
    let f=Fixture::new().await;let enemy=f.db.id().await as i64;arm(&f,enemy);
    let blocker=crate::db::connect(&f.db.url).await.unwrap();
    let pid=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get::<_,i32>(0);
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker.query_one(r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&f.fuel]).await.unwrap();
    let server=f.server().await;let arrival=f.arrival();let quote=f.quote();
    let (result,())=tokio::join!(super::super::reserved::run(&server,&f.session,1,JumpRequest::Beacon {member:f.member,beacon:f.beacon},&arrival,&quote,"effects.JumpDriveIn"),async {
        wait_for_lock(&blocker,pid).await;
        {
            let mut w=f.db.world.lock();let p=w.parks.get_mut(&30000142).unwrap();
            let ship=p.ships.get_mut(&f.ship.id).expect("source must remain in combat until accepted");assert!(ship.identity_reserved);
            let fighter=ship.fight.as_mut().unwrap();fighter.vitals.hull_damage=17.0;
            assert!(fighter.vitals.capacitor.spend(crate::combat::now_ms(),50.0));
            assert_eq!(fighter.modules.len(),1);assert_eq!(fighter.locks.get(&enemy),Some(&Lock::Locked));
            assert_eq!(p.missiles.len(),1);
            w.parks.get_mut(&30000143).unwrap().beacons.remove(&f.beacon);
        }
        blocker.batch_execute("COMMIT").await.unwrap();
    });
    assert!(result.is_err());
    {
        let w=f.db.world.lock();let p=&w.parks[&30000142];let ship=&p.ships[&f.ship.id];let fight=ship.fight.as_ref().unwrap();
        assert!(!ship.identity_reserved);assert_eq!(fight.vitals.hull_damage,17.0);
        assert_eq!(fight.vitals.capacitor.at(crate::combat::now_ms()),950.0);
        assert_eq!(fight.modules.len(),1);assert_eq!(fight.locks.get(&enemy),Some(&Lock::Locked));
        assert_eq!(p.ships[&enemy].fight.as_ref().unwrap().locks.get(&f.ship.id),Some(&Lock::Locked));
        assert_eq!(p.ships[&enemy].fight.as_ref().unwrap().modules.len(),1);assert_eq!(p.missiles.len(),1);
        assert!(!w.parks[&30000143].ships.contains_key(&f.ship.id));
    }
    f.unchanged(&f.ship).await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn accepted_jump_publishes_latest_combat_damage_received_during_commit() {
    let f=Fixture::new().await;let enemy=f.db.id().await as i64;arm(&f,enemy);
    let key=f.db.id().await;let name=format!("travel_commit_{key}");
    f.db.db.batch_execute(&format!(r#"CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        IF NEW."itemID"={} AND NEW."locationID"=30000143 THEN PERFORM pg_advisory_xact_lock({key}::bigint); END IF; RETURN NEW; END $$;
        CREATE CONSTRAINT TRIGGER {name} AFTER UPDATE ON entity DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {name}();"#,f.ship.id)).await.unwrap();
    let blocker=crate::db::connect(&f.db.url).await.unwrap();let pid=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get::<_,i32>(0);
    blocker.query_one("SELECT pg_advisory_lock($1)",&[&(key as i64)]).await.unwrap();
    let server=f.server().await;let arrival=f.arrival();let quote=f.quote();
    let (result,())=tokio::join!(super::super::reserved::run(&server,&f.session,1,JumpRequest::Beacon {member:f.member,beacon:f.beacon},&arrival,&quote,"effects.JumpDriveIn"),async {
        wait_for_lock(&blocker,pid).await;
        {let mut w=f.db.world.lock();let p=w.parks.get_mut(&30000142).unwrap();
            let ship=p.ships.get_mut(&f.ship.id).expect("a committing jump remains in the source fight");assert!(ship.identity_reserved);
            ship.fight.as_mut().unwrap().vitals.hull_damage=19.0;assert_eq!(p.missiles.len(),1);}
        blocker.query_one("SELECT pg_advisory_unlock($1)",&[&(key as i64)]).await.unwrap();
    });
    result.unwrap();
    f.db.db.batch_execute(&format!("DROP TRIGGER {name} ON entity; DROP FUNCTION {name}();")).await.unwrap();
    {let w=f.db.world.lock();assert!(!w.parks[&30000142].ships.contains_key(&f.ship.id));
        assert!(w.pending_jobs.iter().any(|job|matches!(job,crate::heartbeat::Job::Refit {ship_id,..} if *ship_id==enemy)),"accepted removal retains other ships' modifier cleanup jobs");
        let ship=&w.parks[&30000143].ships[&f.ship.id];assert!(!ship.identity_reserved);assert_eq!(ship.fight.as_ref().unwrap().vitals.hull_damage,19.0);}
    let damage=f.db.db.query_one(r#"SELECT COALESCE("valueFloat","valueInt"::float8) FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,f64>(0);
    assert_eq!(damage,19.0,"destination checkpoint stores the latest accepted live snapshot");
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn cancelled_reserved_jump_releases_identity_and_restores_untouched_capacitor_curve() {
    let f=Fixture::new().await;let enemy=f.db.id().await as i64;arm(&f,enemy);
    let before=f.db.world.lock().parks[&30000142].ships[&f.ship.id].fight.as_ref().unwrap().vitals.capacitor;
    let blocker=crate::db::connect(&f.db.url).await.unwrap();
    let pid=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get::<_,i32>(0);
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker.query_one(r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&f.fuel]).await.unwrap();
    let server=f.server().await;let arrival=f.arrival();let quote=f.quote();
    let mut pending=Box::pin(super::super::reserved::run(&server,&f.session,1,JumpRequest::Beacon {member:f.member,beacon:f.beacon},&arrival,&quote,"effects.JumpDriveIn"));
    tokio::select! {
        result=&mut pending=>panic!("departure completed before held fuel lock: {result:?}"),
        ()=wait_for_lock(&blocker,pid)=>{},
    }
    assert!(f.db.world.lock().parks[&30000142].ships[&f.ship.id].identity_reserved);
    drop(pending);
    blocker.batch_execute("COMMIT").await.unwrap();
    {
        let w=f.db.world.lock();let p=&w.parks[&30000142];let ship=&p.ships[&f.ship.id];let fight=ship.fight.as_ref().unwrap();
        assert!(!ship.identity_reserved);assert_eq!(fight.vitals.capacitor,before);
        assert_eq!(fight.modules.len(),1);assert_eq!(fight.locks.get(&enemy),Some(&Lock::Locked));
        assert_eq!(p.ships[&enemy].fight.as_ref().unwrap().locks.get(&f.ship.id),Some(&Lock::Locked));
        assert_eq!(p.ships[&enemy].fight.as_ref().unwrap().modules.len(),1);assert_eq!(p.missiles.len(),1);
        assert!(!w.parks[&30000143].ships.contains_key(&f.ship.id));
    }
    f.unchanged(&f.ship).await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn lethal_damage_during_reserved_jump_payment_refuses_departure_without_erasing_combat() {
    let f=Fixture::new().await;let enemy=f.db.id().await as i64;arm(&f,enemy);
    let blocker=crate::db::connect(&f.db.url).await.unwrap();
    let pid=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get::<_,i32>(0);
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker.query_one(r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&f.fuel]).await.unwrap();
    let server=f.server().await;let arrival=f.arrival();let quote=f.quote();
    let (result,())=tokio::join!(super::super::reserved::run(&server,&f.session,1,JumpRequest::Beacon {member:f.member,beacon:f.beacon},&arrival,&quote,"effects.JumpDriveIn"),async {
        wait_for_lock(&blocker,pid).await;
        {
            let mut w=f.db.world.lock();let ship=w.parks.get_mut(&30000142).unwrap().ships.get_mut(&f.ship.id).unwrap();
            assert!(ship.identity_reserved);
            let vitals=&mut ship.fight.as_mut().unwrap().vitals;vitals.hull_damage=vitals.hull_max;
        }
        blocker.batch_execute("COMMIT").await.unwrap();
    });
    assert!(result.unwrap_err().to_string().contains("destroyed"));
    {
        let w=f.db.world.lock();let p=&w.parks[&30000142];let ship=&p.ships[&f.ship.id];let fight=ship.fight.as_ref().unwrap();
        assert!(!ship.identity_reserved);assert_eq!(fight.vitals.hull_damage,fight.vitals.hull_max);
        assert_eq!(fight.vitals.capacitor.at(crate::combat::now_ms()),1000.0);
        assert_eq!(fight.modules.len(),1);assert_eq!(fight.locks.get(&enemy),Some(&Lock::Locked));
        assert_eq!(p.ships[&enemy].fight.as_ref().unwrap().locks.get(&f.ship.id),Some(&Lock::Locked));assert_eq!(p.missiles.len(),1);
        assert!(!w.parks[&30000143].ships.contains_key(&f.ship.id));
    }
    f.unchanged(&f.ship).await;
}
