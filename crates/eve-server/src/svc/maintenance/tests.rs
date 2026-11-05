use super::*;
use crate::{test_support::Database,session::Session,ballpark::Park};
#[path="inventory_tests.rs"] mod inventory_cases;

struct Fixture {db:Database,actor:Session,host:Session,host_id:i64,ship:i64,system:i64}
impl Fixture {
    async fn new()->Self {
        let db=Database::load().await;let a=db.id().await;let b=db.id().await;let ship=db.id().await;let host=db.id().await;let corp=db.id().await;
        let system=30000141i64;
        db.db.execute(r#"INSERT INTO corporation("corporationID","corporationName") VALUES($1,$2)"#,&[&corp,&format!("Maintenance {corp}")]).await.unwrap();
        for (who,hull,ty) in [(a,ship,588i32),(b,host,23911i32)] {
            db.db.execute(r#"INSERT INTO "character_"("characterID","characterName","shipID","corporationID","solarSystemID","stationID") VALUES($1,$2,$3,$4,$5,0)"#,&[&who,&format!("Bay pilot {who}"),&hull,&corp,&(system as i32)]).await.unwrap();
            db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,$2,$3,$4,0,1,1)"#,&[&hull,&ty,&who,&(system as i32)]).await.unwrap();
        }
        for (skill,level) in super::super::fitting::required_skill_levels(&db.db,588).await.unwrap() {
            let id=db.id().await;
            db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,$2,$3,$3,7,1,1)"#,&[&id,&skill,&a]).await.unwrap();
            db.db.execute(r#"INSERT INTO entity_attributes("itemID","attributeID","valueInt") VALUES($1,280,$2)"#,&[&id,&level]).await.unwrap();
        }
        let session=|who:i32,hull:i32|Session{char_id:Some(who as i64),ship_id:Some(hull as i64),corp_id:Some(corp as i64),solar_system_id:Some(system),solar_system_id2:Some(system),..Default::default()};
        let actor=session(a,ship);let host_session=session(b,host);
        let mut actor_ship=inflight::ship_for_space(&db.ctx(&actor),a as i64,ship as i64).await.unwrap();
        let mut ctx=db.ctx(&host_session);ctx.client_id=2;
        let host_ship=inflight::ship_for_space(&ctx,b as i64,host as i64).await.unwrap();
        actor_ship.arrive(Vec3::new(host_ship.stats.radius+actor_ship.stats.radius+100.0,0.0,0.0));
        let mut park=Park::new(system,vec![],crate::destiny::tick_now());park.ships.insert(ship as i64,actor_ship);park.ships.insert(host as i64,host_ship);
        db.world.lock().parks.insert(system,park);
        Self{db,actor,host:host_session,host_id:host as i64,ship:ship as i64,system}
    }
    fn actor_ctx(&self)->Ctx<'_>{self.db.ctx(&self.actor)}
    fn host_ctx(&self)->Ctx<'_>{let mut c=self.db.ctx(&self.host);c.client_id=2;c}
    async fn item(&self,ty:i32,owner:i64,loc:i64,flag:i32,quantity:i32,singleton:i32)->i64 {
        let id=self.db.id().await;
        self.db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,$2,$3,$4,$5,$6,$7)"#,&[&id,&ty,&(owner as i32),&(loc as i32),&flag,&quantity,&singleton]).await.unwrap();id as i64
    }
    async fn location(&self,id:i64)->(i64,i32,i64){let r=self.db.db.query_one(r#"SELECT "locationID",flag,"ownerID" FROM entity WHERE "itemID"=$1"#,&[&(id as i32)]).await.unwrap();(i64::from(r.get::<_,i32>(0)),r.get(1),i64::from(r.get::<_,i32>(2)))}
}

fn listed(v:Value)->Vec<i64>{match v{Value::Instance{state,..}=>state.get_str("lines").and_then(Value::seq).unwrap_or(&[]).iter().filter_map(|r|r.seq()?.first()?.as_i64()).collect(),_=>vec![]}}
fn cells(v:&Value)->&[Value]{match v{Value::Instance{state,..}=>state.get_str("line").and_then(Value::seq).unwrap_or(&[]),_=>&[]}}
fn success<T>(r:Result<T,MoveError>)->T{match r{Ok(v)=>v,Err(e)=>panic!("{}",embark::error_text(e))}}
async fn inventory_call(ctx:&Ctx<'_>,host:i64,method:&str,args:Vec<Value>)->Outcome{
    super::super::moves::container_write(ctx,super::super::inventory::Container::Item(host),&Call{service:"invbroker".into(),method:method.into(),args,kwargs:vec![]}).await.unwrap()
}
fn moved(out:Outcome)->bool{matches!(out,Outcome::Value(Value::None))}

#[test]
fn packaged_volume_matches_original_client_not_assembled_hull_volume(){
    assert_eq!(unit_volume(588,237,false,15800.0),2500.0);
    assert_eq!(unit_volume(588,237,true,15800.0),15800.0);
    assert_eq!(unit_volume(11019,28,false,1e8),1e8);
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_store_capsule_board_preserves_custody_damage_and_private_inventory(){
    let mut f=Fixture::new().await;let who=f.actor.char_id.unwrap();
    let secret=f.item(34,f.host.char_id.unwrap(),f.host_id,5,10,0).await;
    let cargo=f.item(34,who,f.ship,5,1,0).await;
    assert!(store(&f.actor_ctx(),f.host_id).await.is_err(),"non-charge cargo prevents storage");
    assert_eq!(f.location(f.ship).await.0,f.system);
    f.db.db.execute(r#"DELETE FROM entity WHERE "itemID"=$1"#,&[&(cargo as i32)]).await.unwrap();
    {let mut w=f.db.world.lock();let fight=w.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap();fight.vitals.armor_damage=7.0;fight.vitals.hull_damage=3.0;}
    let capsule=success(store(&f.actor_ctx(),f.host_id).await);
    assert_eq!(f.location(f.ship).await,(f.host_id,90,who));
    assert!(!f.db.world.lock().parks[&f.system].ships.contains_key(&f.ship));
    f.actor.ship_id=Some(capsule);
    let ids=listed(super::super::inventory::list(&f.actor_ctx(),super::super::inventory::Container::Item(f.host_id)).await);
    assert_eq!(ids,vec![f.ship]);assert!(!ids.contains(&secret),"bay access never exposes host cargo");
    let next=success(board_stored(&f.actor_ctx(),f.host_id,f.ship).await);
    assert_eq!(next,f.ship);assert_eq!(f.location(next).await,(f.system,0,who));
    assert!(f.db.db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(capsule as i32)]).await.unwrap().is_none());
    let w=f.db.world.lock();let s=&w.parks[&f.system].ships[&next];assert!(s.is_pilot());assert_eq!(s.fight.as_ref().unwrap().vitals.armor_damage,7.0);assert_eq!(s.fight.as_ref().unwrap().vitals.hull_damage,3.0);
    assert!(w.parks[&f.system].ships.values().all(|s|!s.identity_reserved));
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_launch_stack_scoop_and_transient_configuration(){
    let mut f=Fixture::new().await;let owner=f.host.char_id.unwrap();
    assert_eq!(success(configuration(&f.host_ctx(),None).await).get_str("allowGangSMBUsage"),Some(&Value::Bool(false)));
    success(configuration(&f.host_ctx(),Some(true)).await);
    assert_eq!(success(configuration(&f.host_ctx(),None).await).get_str("allowGangSMBUsage"),Some(&Value::Bool(true)));
    f.actor.corp_id=None;
    f.db.db.execute(r#"UPDATE "character_" SET "corporationID"=0 WHERE "characterID"=$1"#,&[&(f.actor.char_id.unwrap() as i32)]).await.unwrap();
    assert!(view(&f.actor_ctx(),f.host_id).await.is_none(),"fleet opt-in alone is not membership");
    f.db.world.lock().gangs.test_members(vec![owner,f.actor.char_id.unwrap()]);
    assert!(view(&f.actor_ctx(),f.host_id).await.is_some());
    f.db.world.lock().gangs=Default::default();
    assert!(view(&f.actor_ctx(),f.host_id).await.is_none(),"leaving fleet revokes access immediately");
    let packed=f.item(588,owner,f.host_id,90,2,0).await;
    success(operate(&f.host_ctx(),f.host_id,Operation::Launch(vec![packed,packed])).await);
    let launched:Vec<_>=f.db.world.lock().parks[&f.system].ships.values().filter(|s|s.unpiloted&&s.pilot.char_id==owner).map(|s|s.id).collect();
    assert_eq!(launched.len(),2);
    for id in &launched{assert_eq!(f.location(*id).await,(f.system,0,owner));}
    success(operate(&f.host_ctx(),f.host_id,Operation::Scoop(packed)).await);
    assert_eq!(f.location(packed).await,(f.host_id,90,owner));
    assert!(!f.db.world.lock().parks[&f.system].ships.contains_key(&packed));
    let no_access=f.item(588,f.actor.char_id.unwrap(),f.system,0,1,1).await;
    assert!(operate(&f.host_ctx(),f.host_id,Operation::Scoop(no_access)).await.is_err());
    let mut w=f.db.world.lock();let hull=space::remove_ship(&mut w,f.system,f.host_id).unwrap();assert!(!w.maintenance.contains_key(&f.host_id));space::add_ship(&mut w,f.system,hull);
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_station_deposits_use_original_volume_and_reject_full_or_wrong_items(){
    let mut f=Fixture::new().await;let owner=f.host.char_id.unwrap();
    let station:i32=f.db.db.query_one(r#"SELECT "stationID" FROM "staStations" ORDER BY "stationID" LIMIT 1"#,&[]).await.unwrap().get(0);
    f.db.db.execute(r#"UPDATE "character_" SET "stationID"=$2 WHERE "characterID"=$1"#,&[&(owner as i32),&station]).await.unwrap();
    f.db.db.execute(r#"UPDATE entity SET "locationID"=$2,flag=4 WHERE "itemID"=$1"#,&[&(f.host_id as i32),&station]).await.unwrap();
    f.host.station_id=Some(station as i64);f.host.solar_system_id=None;
    let packed=f.item(588,owner,station as i64,4,3,0).await;
    assert!(moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(packed),Value::None,Value::Int(90)]).await));
    assert_eq!(f.location(packed).await,(f.host_id,90,owner));
    assert_eq!(success(used(&f.db.db,f.host_id,90,None).await),7500.0);
    let capacity=super::super::inventory::get_capacity(&f.host_ctx(),super::super::inventory::Container::Item(f.host_id),Some(90)).await;
    assert_eq!(cells(&capacity).get(1),Some(&Value::Double(7500.0)));
    let notship=f.item(34,owner,station as i64,4,1,0).await;
    assert!(!moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(notship),Value::None,Value::Int(90)]).await));
    let huge=f.item(23911,owner,station as i64,4,1,1).await;
    assert!(!moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(huge),Value::None,Value::Int(90)]).await));
    assert_eq!(f.location(huge).await.0,station as i64);
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_array_requires_live_state_permissions_and_physical_user_admission(){
    let mut f=Fixture::new().await;
    let array=f.item(12237,f.actor.corp_id.unwrap(),f.system,0,1,1).await;
    f.db.db.execute(r#"INSERT INTO "posStructures"("itemID",state,"useViewPut","useViewPutTake","useUse") VALUES($1,1,0,0,0)"#,&[&(array as i32)]).await.unwrap();
    let structure=crate::pos::load(&f.db.db,f.system).await.unwrap().into_iter().find(|s|s.id==array).unwrap();
    f.db.world.lock().parks.get_mut(&f.system).unwrap().structures.insert(array,structure);
    assert!(store(&f.actor_ctx(),array).await.is_err(),"default POS permissions require a role");
    f.db.db.execute(r#"UPDATE "posStructures" SET "useViewPut"=1,"useViewPutTake"=1 WHERE "itemID"=$1"#,&[&(array as i32)]).await.unwrap();
    let crowd:Vec<_>=(0..9).map(|n|9_800_000_000+n).collect();
    {let mut w=f.db.world.lock();let park=w.parks.get_mut(&f.system).unwrap();let original=park.ships[&f.ship].clone();
     for id in &crowd{let mut s=original.clone();s.id=*id;s.pilot.char_id=*id;park.ships.insert(*id,s);}}
    assert!(store(&f.actor_ctx(),array).await.is_err(),"nearby piloted hulls count even without inventory windows");
    {let mut w=f.db.world.lock();let park=w.parks.get_mut(&f.system).unwrap();for id in crowd{park.ships.remove(&id);}park.structures.get_mut(&array).unwrap().state=crate::pos::state::UNANCHORED;}
    assert!(store(&f.actor_ctx(),array).await.is_err(),"unanchored arrays cannot operate");
    f.db.world.lock().parks.get_mut(&f.system).unwrap().structures.get_mut(&array).unwrap().state=crate::pos::state::ANCHORED;
    let capsule=success(store(&f.actor_ctx(),array).await);f.actor.ship_id=Some(capsule);
    assert_eq!(f.location(f.ship).await,(array,0,f.actor.char_id.unwrap()));
    assert_eq!(success(board_stored(&f.actor_ctx(),array,f.ship).await),f.ship);
    assert_eq!(f.location(f.ship).await.0,f.system);
}

async fn wait_blocked(db:&tokio_postgres::Client,pid:i32){
    tokio::time::timeout(std::time::Duration::from_secs(10),async{loop{
        db.query_one("SELECT pg_stat_clear_snapshot()",&[]).await.unwrap();
        if db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",&[&pid]).await.unwrap().get::<_,bool>(0){break;}
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }}).await.unwrap();
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_rechecks_moving_host_after_wait_without_losing_live_damage_or_custody(){
    let f=Fixture::new().await;
    let charge:i32=f.db.db.query_one(r#"SELECT t."typeID" FROM "invTypes" t JOIN "invGroups" g ON g."groupID"=t."groupID" WHERE g."categoryID"=8 ORDER BY t."typeID" LIMIT 1"#,&[]).await.unwrap().get(0);
    let cargo=f.item(charge,f.actor.char_id.unwrap(),f.ship,5,1,0).await;
    let blocker=crate::db::connect(&f.db.url).await.unwrap();let pid:i32=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    blocker.batch_execute("BEGIN").await.unwrap();blocker.query_one(r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(cargo as i32)]).await.unwrap();
    let ctx=f.actor_ctx();
    let (result,())=tokio::join!(store(&ctx,f.host_id),async{
        wait_blocked(&blocker,pid).await;
        {let mut w=f.db.world.lock();let park=w.parks.get_mut(&f.system).unwrap();assert!(park.ships[&f.ship].identity_reserved);assert!(park.ships[&f.host_id].identity_reserved);
         park.ships.get_mut(&f.host_id).unwrap().pos=Vec3::new(1e9,0.0,0.0);park.ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.hull_damage=7.0;}
        blocker.batch_execute("ROLLBACK").await.unwrap();
    });
    assert!(result.is_err());assert_eq!(f.location(f.ship).await.0,f.system);assert_eq!(f.location(cargo).await.0,f.ship);
    let w=f.db.world.lock();let ship=&w.parks[&f.system].ships[&f.ship];assert_eq!(ship.fight.as_ref().unwrap().vitals.hull_damage,7.0);assert!(!ship.identity_reserved);assert!(!w.parks[&f.system].ships[&f.host_id].identity_reserved);
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_final_flush_preserves_damage_received_while_custody_commit_waits(){
    let f=Fixture::new().await;let key=f.db.id().await;
    let trigger=format!("maintenance_commit_{key}");
    f.db.db.batch_execute(&format!(r#"CREATE FUNCTION {trigger}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        IF NEW."itemID"={} AND NEW."locationID"={} THEN PERFORM pg_advisory_xact_lock({key}::bigint); END IF; RETURN NEW; END $$;
        CREATE CONSTRAINT TRIGGER {trigger} AFTER UPDATE ON entity DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {trigger}();"#,f.ship,f.host_id)).await.unwrap();
    let blocker=crate::db::connect(&f.db.url).await.unwrap();let pid:i32=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    blocker.query_one("SELECT pg_advisory_lock($1)",&[&(key as i64)]).await.unwrap();
    let ctx=f.actor_ctx();
    let (result,())=tokio::join!(store(&ctx,f.host_id),async{
        wait_blocked(&blocker,pid).await;
        {let mut w=f.db.world.lock();let ship=w.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap();assert!(ship.identity_reserved);ship.fight.as_mut().unwrap().vitals.hull_damage=11.0;}
        blocker.query_one("SELECT pg_advisory_unlock($1)",&[&(key as i64)]).await.unwrap();
    });
    success(result);
    let damage:f64=f.db.db.query_one(r#"SELECT COALESCE("valueFloat","valueInt"::float8) FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(f.ship as i32)]).await.unwrap().get(0);
    assert_eq!(damage,11.0);assert!(f.db.world.lock().maintenance_repairs.is_empty());
    f.db.db.batch_execute(&format!("DROP TRIGGER {trigger} ON entity; DROP FUNCTION {trigger}();")).await.unwrap();
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_failed_final_flush_blocks_withdrawal_and_stale_generation_cannot_overwrite(){
    let mut f=Fixture::new().await;
    let mut fighter=f.db.world.lock().parks[&f.system].ships[&f.ship].fight.clone().unwrap();fighter.vitals.hull_damage=13.0;
    let capsule=success(store(&f.actor_ctx(),f.host_id).await);f.actor.ship_id=Some(capsule);
    let generation:i64=f.db.db.query_one("SELECT generation FROM maintenance_custody WHERE item_id=$1",&[&(f.ship as i32)]).await.unwrap().get(0);
    let pending=Repair{id:f.ship,host:f.host_id,owner:f.actor.char_id.unwrap(),flag:90,generation,at_ms:crate::combat::now_ms(),fighter};
    f.db.world.lock().maintenance_repairs.insert(f.ship,pending.clone());
    repair::attempt(&f.db.world,"host=127.0.0.1 port=1 user=unavailable connect_timeout=1",pending.clone()).await;
    assert!(board_stored(&f.actor_ctx(),f.host_id,f.ship).await.is_err());
    retry_repairs(&f.db.world,&f.db.url).await;
    assert!(f.db.world.lock().maintenance_repairs.is_empty());
    // A later transfer generation must make every delayed old repair inert,
    // even if the ship happens to return to the same owner, host and bay.
    f.db.db.execute(r#"UPDATE entity SET flag=5 WHERE "itemID"=$1"#,&[&(f.ship as i32)]).await.unwrap();
    f.db.db.execute(r#"UPDATE entity SET flag=90 WHERE "itemID"=$1"#,&[&(f.ship as i32)]).await.unwrap();
    let later=success(repair::begin(&f.db.db,f.ship,f.host_id,f.actor.char_id.unwrap(),90).await);assert_ne!(later,generation);
    f.db.db.execute(r#"UPDATE entity_attributes SET "valueInt"=NULL,"valueFloat"=2 WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(f.ship as i32)]).await.unwrap();
    repair::attempt(&f.db.world,&f.db.url,pending).await;
    let damage:f64=f.db.db.query_one(r#"SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(f.ship as i32)]).await.unwrap().get(0);assert_eq!(damage,2.0);
}
