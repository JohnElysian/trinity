use super::*;
use crate::{ballpark::{Celestial,Park},combat::{Action,Active,Charged,Lock},test_support::Database};
use std::sync::Arc;

struct Fixture {server:Arc<Server>,session:Session,ship:i64,station:i64,from:i64,from_gate:i64,to_gate:i64,to:Place,enemy:i64}
impl Fixture {
    async fn new()->Self {
        let db=Database::load().await;
        let pilot=db.id().await;let ship=db.id().await;let corp=db.id().await;let station=db.id().await;let enemy=db.id().await;
        let gate=db.db.query_one(r#"SELECT g."itemID",g."solarSystemID",g."constellationID",g."regionID",d."itemID",d."solarSystemID",d."constellationID",d."regionID",d.x,d.y,d.z,COALESCE(d.radius,0)::float8
            FROM "mapJumps" j JOIN "mapDenormalize" g ON g."itemID"=j."stargateID" JOIN "mapDenormalize" d ON d."itemID"=j."celestialID"
            WHERE g."groupID"=10 AND d."groupID"=10 AND g."solarSystemID"<>d."solarSystemID" ORDER BY g."itemID" LIMIT 1"#,&[]).await.unwrap();
        let from_gate=gate.get::<_,i32>(0) as i64;let from=gate.get::<_,i32>(1) as i64;let to_gate=gate.get::<_,i32>(4) as i64;
        let to=Place {station:None,system:gate.get::<_,i32>(5) as i64,constellation:gate.get::<_,i32>(6) as i64,region:gate.get::<_,i32>(7) as i64,
            at:space::arrival_point(Vec3::new(gate.get(8),gate.get(9),gate.get(10)),gate.get(11))};
        db.db.execute(r#"INSERT INTO corporation ("corporationID","corporationName") VALUES ($1,$2)"#,&[&corp,&format!("Travel corp {corp}")]).await.unwrap();
        db.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","corporationID","shipID","solarSystemID","stationID") VALUES ($1,$2,$3,$4,$5,0)"#,
            &[&pilot,&format!("Transition pilot {pilot}"),&corp,&ship,&(from as i32)]).await.unwrap();
        db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES ($1,587,$2,$3,0,1,1),($2,1373,$2,$3,57,1,1)"#,
            &[&ship,&pilot,&(from as i32)]).await.unwrap();
        db.db.execute(r#"INSERT INTO "staStations" ("stationID","stationTypeID","corporationID","solarSystemID","constellationID","regionID","stationName",x,y,z)
            VALUES ($1,1529,$2,$3,$4,$5,$6,0,0,0)"#,&[&station,&corp,&(from as i32),&gate.get::<_,i32>(2),&gate.get::<_,i32>(3),&format!("Transition station {station}")]).await.unwrap();
        let session=Session {char_id:Some(pilot as i64),ship_id:Some(ship as i64),corp_id:Some(corp as i64),solar_system_id:Some(from),solar_system_id2:Some(from),role:8,..Default::default()};
        let mut hull=inflight::ship_for_space(&db.ctx(&session),pilot as i64,ship as i64).await.unwrap();
        hull.pos=Vec3::new(2000.0,0.0,0.0);hull.fight.as_mut().unwrap().vitals.capacitor=Charged::full(1000.0,0.0,combat::now_ms());
        let body=|id,group|Celestial {id,type_id:1529,group_id:group,name:"Transition endpoint".into(),pos:Vec3::ZERO,radius:1000.0,jump:None};
        let mut park=Park::new(from,vec![body(station as i64,15),body(from_gate,10)],destiny::tick_now());
        let module=|id,target|Active {module_id:id,type_id:3651,effect_id:4,guid:"effects.Test".into(),offensive:true,warp_safe:false,disallow_warp_alignment:false,
            action:Action::Passive,target,ammo:None,duration_ms:1000,cap_need:0.0,first_cycle_prepaid:false,started_ms:combat::now_ms(),next_ms:i64::MAX,
            cycles:1,repeat:1000,stopping:false,name:"Travel test weapon".into(),reach:None,landed:true};
        let mut attacker=hull.clone();attacker.id=enemy as i64;attacker.client_id=0;attacker.pilot.char_id=enemy as i64;
        attacker.fight.as_mut().unwrap().locks.insert(ship as i64,Lock::Locked);
        attacker.fight.as_mut().unwrap().modules.push(module(enemy as i64+1,Some(ship as i64)));
        hull.fight.as_mut().unwrap().locks.insert(enemy as i64,Lock::Locked);
        hull.fight.as_mut().unwrap().modules.push(module(enemy as i64+2,None));
        park.ships.insert(ship as i64,hull);park.ships.insert(enemy as i64,attacker);
        park.missiles.push(crate::npc::Missile {attacker:crate::npc::combat_ref::Attacker {ship_id:enemy as i64,char_id:enemy as i64,client_id:None,ship_type:587,weapon_type:3651},
            target:ship as i64,impact_ms:i64::MAX,damage:[1.0,0.0,0.0,0.0],cloud:100.0,aoe_velocity:100.0,reaches:true,weapon:"Travel missile".into()});
        {let mut w=db.world.lock();w.parks.insert(from,park);w.parks.insert(to.system,Park::new(to.system,vec![],destiny::tick_now()));
            w.maintenance.insert(ship as i64,crate::svc::maintenance::Configuration {allow_fleet:true,pilot:pilot as i64,system:from});}
        let server=Arc::new(Server {table:Default::default(),db:crate::db::Shared::new(db.db,db.url.clone()),db_url:db.url,node_id:1,bulk:db.bulk,dogma:db.dogma,world:db.world,
            services:crate::svc::registry(),dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0});
        Self {server,session,ship:ship as i64,station:station as i64,from,from_gate,to_gate,to,enemy:enemy as i64}
    }
    fn request(&self,kind:u8)->Request {match kind {0=>Request::Gate {from_gate:self.from_gate,to_gate:self.to_gate,to:self.to},1=>Request::Dock {station:self.station,towed:false},_=>Request::Admin {to:Place{system:self.from,at:Vec3::new(9e7,3e7,1e7),..self.to}}}}
    async fn install_block(&self,deferred:bool,fail:bool)->(tokio_postgres::Client,i32,String) {
        let name=format!("transition_wait_{}",self.ship);let key=self.ship;
        self.server.db.snapshot().batch_execute(&format!(r#"CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
            IF NEW."itemID"={key} THEN PERFORM pg_advisory_xact_lock({key}::bigint); {} END IF; RETURN NEW; END $$;
            CREATE {} TRIGGER {name} {} UPDATE ON entity {} FOR EACH ROW EXECUTE FUNCTION {name}();"#,
            if fail {"RAISE EXCEPTION 'forced transition failure';"}else{""},if deferred {"CONSTRAINT"}else{""},if deferred {"AFTER"}else{"BEFORE"},
            if deferred {"DEFERRABLE INITIALLY DEFERRED"}else{""})).await.unwrap();
        let blocker=crate::db::connect(&self.server.db_url).await.unwrap();let pid=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
        blocker.query_one("SELECT pg_advisory_lock($1)",&[&key]).await.unwrap();(blocker,pid,name)
    }
    async fn unlock(&self,blocker:&tokio_postgres::Client) {blocker.query_one("SELECT pg_advisory_unlock($1)",&[&self.ship]).await.unwrap();}
    async fn cleanup(&self,name:&str) {self.server.db.snapshot().batch_execute(&format!("DROP TRIGGER {name} ON entity; DROP FUNCTION {name}();")).await.unwrap();}
    async fn unchanged(&self) {
        let row=self.server.db.snapshot().query_one(r#"SELECT c."stationID",c."solarSystemID",e."locationID",e.flag FROM "character_" c JOIN entity e ON e."itemID"=c."shipID" WHERE c."characterID"=$1"#,
            &[&(self.session.char_id.unwrap() as i32)]).await.unwrap();
        assert_eq!((row.get::<_,i32>(0),row.get::<_,i32>(1),row.get::<_,i32>(2),row.get::<_,i32>(3)),(0,self.from as i32,self.from as i32,0));
        let w=self.server.world.lock();let p=&w.parks[&self.from];let ship=&p.ships[&self.ship];let fight=ship.fight.as_ref().unwrap();
        assert!(!ship.identity_reserved);assert_eq!(fight.modules.len(),1);assert_eq!(fight.locks.get(&self.enemy),Some(&Lock::Locked));
        assert_eq!(p.ships[&self.enemy].fight.as_ref().unwrap().locks.get(&self.ship),Some(&Lock::Locked));assert_eq!(p.missiles.len(),1);
        assert!(w.maintenance[&self.ship].allow_fleet);
    }
}

async fn blocked(db:&tokio_postgres::Client,pid:i32) {
    tokio::time::timeout(Duration::from_secs(15),async {loop {
        db.query_one("SELECT pg_stat_clear_snapshot()",&[]).await.unwrap();
        if db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",&[&pid]).await.unwrap().get::<_,bool>(0) {break;}
        tokio::time::sleep(Duration::from_millis(10)).await;
    }}).await.expect("transition reaches held SQL lock");
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn refused_gate_dock_and_admin_transitions_keep_live_combat_and_damage() {
    for kind in 0..3 {
        let f=Fixture::new().await;let (blocker,pid,name)=f.install_block(false,true).await;
        let (result,())=tokio::join!(run(&f.server,&f.session,1,f.request(kind)),async {
            blocked(&blocker,pid).await;
            {let mut w=f.server.world.lock();let p=w.parks.get_mut(&f.from).unwrap();let ship=p.ships.get_mut(&f.ship).unwrap();assert!(ship.identity_reserved);
                let fight=ship.fight.as_mut().unwrap();fight.vitals.hull_damage=17.0;assert!(fight.vitals.capacitor.spend(combat::now_ms(),50.0));
                assert_eq!(fight.modules.len(),1);assert_eq!(p.missiles.len(),1);}
            f.unlock(&blocker).await;
        });
        assert!(result.is_err());f.cleanup(&name).await;f.unchanged().await;
        let w=f.server.world.lock();let fight=w.parks[&f.from].ships[&f.ship].fight.as_ref().unwrap();
        assert_eq!(fight.vitals.hull_damage,17.0);assert_eq!(fight.vitals.capacitor.at(combat::now_ms()),950.0);
    }
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn reserved_gate_and_dock_reject_late_range_death_and_cancellation() {
    for (kind,change) in [(0,0),(1,0),(0,1),(1,1),(0,2),(1,2)] {
        let f=Fixture::new().await;let (blocker,pid,name)=f.install_block(false,false).await;
        let mut pending=Box::pin(run(&f.server,&f.session,1,f.request(kind)));
        tokio::select! {result=&mut pending=>panic!("transition completed before SQL blocker: {}",result.is_ok()),()=blocked(&blocker,pid)=>{}}
        if change==2 {drop(pending);f.unlock(&blocker).await;}else{
            {let mut w=f.server.world.lock();let ship=w.parks.get_mut(&f.from).unwrap().ships.get_mut(&f.ship).unwrap();
                if change==0 {ship.pos=Vec3::new(1e8,0.0,0.0);}else{let v=&mut ship.fight.as_mut().unwrap().vitals;v.hull_damage=v.hull_max;}}
            f.unlock(&blocker).await;assert!(pending.await.is_err());
        }
        f.cleanup(&name).await;f.unchanged().await;
    }
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn accepted_gate_dock_and_admin_publish_latest_commit_damage_and_expected_location() {
    for kind in 0..3 {
        let f=Fixture::new().await;let (blocker,pid,name)=f.install_block(true,false).await;
        let (result,())=tokio::join!(run(&f.server,&f.session,1,f.request(kind)),async {
            blocked(&blocker,pid).await;
            {let mut w=f.server.world.lock();let ship=w.parks.get_mut(&f.from).unwrap().ships.get_mut(&f.ship).unwrap();assert!(ship.identity_reserved);
                ship.fight.as_mut().unwrap().vitals.hull_damage=19.0;}
            f.unlock(&blocker).await;
        });
        let arrived=result.unwrap();f.cleanup(&name).await;
        let row=f.server.db.snapshot().query_one(r#"SELECT c."stationID",c."solarSystemID",e."locationID",a."valueFloat" FROM "character_" c JOIN entity e ON e."itemID"=c."shipID"
            JOIN entity_attributes a ON a."itemID"=e."itemID" AND a."attributeID"=3 WHERE c."characterID"=$1"#,&[&(f.session.char_id.unwrap() as i32)]).await.unwrap();
        assert_eq!(row.get::<_,i32>(0) as i64,arrived.place.station.unwrap_or(0));assert_eq!(row.get::<_,i32>(1) as i64,arrived.place.system);
        assert_eq!(row.get::<_,i32>(2) as i64,arrived.place.station.unwrap_or(arrived.place.system));assert_eq!(row.get::<_,f64>(3),19.0);
        let w=f.server.world.lock();if kind==1 {assert!(!w.parks[&f.from].ships.contains_key(&f.ship));assert!(!w.maintenance.contains_key(&f.ship));}
        else {let hull=&w.parks[&arrived.place.system].ships[&f.ship];assert!(!hull.identity_reserved);assert_eq!(hull.pos,arrived.place.at);
            assert_eq!(hull.fight.as_ref().unwrap().vitals.hull_damage,19.0);assert_eq!(hull.arrived_through,if kind==0 {Some(f.to_gate)}else{None});
            if kind==2 {assert!(w.maintenance[&f.ship].allow_fleet);}else{assert!(!w.maintenance.contains_key(&f.ship));}}
    }
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn accepted_owned_transition_finishes_when_its_caller_is_cancelled() {
    let f=Fixture::new().await;let (blocker,pid,name)=f.install_block(true,false).await;
    let server=f.server.clone();let session=f.session.clone();let request=f.request(0);
    let caller=tokio::spawn(async move {tokio::spawn(async move {run(&server,&session,1,request).await}).await});
    blocked(&blocker,pid).await;caller.abort();assert!(caller.await.unwrap_err().is_cancelled());
    f.unlock(&blocker).await;
    tokio::time::timeout(Duration::from_secs(15),async {loop {
        if f.server.world.lock().parks[&f.to.system].ships.get(&f.ship).is_some_and(|s|!s.identity_reserved) {break;}
        tokio::time::sleep(Duration::from_millis(10)).await;
    }}).await.unwrap();
    // The owned task may still be doing its fresh destination checkpoint.
    let row=f.server.db.snapshot().query_one(r#"SELECT "solarSystemID" FROM "character_" WHERE "characterID"=$1"#,&[&(f.session.char_id.unwrap() as i32)]).await.unwrap();
    assert_eq!(row.get::<_,i32>(0) as i64,f.to.system);
    f.cleanup(&name).await;
    assert!(!f.server.world.lock().parks[&f.from].ships.contains_key(&f.ship));
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn definitive_commit_failures_resolve_before_releasing_gate_station_and_admin_reservations() {
    for kind in 0..3 {
        let f=Fixture::new().await;let(blocker,pid,name)=f.install_block(true,true).await;
        let(result,())=tokio::join!(tokio::time::timeout(Duration::from_secs(15),run(&f.server,&f.session,1,f.request(kind))),async {
            blocked(&blocker,pid).await;
            {let mut world=f.server.world.lock();let ship=world.parks.get_mut(&f.from).unwrap().ships.get_mut(&f.ship).unwrap();
                assert!(ship.identity_reserved);ship.fight.as_mut().unwrap().vitals.hull_damage=23.0;}
            f.unlock(&blocker).await;
        });
        assert!(matches!(result.expect("definitive rollback is resolved"),Err(Failure::Refused(_))));
        f.cleanup(&name).await;f.unchanged().await;
        assert!(!crate::travel::blocked_character_in_memory(&f.server.world,f.session.char_id.unwrap()));
        assert!(!crate::losses::pending::blocked_character(&*f.server.db.snapshot(),f.session.char_id.unwrap()).await.unwrap());
        assert_eq!(f.server.world.lock().parks[&f.from].ships[&f.ship].fight.as_ref().unwrap().vitals.hull_damage,23.0);
    }
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn failed_space_final_snapshot_stays_detached_until_durable_retry() {
    let f=Fixture::new().await;let name=format!("reject_final_motion_{}",f.ship);
    f.server.db.snapshot().batch_execute(&format!(r#"CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        IF NEW."itemID"={} THEN RAISE EXCEPTION 'forced final motion failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER {name} BEFORE INSERT ON "shipSpaceMotion" FOR EACH ROW EXECUTE FUNCTION {name}();"#,f.ship)).await.unwrap();
    f.server.world.lock().parks.get_mut(&f.from).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.hull_damage=29.0;
    let result=run(&f.server,&f.session,1,f.request(0)).await;
    f.server.db.snapshot().batch_execute(&format!("DROP TRIGGER {name} ON \"shipSpaceMotion\"; DROP FUNCTION {name}();")).await.unwrap();
    assert!(matches!(result,Err(Failure::Pending(_))));
    assert!(crate::travel::blocked_character(&*f.server.db.snapshot(),f.session.char_id.unwrap()).await.unwrap());
    {let world=f.server.world.lock();assert!(!world.parks[&f.from].ships.contains_key(&f.ship));assert!(!world.parks[&f.to.system].ships.contains_key(&f.ship));}
    let mut db=crate::db::connect(&f.server.db_url).await.unwrap();crate::travel::retry(&f.server,&mut db).await;
    assert!(!crate::travel::blocked_character(&db,f.session.char_id.unwrap()).await.unwrap());
    assert!(!f.server.world.lock().parks[&f.to.system].ships.contains_key(&f.ship),"retry never publishes an old connection's captured hull");
    assert_eq!(db.query_one(r#"SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(f.ship as i32)]).await.unwrap().get::<_,f64>(0),29.0);
    assert!(db.query_opt(r#"SELECT 1 FROM "shipSpaceMotion" WHERE "itemID"=$1 AND "solarSystemID"=$2"#,&[&(f.ship as i32),&(f.to.system as i32)]).await.unwrap().is_some());
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn delayed_healthy_arrival_preserves_reconnected_motion_and_newer_boarding() {
    let f=Fixture::new().await;
    let captured=f.server.world.lock().parks[&f.from].ships[&f.ship].clone();
    run(&f.server,&f.session,1,f.request(0)).await.unwrap();
    let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    let row=db.query_one(r#"SELECT generation,"characterID","shipID","shipTypeID",source,destination FROM "travelReceipts" WHERE "shipID"=$1 ORDER BY generation DESC LIMIT 1"#,&[&(f.ship as i32)]).await.unwrap();
    let marker=crate::travel::Marker {generation:row.get(0),pilot:i64::from(row.get::<_,i32>(1)),ship:i64::from(row.get::<_,i32>(2)),ship_type:i64::from(row.get::<_,i32>(3)),
        source:serde_json::from_str(row.get::<_,&str>(4)).unwrap(),destination:serde_json::from_str(row.get::<_,&str>(5)).unwrap()};
    let pos=Vec3::new(7000.0,8000.0,9000.0);let vel=Vec3::new(4.0,5.0,6.0);
    {let mut world=f.server.world.lock();let ship=world.parks.get_mut(&f.to.system).unwrap().ships.get_mut(&f.ship).unwrap();ship.pos=pos;ship.vel=vel;ship.client_id=55;}
    let profile=inflight::profile(&db,&f.server.dogma,marker.pilot,marker.ship,&Default::default(),combat::now_ms()).await.unwrap().unwrap();
    super::super::completion::finish_space(&f.server,&mut db,marker,captured.clone(),profile,Some(f.to_gate)).await.unwrap();
    {let world=f.server.world.lock();let ship=&world.parks[&f.to.system].ships[&f.ship];assert_eq!(ship.pos,pos);assert_eq!(ship.vel,vel);assert_eq!(ship.client_id,55);}
    let mut current=f.session.clone();current.solar_system_id=Some(f.to.system);current.solar_system_id2=Some(f.to.system);
    let newer=Place {at:Vec3::new(9e8,8e8,7e8),..f.to};
    run(&f.server,&current,55,Request::Admin {to:newer}).await.unwrap();
    let profile=inflight::profile(&db,&f.server.dogma,marker.pilot,marker.ship,&Default::default(),combat::now_ms()).await.unwrap().unwrap();
    f.server.world.lock().parks.get_mut(&f.to.system).unwrap().ships.remove(&f.ship);
    assert!(matches!(super::super::completion::finish_space(&f.server,&mut db,marker,captured.clone(),profile,Some(f.to_gate)).await,Err(Failure::Pending(_))),
        "same-hull same-system later relocation also supersedes the old generation");
    assert!(!f.server.world.lock().parks[&f.to.system].ships.contains_key(&f.ship));
    assert_eq!(db.query_one(r#"SELECT x FROM entity WHERE "itemID"=$1"#,&[&(f.ship as i32)]).await.unwrap().get::<_,f64>(0),newer.at.x);
    let replacement=db.query_one("SELECT nextval('dyn_id_seq')::integer",&[]).await.unwrap().get::<_,i32>(0);
    db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,587,$2,$3,0,1,1)"#,
        &[&replacement,&(marker.pilot as i32),&(f.to.system as i32)]).await.unwrap();
    let profile=inflight::profile(&db,&f.server.dogma,marker.pilot,marker.ship,&Default::default(),combat::now_ms()).await.unwrap().unwrap();
    let mut writer=crate::db::connect(&f.server.db_url).await.unwrap();let tx=writer.transaction().await.unwrap();
    tx.execute(r#"UPDATE "character_" SET "shipID"=$2 WHERE "characterID"=$1"#,&[&(marker.pilot as i32),&replacement]).await.unwrap();
    let mut pending=std::pin::pin!(super::super::completion::finish_space(&f.server,&mut db,marker,captured,profile,Some(f.to_gate)));
    assert!(tokio::time::timeout(Duration::from_millis(50),&mut pending).await.is_err());
    f.server.world.lock().parks.get_mut(&f.to.system).unwrap().ships.remove(&f.ship);
    tx.commit().await.unwrap();assert!(matches!(pending.await,Err(Failure::Pending(_))));
    assert!(!f.server.world.lock().parks[&f.to.system].ships.contains_key(&f.ship));
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn docking_stops_active_cloak_at_publication_and_persists_original_deadlines() {
    let f=Fixture::new().await;
    let module=f.server.db.snapshot().query_one("SELECT nextval('dyn_id_seq')::integer",&[]).await.unwrap().get::<_,i32>(0);
    f.server.db.snapshot().execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES ($1,11370,$2,$3,11,1,1)"#,
        &[&module,&(f.session.char_id.unwrap() as i32),&(f.ship as i32)]).await.unwrap();
    f.server.db.snapshot().execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt") VALUES ($1,2,1)"#,&[&module]).await.unwrap();
    {let mut w=f.server.world.lock();let p=w.parks.get_mut(&f.from).unwrap();p.ships.remove(&f.enemy);p.missiles.clear();
        let ship=p.ships.get_mut(&f.ship).unwrap();ship.arrive(Vec3::new(1e8,0.0,0.0));let fight=ship.fight.as_mut().unwrap();fight.locks.clear();fight.modules.clear();}
    let ctx=crate::services::Ctx {bulk:&f.server.bulk,dogma:&f.server.dogma,session:&f.session,db:&*f.server.db.snapshot(),
        db_url:&f.server.db_url,node_id:1,world:&f.server.world,client_id:1};
    let activate=crate::services::Call {service:"dogmaIM".into(),method:"Activate".into(),args:vec![Value::Int(module as i64),Value::bytes("cloaking"),Value::None,Value::Int(0)],kwargs:vec![]};
    assert!(matches!(inflight::dogma_im(&activate,&ctx).await,Some(crate::services::Outcome::Value(_))));
    let (blocker,pid,name)=f.install_block(true,false).await;
    let (result,before)=tokio::join!(run(&f.server,&f.session,1,Request::Dock {station:f.station,towed:true}),async {
        blocked(&blocker,pid).await;
        {let w=f.server.world.lock();let ship=&w.parks[&f.from].ships[&f.ship];assert!(ship.cloaked);assert!(ship.identity_reserved);}
        let before=combat::now_ms();f.unlock(&blocker).await;before
    });
    let arrived=result.unwrap();f.cleanup(&name).await;
    let history=crate::cloak::durability::load_history(&*f.server.db.snapshot(),f.session.char_id.unwrap(),f.ship,combat::now_ms()).await.unwrap();
    assert!(history.module_ready[&(module as i64)]>before);assert!(history.targeting_at>before);
    let docked=Session {station_id:arrived.place.station,solar_system_id:None,..f.session.clone()};
    let ctx=crate::services::Ctx {session:&docked,..ctx};
    let loaded=inflight::ship_for_space(&ctx,f.session.char_id.unwrap(),f.ship).await.unwrap();
    assert!(!loaded.cloaked);assert!(loaded.fight.as_ref().unwrap().modules.is_empty());assert_eq!(loaded.fight.as_ref().unwrap().cloak_history,history);
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn same_system_admin_teleport_refreshes_owner_ego_and_keeps_maintenance_preference() {
    let f=Fixture::new().await;
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (peer,accepted)=tokio::join!(tokio::net::TcpStream::connect(listener.local_addr().unwrap()),listener.accept());
    let _peer=peer.unwrap();let (socket,address)=accepted.unwrap();
    let mut conn=crate::conn::Conn::new(socket,address,f.server.clone(),0);assert_eq!(conn.client_id,1);conn.session=f.session.clone();
    let mut events={let mut w=f.server.world.lock();let events=w.connect(1);w.set_presence(1,conn.presence());events};
    let at=Vec3::new(9e7,3e7,1e7);
    conn.teleport(None,f.from,0,0,at).await.unwrap();
    fn contains_set_state(value:&Value)->bool {value.as_string().as_deref()==Some("SetState") || value.seq().is_some_and(|seq|seq.iter().any(contains_set_state))}
    let mut reset=false;
    while let Ok(push)=events.try_recv() {if let crate::world::Push::Notify(n)=push {if n.event.starts_with("DoDestiny") && contains_set_state(&n.args) {reset=true;}}}
    assert!(reset,"same-system teleport must reset the owner's original native ego state");
    let w=f.server.world.lock();assert_eq!(w.parks[&f.from].ships[&f.ship].pos,at);assert!(w.maintenance[&f.ship].allow_fleet);
    assert_eq!(conn.session.solar_system_id,Some(f.from));assert_eq!(conn.session.ship_id,Some(f.ship));
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn lethal_docking_during_commit_settles_destroyed_hull_before_station_session() {
    let f=Fixture::new().await;let (blocker,pid,name)=f.install_block(true,false).await;
    let (result,())=tokio::join!(run(&f.server,&f.session,1,f.request(1)),async {
        blocked(&blocker,pid).await;
        {let mut w=f.server.world.lock();let v=&mut w.parks.get_mut(&f.from).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals;
            // A lower active maximum must not become healthy when the inactive
            // arrival profile restores the original hull maximum.
            v.hull_max=10.0;v.hull_damage=11.0;}
        f.unlock(&blocker).await;
    });
    let arrived=result.unwrap();f.cleanup(&name).await;
    assert_ne!(arrived.ship_id,f.ship);assert_eq!(arrived.place.station,Some(f.station));assert!(arrived.docking.is_none());
    assert!(f.server.db.snapshot().query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(f.ship as i32)]).await.unwrap().is_none());
    let row=f.server.db.snapshot().query_one(r#"SELECT e."typeID",e."locationID",c."shipID" FROM "character_" c JOIN entity e ON e."itemID"=c."shipID" WHERE c."characterID"=$1"#,
        &[&(f.session.char_id.unwrap() as i32)]).await.unwrap();
    assert_eq!(row.get::<_,i32>(0),670);assert_eq!(row.get::<_,i32>(1) as i64,f.station);assert_eq!(row.get::<_,i32>(2) as i64,arrived.ship_id);
    assert_eq!(f.server.db.snapshot().query_one(r#"SELECT count(*) FROM "killLog" WHERE "victimItemID"=$1"#,&[&(f.ship as i32)]).await.unwrap().get::<_,i64>(0),1);
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn lethal_admin_capsule_arrival_uses_medical_home_and_records_original_fight_system() {
    let f=Fixture::new().await;
    let destination=f.server.db.snapshot().query_one("SELECT nextval('dyn_id_seq')::integer",&[]).await.unwrap().get::<_,i32>(0);
    f.server.db.snapshot().execute(r#"INSERT INTO "staStations" ("stationID","stationTypeID","corporationID","solarSystemID","constellationID","regionID","stationName",x,y,z)
        VALUES ($1,1529,$2,$3,$4,$5,$6,0,0,0)"#,&[&destination,&(f.session.corp_id.unwrap() as i32),&(f.to.system as i32),&(f.to.constellation as i32),&(f.to.region as i32),&format!("Remote transition station {destination}")]).await.unwrap();
    f.server.db.snapshot().execute(r#"UPDATE entity SET "typeID"=670 WHERE "itemID"=$1"#,&[&(f.ship as i32)]).await.unwrap();
    crate::svc::medical::replace_clone(&*f.server.db.snapshot(),f.session.char_id.unwrap(),crate::svc::medical::TYPE_CLONE_ALPHA,Some(f.station)).await.unwrap();
    {let mut w=f.server.world.lock();let ship=w.parks.get_mut(&f.from).unwrap().ships.get_mut(&f.ship).unwrap();ship.pilot.type_id=670;ship.pilot.group_id=29;}
    let request=Request::Admin {to:Place {station:Some(destination as i64),..f.to}};
    let (blocker,pid,name)=f.install_block(true,false).await;
    let (result,())=tokio::join!(run(&f.server,&f.session,1,request),async {
        blocked(&blocker,pid).await;
        {let mut w=f.server.world.lock();let v=&mut w.parks.get_mut(&f.from).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals;
            v.hull_damage=v.hull_max;}
        f.unlock(&blocker).await;
    });
    let arrived=result.unwrap();f.cleanup(&name).await;
    assert_ne!(arrived.ship_id,f.ship);assert_eq!(arrived.place.station,Some(f.station));assert_eq!(arrived.place.system,f.from);
    let row=f.server.db.snapshot().query_one(r#"SELECT "solarSystemID","isPodKill" FROM "killLog" WHERE "victimItemID"=$1"#,&[&(f.ship as i32)]).await.unwrap();
    assert_eq!(row.get::<_,i32>(0) as i64,f.from);assert_eq!(row.get::<_,i16>(1),1);
    assert!(!crate::losses::pending::blocked_character(&*f.server.db.snapshot(),f.session.char_id.unwrap()).await.unwrap());
    assert!(f.server.world.lock().pending_station_arrivals.is_empty());
}
