use super::*;
use std::collections::HashSet;
use crate::{test_support::Database,session::Session};

struct Fixture { db:Database, session:Session, pilot:i32, ship:i32 }
impl Fixture {
    async fn new()->Self {
        let db=Database::load().await;
        let pilot=db.id().await;let ship=db.id().await;
        let place=db.db.query_one(r#"SELECT "stationID","solarSystemID" FROM "staStations" ORDER BY "stationID" LIMIT 1"#,&[]).await.unwrap();
        let station:i32=place.get(0);let system:i32=place.get(1);
        db.db.execute(r#"INSERT INTO "character_"("characterID","characterName","shipID","stationID","solarSystemID") VALUES($1,$2,$3,$4,$5)"#,&[&pilot,&format!("Booster fixture {pilot}"),&ship,&station,&system]).await.unwrap();
        db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,1373,$1,$3,57,1,1),($2,587,$1,$3,4,1,1)"#,&[&pilot,&ship,&station]).await.unwrap();
        Self{db,session:Session{char_id:Some(pilot as i64),ship_id:Some(ship as i64),station_id:Some(station as i64),solar_system_id2:Some(system as i64),..Default::default()},pilot,ship}
    }
    async fn item(&self,kind:i32,location:i32,flag:i32,quantity:i32,singleton:i32)->i32 {
        let id=self.db.id().await;
        self.db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,$2,$3,$4,$5,$6,$7)"#,&[&id,&kind,&self.pilot,&location,&flag,&quantity,&singleton]).await.unwrap();id
    }
    async fn train(&self,level:i64) {
        let skill=self.item(BIOLOGY,self.pilot,7,1,1).await;
        self.db.db.execute(r#"INSERT INTO entity_attributes("itemID","attributeID","valueInt") VALUES($1,280,$2)"#,&[&skill,&level]).await.unwrap();
    }
    async fn consume(&self,id:i32)->Outcome {consume(&self.db.ctx(&self.session),&[Value::Int(id as i64)]).await}
    async fn count(&self,id:i32)->i32 {self.db.db.query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#,&[&id]).await.unwrap().get(0)}
    async fn expire_now(&self) {
        let now=crate::bulkdata::filetime_now();
        self.db.db.execute("UPDATE character_boosters SET started=$2::bigint-2,expires=$2::bigint-1 WHERE character=$1",&[&self.pilot,&now]).await.unwrap();
    }
    async fn cleanup(self) {
        self.db.db.execute(r#"DELETE FROM entity_attributes WHERE "itemID" IN(SELECT "itemID" FROM entity WHERE "ownerID"=$1)"#,&[&self.pilot]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM entity WHERE "ownerID"=$1"#,&[&self.pilot]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "character_" WHERE "characterID"=$1"#,&[&self.pilot]).await.unwrap();
    }
}

fn success(out:&Outcome)->bool {matches!(out,Outcome::Value(Value::None))}
fn refusal(out:Outcome,needle:&str)->bool {matches!(out,Outcome::UserError{args,..} if args.iter().any(|(_,v)|v.as_string().is_some_and(|s|s.contains(needle))))}

async fn wait_for_blocker(db:&Client,blocker:i32) {
    tokio::time::timeout(std::time::Duration::from_secs(10),async {loop {
        if db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",&[&blocker]).await.unwrap().get::<_,bool>(0) {break;}
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }}).await.expect("operation reaches the competing row lock");
}

#[test]
fn original_biology_duration_and_supported_effects_are_bounded() {
    assert_eq!(expiry(100,1_800_000.0,20.0,0),Some(18_000_000_100));
    assert_eq!(expiry(100,1_800_000.0,20.0,1),Some(21_600_000_100));
    assert_eq!(expiry(100,1_800_000.0,20.0,5),Some(36_000_000_100));
    assert_eq!(expiry(i64::MAX-1,1_800_000.0,20.0,5),None);
    for invalid in [0.0,-1.0,f64::NAN,f64::INFINITY] {assert_eq!(expiry(100,invalid,20.0,1),None);}
    for unsupported in [9947,9950,28670,28674,28676] {assert_eq!(modifier(unsupported),None);}
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn original_synth_effects_consume_expire_and_reload_without_later_modifier_contamination() {
    let f=Fixture::new().await;f.train(5).await;
    let gun=f.item(484,f.ship,27,1,1).await;
    let launcher=f.item(499,f.ship,28,1,1).await;
    let missile=f.item(211,f.ship,28,10,0).await;
    for module in [gun,launcher] {f.db.db.execute(r#"INSERT INTO entity_attributes("itemID","attributeID","valueInt") VALUES($1,2,1)"#,&[&module]).await.unwrap();}
    for (kind,target,attribute,factor) in [(28672,missile,654,0.97),(28678,gun,54,1.03),(28680,f.ship,482,1.03),(28682,f.ship,37,1.03),(28684,gun,158,1.03)] {
        let before=crate::dogma::Loadout::load(&f.db.db,&f.db.dogma,f.pilot as i64,Some(f.ship as i64),&HashSet::new()).await.unwrap().snapshot();
        let stack=f.item(kind,f.session.station_id.unwrap() as i32,4,2,0).await;
        let result=f.consume(stack).await;assert!(success(&result),"type {kind} consumed");assert_eq!(f.count(stack).await,1);
        let row=f.db.db.query_one("SELECT item,expires-started,expires FROM character_boosters WHERE character=$1",&[&f.pilot]).await.unwrap();
        let active:i32=row.get(0);assert_ne!(active,stack);assert_eq!(row.get::<_,i64>(1),36_000_000_000);
        let stamp:i64=f.db.db.query_one(r#"SELECT "valueInt" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=1088"#,&[&active]).await.unwrap().get(0);
        assert_eq!(stamp,row.get::<_,i64>(2));
        let fresh=crate::db::connect(&f.db.url).await.unwrap();
        let after=crate::dogma::Loadout::load(&fresh,&f.db.dogma,f.pilot as i64,Some(f.ship as i64),&HashSet::new()).await.unwrap().snapshot();
        let baseline=before[&(target as i64)][&attribute];assert!(baseline>0.0);
        assert!((after[&(target as i64)][&attribute]/baseline-factor).abs()<1e-9,"original effect {kind} applies exactly once");
        // Other period-specific roles must not leak from the later catalogue.
        for other in [51,64,164,165,166,167,168] {
            for id in [f.pilot,gun] {assert_eq!(before[&(id as i64)].get(&other),after[&(id as i64)].get(&other));}
        }
        let call=crate::services::Call{service:"dogmaIM".into(),method:"CharGetInfo".into(),args:vec![],kwargs:vec![]};
        let info=super::super::dogma::dogma_im(&call,&f.db.ctx(&f.session)).await.unwrap();
        assert!(matches!(info,Outcome::Value(Value::Dict(ref rows)) if rows.iter().any(|(id,_)|*id==Value::Int(active as i64))));
        fn contains_exact_expiry(value:&Value,stamp:i64)->bool {
            match value {
                Value::Dict(pairs)=>pairs.iter().any(|(key,value)|(*key==Value::Int(1088) && *value==Value::Long(stamp)) || contains_exact_expiry(value,stamp)),
                Value::List(values)|Value::Tuple(values)=>values.iter().any(|v|contains_exact_expiry(v,stamp)),
                Value::Instance{state,..}=>contains_exact_expiry(state,stamp),_=>false,
            }
        }
        assert!(matches!(info,Outcome::Value(ref value) if contains_exact_expiry(value,stamp)),"original expiryTime remains exact FILETIME beyond f64 integer precision");
        f.expire_now().await;
        let expired=crate::dogma::Loadout::load(&fresh,&f.db.dogma,f.pilot as i64,Some(f.ship as i64),&HashSet::new()).await.unwrap().snapshot();
        assert_eq!(expired[&(target as i64)][&attribute],baseline,"expired effects are absent before cleanup/reconnect");
        let info=super::super::dogma::dogma_im(&call,&f.db.ctx(&f.session)).await.unwrap();
        assert!(matches!(info,Outcome::Value(Value::Dict(ref rows)) if rows.iter().all(|(id,_)|*id!=Value::Int(active as i64))));
        let mut worker=crate::db::connect(&f.db.url).await.unwrap();expire(&mut worker,&f.db.world).await.unwrap();
        assert!(f.db.db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&active]).await.unwrap().is_none());
    }
    // A legacy/injected unsupported booster must not inherit the later
    // catalogue's duplicate Crash or learning/ROF mappings.
    for (kind,slot) in [(9947,3),(15457,1)] {
        let before=crate::dogma::Loadout::load(&f.db.db,&f.db.dogma,f.pilot as i64,Some(f.ship as i64),&HashSet::new()).await.unwrap().snapshot();
        let active=f.item(kind,f.pilot,88,1,1).await;let now=crate::bulkdata::filetime_now();
        f.db.db.execute("INSERT INTO character_boosters(item,character,type_id,slot,started,expires) VALUES($1,$2,$3,$4,$5,$6)",&[&active,&f.pilot,&kind,&slot,&now,&(now+18_000_000_000)]).await.unwrap();
        let after=crate::dogma::Loadout::load(&f.db.db,&f.db.dogma,f.pilot as i64,Some(f.ship as i64),&HashSet::new()).await.unwrap().snapshot();
        for id in [f.pilot,f.ship,gun,missile] {assert_eq!(before[&(id as i64)],after[&(id as i64)],"unsupported original type {kind} receives no fabricated later modifiers");}
        f.expire_now().await;let mut worker=crate::db::connect(&f.db.url).await.unwrap();expire(&mut worker,&f.db.world).await.unwrap();
    }
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn consumption_checks_skills_slots_custody_and_rolls_back_failed_split() {
    let f=Fixture::new().await;let station=f.session.station_id.unwrap() as i32;
    let stack=f.item(28682,station,4,2,0).await;
    assert!(refusal(f.consume(stack).await,"You need skill"));assert_eq!(f.count(stack).await,2);
    f.train(1).await;
    f.db.db.execute(r#"INSERT INTO "pendingShipLosses"("shipID","characterID","solarSystemID","shipTypeID",occupied,"createdAt",loss) VALUES($1,$2,$3,587,true,0,'{}')"#,
        &[&f.ship,&f.pilot,&(f.session.solar_system_id2.unwrap() as i32)]).await.unwrap();
    assert!(refusal(f.consume(stack).await,"ship loss"),"direct service call observes the durable ordinary-loss quarantine");
    assert_eq!(f.count(stack).await,2);
    f.db.db.execute(r#"DELETE FROM "pendingShipLosses" WHERE "shipID"=$1"#,&[&f.ship]).await.unwrap();
    for kind in [9947,9950,28670,28674,28676] {let item=f.item(kind,station,4,1,0).await;assert!(refusal(f.consume(item).await,"not implemented"));assert_eq!(f.count(item).await,1);}
    let remote=f.item(28682,station+1,4,1,0).await;assert!(refusal(f.consume(remote).await,"accessible"));
    let foreign=f.item(28682,station,4,1,0).await;
    f.db.db.execute(r#"UPDATE entity SET "ownerID"=1 WHERE "itemID"=$1"#,&[&foreign]).await.unwrap();
    assert!(refusal(f.consume(foreign).await,"not your"));
    f.db.db.execute(r#"DELETE FROM entity WHERE "itemID"=$1"#,&[&foreign]).await.unwrap();
    let guard=format!("booster_fail_{}",f.pilot);
    f.db.db.batch_execute(&format!(r#"ALTER TABLE entity ADD CONSTRAINT {guard} CHECK("ownerID"<>{} OR flag<>88) NOT VALID"#,f.pilot)).await.unwrap();
    assert!(!success(&f.consume(stack).await));
    f.db.db.batch_execute(&format!("ALTER TABLE entity DROP CONSTRAINT {guard}")).await.unwrap();
    assert_eq!(f.count(stack).await,2,"failed active-item insert restores split source");
    let ctx=f.db.ctx(&f.session);let args=[Value::Int(stack as i64)];
    let (first,second)=tokio::join!(consume(&ctx,&args),consume(&ctx,&args));
    assert_ne!(success(&first),success(&second));
    let rejected=if success(&first){second}else{first};
    assert!(matches!(rejected,Outcome::UserError{msg,..} if msg=="OnlyOneBoosterActive"));
    assert_eq!(f.count(stack).await,1);
    assert_eq!(f.db.db.query_one("SELECT count(*) FROM character_boosters WHERE character=$1",&[&f.pilot]).await.unwrap().get::<_,i64>(0),1);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn consumption_and_expiry_revalidate_items_moved_while_waiting() {
    let f=Fixture::new().await;f.train(1).await;
    let station=f.session.station_id.unwrap() as i32;
    let item=f.item(28682,station,4,1,0).await;
    let mut blocker=crate::db::connect(&f.db.url).await.unwrap();let tx=blocker.transaction().await.unwrap();
    tx.query_one(r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&item]).await.unwrap();
    let pid:i32=tx.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    let release=async {wait_for_blocker(&f.db.db,pid).await;tx.execute(r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,&[&item,&(station+1)]).await.unwrap();tx.commit().await.unwrap();};
    let (out,())=tokio::join!(f.consume(item),release);assert!(refusal(out,"accessible"));
    f.db.db.execute(r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,&[&item,&station]).await.unwrap();
    assert!(success(&f.consume(item).await));f.expire_now().await;
    let tx=blocker.transaction().await.unwrap();tx.query_one(r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&item]).await.unwrap();
    let release=async {wait_for_blocker(&f.db.db,pid).await;tx.execute(r#"UPDATE entity SET "locationID"=$2,flag=4 WHERE "itemID"=$1"#,&[&item,&station]).await.unwrap();tx.commit().await.unwrap();};
    let mut worker=crate::db::connect(&f.db.url).await.unwrap();
    let (out,())=tokio::join!(expire(&mut worker,&f.db.world),release);out.unwrap();
    assert_eq!(f.count(item).await,1,"stale expiry cannot destroy moved inventory");
    assert!(f.db.db.query_opt("SELECT 1 FROM character_boosters WHERE item=$1",&[&item]).await.unwrap().is_none());
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn live_refresh_waits_for_identity_reservation_preserves_damage_and_recovers_expiry() {
    let mut f=Fixture::new().await;f.train(1).await;
    let system=f.session.solar_system_id2.unwrap();
    f.db.db.execute(r#"UPDATE "character_" SET "stationID"=0 WHERE "characterID"=$1"#,&[&f.pilot]).await.unwrap();
    f.db.db.execute(r#"UPDATE entity SET "locationID"=$2,flag=0 WHERE "itemID"=$1"#,&[&f.ship,&(system as i32)]).await.unwrap();
    f.session.station_id=None;f.session.solar_system_id=Some(system);
    let mut ship=super::super::inflight::ship_for_space(&f.db.ctx(&f.session),f.pilot as i64,f.ship as i64).await.unwrap();
    let speed=ship.stats.max_velocity;ship.identity_reserved=true;ship.fight.as_mut().unwrap().vitals.hull_damage=7.0;
    let mut park=crate::ballpark::Park::new(system,vec![],crate::destiny::tick_now());park.ships.insert(f.ship as i64,ship);f.db.world.lock().parks.insert(system,park);
    let booster=f.item(28682,f.ship,5,1,0).await;assert!(success(&f.consume(booster).await));
    assert!(f.db.db.query_opt("SELECT 1 FROM character_booster_refresh WHERE character=$1",&[&f.pilot]).await.unwrap().is_some());
    {let mut w=f.db.world.lock();let ship=w.parks.get_mut(&system).unwrap().ships.get_mut(&(f.ship as i64)).unwrap();assert_eq!(ship.stats.max_velocity,speed);ship.identity_reserved=false;}
    refresh::character(&f.db.db,&f.db.dogma,&f.db.world,f.pilot as i64).await.unwrap();
    {let w=f.db.world.lock();let ship=&w.parks[&system].ships[&(f.ship as i64)];assert!((ship.stats.max_velocity-speed*1.03).abs()<1e-9);assert_eq!(ship.fight.as_ref().unwrap().vitals.hull_damage,7.0);}
    let restored=super::super::inflight::ship_for_space(&f.db.ctx(&f.session),f.pilot as i64,f.ship as i64).await.unwrap();assert!((restored.stats.max_velocity-speed*1.03).abs()<1e-9);
    f.expire_now().await;let mut worker=crate::db::connect(&f.db.url).await.unwrap();expire(&mut worker,&f.db.world).await.unwrap();
    refresh::character(&f.db.db,&f.db.dogma,&f.db.world,f.pilot as i64).await.unwrap();
    {let w=f.db.world.lock();let ship=&w.parks[&system].ships[&(f.ship as i64)];assert_eq!(ship.stats.max_velocity,speed);assert_eq!(ship.fight.as_ref().unwrap().vitals.hull_damage,7.0);}
    assert!(f.db.db.query_opt("SELECT 1 FROM character_booster_refresh WHERE character=$1",&[&f.pilot]).await.unwrap().is_none());
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn expiry_refresh_survives_committed_location_reserved_source_and_detached_travel() {
    let mut f=Fixture::new().await;f.train(1).await;
    let from=f.session.solar_system_id2.unwrap();
    f.db.db.execute(r#"UPDATE "character_" SET "stationID"=0 WHERE "characterID"=$1"#,&[&f.pilot]).await.unwrap();
    f.db.db.execute(r#"UPDATE entity SET "locationID"=$2,flag=0 WHERE "itemID"=$1"#,&[&f.ship,&(from as i32)]).await.unwrap();
    f.session.station_id=None;f.session.solar_system_id=Some(from);
    let mut ship=super::super::inflight::ship_for_space(&f.db.ctx(&f.session),f.pilot as i64,f.ship as i64).await.unwrap();
    let speed=ship.stats.max_velocity;ship.fight.as_mut().unwrap().vitals.hull_damage=9.0;
    let mut park=crate::ballpark::Park::new(from,vec![],crate::destiny::tick_now());park.ships.insert(f.ship as i64,ship);f.db.world.lock().parks.insert(from,park);
    let booster=f.item(28682,f.ship,5,1,0).await;assert!(success(&f.consume(booster).await));
    {let mut w=f.db.world.lock();let ship=w.parks.get_mut(&from).unwrap().ships.get_mut(&(f.ship as i64)).unwrap();assert!((ship.stats.max_velocity-speed*1.03).abs()<1e-9);ship.identity_reserved=true;}
    let row=f.db.db.query_one(r#"SELECT "solarSystemID","constellationID","regionID" FROM "mapSolarSystems" WHERE "solarSystemID"<>$1 ORDER BY "solarSystemID" LIMIT 1"#,&[&(from as i32)]).await.unwrap();
    let destination=crate::travel::Place{system:i64::from(row.get::<_,i32>(0)),station:None,constellation:i64::from(row.get::<_,i32>(1)),region:i64::from(row.get::<_,i32>(2)),at:crate::ballpark::Vec3::ZERO};
    let source=crate::travel::Place{system:from,..destination};
    // The first COMMIT moves SQL custody, while the old source hull remains
    // reserved and damageable. Its destination park does not exist yet.
    f.db.db.execute(r#"UPDATE "character_" SET "solarSystemID"=$2 WHERE "characterID"=$1"#,&[&f.pilot,&(destination.system as i32)]).await.unwrap();
    f.db.db.execute(r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,&[&f.ship,&(destination.system as i32)]).await.unwrap();
    f.expire_now().await;let mut worker=crate::db::connect(&f.db.url).await.unwrap();expire(&mut worker,&f.db.world).await.unwrap();
    let pending=async||f.db.db.query_opt("SELECT 1 FROM character_booster_refresh WHERE character=$1",&[&f.pilot]).await.unwrap().is_some();
    refresh::character(&f.db.db,&f.db.dogma,&f.db.world,f.pilot as i64).await.unwrap();
    assert!(pending().await,"reserved source identity retains expiry work despite committed destination custody");
    let mut captured=f.db.world.lock().parks.get_mut(&from).unwrap().ships.remove(&(f.ship as i64)).unwrap();
    let tx=worker.transaction().await.unwrap();let marker=crate::travel::begin(&tx,f.pilot as i64,f.ship as i64,source,destination).await.unwrap();tx.commit().await.unwrap();
    crate::travel::track_uncertain(&f.db.world,&marker).unwrap();
    f.db.db.execute(r#"UPDATE "travelReceipts" SET state=1 WHERE generation=$1"#,&[&marker.generation]).await.unwrap();
    refresh::character(&f.db.db,&f.db.dogma,&f.db.world,f.pilot as i64).await.unwrap();
    assert!(pending().await,"in-memory unresolved completion retains expiry work with no published hull");
    f.db.db.execute(r#"UPDATE "travelReceipts" SET state=0 WHERE generation=$1"#,&[&marker.generation]).await.unwrap();
    crate::travel::clear_uncertain(&f.db.world,&marker);
    refresh::character(&f.db.db,&f.db.dogma,&f.db.world,f.pilot as i64).await.unwrap();
    assert!(pending().await,"durable pending travel retains expiry work even without its in-memory reservation");
    f.db.db.execute(r#"UPDATE "travelReceipts" SET state=1 WHERE generation=$1"#,&[&marker.generation]).await.unwrap();
    f.db.db.execute(r#"INSERT INTO "pendingShipLogouts"("shipID","characterID","solarSystemID","createdAt") VALUES($1,$2,$3,$4)"#,
        &[&f.ship,&f.pilot,&(destination.system as i32),&crate::combat::now_ms()]).await.unwrap();
    refresh::character(&f.db.db,&f.db.dogma,&f.db.world,f.pilot as i64).await.unwrap();
    assert!(pending().await,"a detached final logout snapshot must not discard deferred expiry work");
    f.db.db.execute(r#"DELETE FROM "pendingShipLogouts" WHERE "shipID"=$1"#,&[&f.ship]).await.unwrap();
    captured.identity_reserved=false;captured.arrive(destination.at);
    let mut park=crate::ballpark::Park::new(destination.system,vec![],crate::destiny::tick_now());park.ships.insert(f.ship as i64,captured);f.db.world.lock().parks.insert(destination.system,park);
    refresh::character(&f.db.db,&f.db.dogma,&f.db.world,f.pilot as i64).await.unwrap();
    assert!(!pending().await,"published destination consumes the repair only after applying the current profile");
    {let w=f.db.world.lock();let ship=&w.parks[&destination.system].ships[&(f.ship as i64)];assert_eq!(ship.stats.max_velocity,speed);assert_eq!(ship.fight.as_ref().unwrap().vitals.hull_damage,9.0);}
    f.db.db.execute(r#"DELETE FROM "travelReceipts" WHERE generation=$1"#,&[&marker.generation]).await.unwrap();
    f.cleanup().await;
}
