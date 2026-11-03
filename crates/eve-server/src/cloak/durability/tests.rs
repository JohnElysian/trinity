use super::*;
use crate::{ballpark::Park,combat,items::ItemTxn,services::{Call,Outcome},session::Session,svc::{inflight,inventory::{Container,CONTAINER_HANGAR},moves},test_support::Database};
use eve_marshal::Value;

struct Fixture {db:Database,session:Session,ship:Ship,module:i64,station:i64,other:i64}
impl Fixture {
    async fn new()->Self {
        let db=Database::load().await;
        let pilot=db.id().await;let hull=db.id().await;let module=db.id().await;let other=db.id().await;
        let row=db.db.query_one(r#"SELECT "stationID","solarSystemID" FROM "staStations" ORDER BY "stationID" LIMIT 1"#,&[]).await.unwrap();
        let station=i64::from(row.get::<_,i32>(0));let system=i64::from(row.get::<_,i32>(1));
        db.db.execute(r#"INSERT INTO "character_"("characterID","characterName","shipID","solarSystemID","stationID") VALUES($1,$2,$3,$4,0)"#,
            &[&pilot,&format!("Cooldown {pilot}"),&hull,&(system as i32)]).await.unwrap();
        db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES
            ($1,587,$2,$3,0,1,1),($4,11370,$2,$1,11,1,1),($5,587,$2,$3,0,1,1)"#,
            &[&hull,&pilot,&(system as i32),&module,&other]).await.unwrap();
        db.db.execute(r#"INSERT INTO entity_attributes("itemID","attributeID","valueInt") VALUES($1,2,1)"#,&[&module]).await.unwrap();
        let session=Session {char_id:Some(pilot as i64),ship_id:Some(hull as i64),solar_system_id:Some(system),solar_system_id2:Some(system),..Default::default()};
        let ship=inflight::ship_for_space(&db.ctx(&session),pilot as i64,hull as i64).await.unwrap();
        let mut park=Park::new(system,vec![],crate::destiny::tick_now());park.ships.insert(ship.id,ship);
        db.world.lock().parks.insert(system,park);
        for method in ["Activate","Deactivate"] {
            let call=Call {service:"dogmaIM".into(),method:method.into(),args:vec![Value::Int(module as i64),Value::bytes("cloaking"),Value::None,Value::Int(0)],kwargs:vec![]};
            assert!(matches!(inflight::dogma_im(&call,&db.ctx(&session)).await,Some(Outcome::Value(_))),"{method}");
        }
        let ship=db.world.lock().parks[&system].ships[&(hull as i64)].clone();
        assert!(ship.fight.as_ref().unwrap().cloak_history.module_ready[&(module as i64)]>combat::now_ms());
        Self {db,session,ship,module:module as i64,station,other:other as i64}
    }
    fn history(&self)->History {self.ship.fight.as_ref().unwrap().cloak_history.clone()}
    fn docked(&self)->Session {Session {station_id:Some(self.station),solar_system_id:None,..self.session.clone()}}
    async fn save(&self) {
        let mut db=crate::db::connect(&self.db.url).await.unwrap();let tx=db.transaction().await.unwrap();
        save_history(&tx,&self.ship,combat::now_ms()).await.unwrap();tx.commit().await.unwrap();
    }
    async fn move_module(&self,docked:&Session)->Outcome {
        let call=Call {service:"invcontainer".into(),method:"Add".into(),args:vec![Value::Int(self.module),Value::None,Value::Int(4)],kwargs:vec![]};
        moves::container_write(&self.db.ctx(docked),Container::Special {container_id:CONTAINER_HANGAR,owner_id:None},&call).await.unwrap()
    }
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn observed_cloak_deadlines_survive_dock_unfit_checks_and_undock_without_restarting() {
    let f=Fixture::new().await;let expected=f.history();let now=combat::now_ms();
    let mut db=crate::db::connect(&f.db.url).await.unwrap();
    assert!(crate::space_state::save_detached(&mut db,f.session.solar_system_id.unwrap(),&f.ship,crate::destiny::tick_now()).await.unwrap());
    let tx=db.transaction().await.unwrap();
    tx.execute(r#"UPDATE "character_" SET "stationID"=$2 WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32),&(f.station as i32)]).await.unwrap();
    tx.execute(r#"UPDATE entity SET "locationID"=$2,flag=4 WHERE "itemID"=$1"#,&[&(f.ship.id as i32),&(f.station as i32)]).await.unwrap();
    save_history(&tx,&f.ship,now).await.unwrap();tx.commit().await.unwrap();
    f.db.world.lock().parks.get_mut(&f.session.solar_system_id.unwrap()).unwrap().ships.remove(&f.ship.id);
    assert!(!f.db.db.query_one(r#"SELECT EXISTS(SELECT 1 FROM "shipSpaceMotion" WHERE "itemID"=$1)"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,bool>(0));
    assert_eq!(load_history(&f.db.db,f.ship.pilot.char_id,f.ship.id,now).await.unwrap(),expected);
    let docked=f.docked();
    assert!(matches!(f.move_module(&docked).await,Outcome::UserError {msg,..} if msg=="CannotRemoveModuleReactivationDelayed"));
    let restored=inflight::ship_for_space(&f.db.ctx(&docked),f.ship.pilot.char_id,f.ship.id).await.unwrap();
    assert!(!restored.cloaked);assert!(restored.fight.as_ref().unwrap().modules.is_empty());
    assert_eq!(restored.fight.as_ref().unwrap().cloak_history,expected,"docked reconnect reads the original deadlines only");
    let tx=db.transaction().await.unwrap();
    tx.execute(r#"UPDATE "character_" SET "stationID"=0 WHERE "characterID"=$1"#,&[&(f.ship.pilot.char_id as i32)]).await.unwrap();
    tx.execute(r#"UPDATE entity SET "locationID"=$2,flag=0 WHERE "itemID"=$1"#,&[&(f.ship.id as i32),&(f.session.solar_system_id.unwrap() as i32)]).await.unwrap();
    save_history(&tx,&restored,now+1).await.unwrap();tx.commit().await.unwrap();
    let loaded=inflight::ship_for_space(&f.db.ctx(&f.session),f.ship.pilot.char_id,f.ship.id).await.unwrap();
    assert_eq!(loaded.fight.as_ref().unwrap().cloak_history,expected);
    f.db.world.lock().parks.get_mut(&f.session.solar_system_id.unwrap()).unwrap().ships.insert(f.ship.id,loaded);
    let activate=Call {service:"dogmaIM".into(),method:"Activate".into(),args:vec![Value::Int(f.module),Value::bytes("cloaking"),Value::None,Value::Int(0)],kwargs:vec![]};
    assert!(matches!(inflight::dogma_im(&activate,&f.db.ctx(&f.session)).await,Some(Outcome::UserError {msg,..}) if msg=="ModuleReactivationDelayed"));
    let end=expected.module_ready[&f.module].max(expected.targeting_at);
    assert_eq!(load_history(&f.db.db,f.ship.pilot.char_id,f.ship.id,end+1).await.unwrap(),History::default());
    let mut txn=ItemTxn::begin(&mut db).await.unwrap();let item=txn.item(f.module).await.unwrap().unwrap();
    assert_eq!(module_reactivation_remaining(txn.tx(),&item,end+1).await.unwrap(),None);
    txn.commit().await.unwrap();
    assert_eq!(load_history(&f.db.db,f.ship.pilot.char_id,f.ship.id,now).await.unwrap(),expected,"querying expiry does not create a later deadline");
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn cooldown_transaction_rollback_and_custody_changes_cannot_leak_between_hulls_or_owners() {
    let f=Fixture::new().await;let expected=f.history();let now=combat::now_ms();f.save().await;
    let mut db=crate::db::connect(&f.db.url).await.unwrap();let tx=db.transaction().await.unwrap();
    tx.execute(r#"UPDATE entity SET "locationID"=$2,flag=4 WHERE "itemID"=$1"#,&[&(f.ship.id as i32),&(f.station as i32)]).await.unwrap();
    let mut newer=f.ship.clone();newer.fight.as_mut().unwrap().cloak_history.module_ready.insert(f.module,expected.module_ready[&f.module]+1000);
    save_history(&tx,&newer,now).await.unwrap();tx.rollback().await.unwrap();
    assert_eq!(load_history(&f.db.db,f.ship.pilot.char_id,f.ship.id,now).await.unwrap(),expected);
    assert_eq!(f.db.db.query_one(r#"SELECT "locationID" FROM entity WHERE "itemID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,i32>(0),f.session.solar_system_id.unwrap() as i32);
    assert_eq!(load_history(&f.db.db,f.ship.pilot.char_id,f.other,now).await.unwrap(),History::default());
    assert_eq!(load_history(&f.db.db,f.ship.pilot.char_id+1,f.ship.id,now).await.unwrap(),History::default());
    f.db.db.execute(r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,&[&(f.module as i32),&(f.other as i32)]).await.unwrap();
    assert!(!load_history(&f.db.db,f.ship.pilot.char_id,f.ship.id,now).await.unwrap().module_ready.contains_key(&f.module));
    assert_eq!(load_history(&f.db.db,f.ship.pilot.char_id,f.other,now).await.unwrap(),History::default());
    f.db.db.execute(r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,&[&(f.module as i32),&(f.ship.id as i32)]).await.unwrap();
    assert!(!load_history(&f.db.db,f.ship.pilot.char_id,f.ship.id,now).await.unwrap().module_ready.contains_key(&f.module),"returning an item cannot revive invalidated durable custody");
    f.save().await;
    f.db.db.execute(r#"UPDATE "cloakCooldowns" SET "readyAt"=$2 WHERE "itemID"=$1"#,&[&(f.module as i32),&i64::MAX]).await.unwrap();
    assert!(!load_history(&f.db.db,f.ship.pilot.char_id,f.ship.id,now).await.unwrap().module_ready.contains_key(&f.module));
    f.save().await;
    assert_eq!(load_history(&f.db.db,f.ship.pilot.char_id,f.ship.id,now).await.unwrap(),expected,"an invalid stored value cannot override a valid observed deadline");
    f.db.db.execute(r#"UPDATE "cloakCooldowns" SET "typeID"=11578,"readyAt"=$2 WHERE "itemID"=$1"#,&[&(f.module as i32),&expected.module_ready[&f.module]]).await.unwrap();
    assert!(!load_history(&f.db.db,f.ship.pilot.char_id,f.ship.id,now).await.unwrap().module_ready.contains_key(&f.module));
    f.db.db.execute(r#"UPDATE entity SET "ownerID"=$2 WHERE "itemID"=$1"#,&[&(f.ship.id as i32),&((f.ship.pilot.char_id+1) as i32)]).await.unwrap();
    f.db.db.execute(r#"UPDATE entity SET "ownerID"=$2 WHERE "itemID"=$1"#,&[&(f.ship.id as i32),&(f.ship.pilot.char_id as i32)]).await.unwrap();
    assert_eq!(load_history(&f.db.db,f.ship.pilot.char_id,f.ship.id,now).await.unwrap(),History::default());
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn cooldown_save_rechecks_module_custody_after_a_row_lock_wait() {
    let f=Fixture::new().await;f.save().await;
    let blocker=crate::db::connect(&f.db.url).await.unwrap();blocker.batch_execute("BEGIN").await.unwrap();
    blocker.execute(r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,&[&(f.module as i32),&(f.other as i32)]).await.unwrap();
    let pid:i32=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    let ((),())=tokio::join!(f.save(),async {
        tokio::time::timeout(std::time::Duration::from_secs(10),async {loop {
            if f.db.db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",&[&pid]).await.unwrap().get::<_,bool>(0) {break;}
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }}).await.unwrap();blocker.batch_execute("COMMIT").await.unwrap();
    });
    assert!(!load_history(&f.db.db,f.ship.pilot.char_id,f.ship.id,combat::now_ms()).await.unwrap().module_ready.contains_key(&f.module));
}
