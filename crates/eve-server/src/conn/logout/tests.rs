use super::*;
use crate::{ballpark::{Park,Vec3},session::Session,test_support::Database};
mod ack;

struct Fixture {db:Database,session:Session,id:i64,pilot:i64,system:i64}
impl Fixture {
    async fn new()->Self {
        let db=Database::load().await;let pilot=db.id().await;let id=db.id().await;let system=30000142_i32;
        db.db.execute(r#"INSERT INTO "character_"("characterID","characterName","shipID","solarSystemID","stationID") VALUES($1,$2,$3,$4,0)"#,&[&pilot,&format!("Retained logout {pilot}"),&id,&system]).await.unwrap();
        db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,587,$2,$3,0,1,1)"#,&[&id,&pilot,&system]).await.unwrap();
        let session=Session {char_id:Some(pilot as i64),ship_id:Some(id as i64),solar_system_id:Some(system as i64),..Default::default()};
        let ship=crate::svc::inflight::ship_for_space(&db.ctx(&session),pilot as i64,id as i64).await.unwrap();
        {let mut w=db.world.lock();let mut park=Park::new(system as i64,vec![],crate::destiny::tick_now());park.ships.insert(id as i64,ship);w.parks.insert(system as i64,park);w.retained_logouts.insert(id as i64,(pilot as i64,1));}
        Self {db,session,id:id as i64,pilot:pilot as i64,system:system as i64}
    }
    async fn clean(self) {
        self.db.db.execute(r#"DELETE FROM entity_attributes WHERE "itemID"=$1"#,&[&(self.id as i32)]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM entity WHERE "itemID"=$1"#,&[&(self.id as i32)]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "character_" WHERE "characterID"=$1"#,&[&(self.pilot as i32)]).await.unwrap();
    }
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn active_cycle_retains_source_then_saves_last_damage_and_position() {
    let f=Fixture::new().await;let mut writer=crate::db::connect(&f.db.url).await.unwrap();
    {let mut w=f.db.world.lock();w.portal_cycles.insert(f.id,crate::world::Bridge {generation:19,started_ms:combat::now_ms(),until_ms:combat::now_ms()+60000,
        titan_pilot:f.pilot,generator_id:123,system:f.system,to_system:30000141,beacon:99,member:None,mass_factor:1.0,harmonics:0});}
    retry_one(&f.db.world,&mut writer,f.id).await.unwrap();
    assert!(!blocked(&f.db.db,f.id).await.unwrap());assert!(f.db.world.lock().parks[&f.system].ships.contains_key(&f.id));
    f.db.world.lock().portal_cycles.get_mut(&f.id).unwrap().until_ms=combat::now_ms()-1;
    {
        let mut w=f.db.world.lock();w.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.id).unwrap().fight.as_mut().unwrap().modules.push(combat::Active {
            module_id:124,type_id:21096,effect_id:2857,guid:String::new(),offensive:false,warp_safe:false,disallow_warp_alignment:true,
            action:combat::Action::Cyno,target:None,ammo:None,duration_ms:60000,cap_need:0.0,first_cycle_prepaid:false,
            started_ms:combat::now_ms(),next_ms:combat::now_ms()+60000,cycles:1,repeat:0,stopping:false,name:"Cyno".into(),reach:None,landed:false,
        });
    }
    retry_one(&f.db.world,&mut writer,f.id).await.unwrap();assert!(!blocked(&f.db.db,f.id).await.unwrap(),"original906 cyno cycle also retains its live source");
    f.db.world.lock().parks.get_mut(&f.system).unwrap().ships.get_mut(&f.id).unwrap().fight.as_mut().unwrap().modules.clear();
    retry_one(&f.db.world,&mut writer,f.id).await.unwrap();
    assert!(blocked(&f.db.db,f.id).await.unwrap());assert!(f.db.world.lock().parks[&f.system].ships.contains_key(&f.id),"marker commits before live removal");
    {let mut w=f.db.world.lock();let ship=w.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.id).unwrap();ship.pos=Vec3::new(123.0,456.0,789.0);ship.fight.as_mut().unwrap().vitals.hull_damage=27.0;}
    retry_one(&f.db.world,&mut writer,f.id).await.unwrap();
    assert!(!blocked(&f.db.db,f.id).await.unwrap());assert!(!f.db.world.lock().parks[&f.system].ships.contains_key(&f.id));assert!(!blocked_character_in_memory(&f.db.world,f.pilot));
    let row=f.db.db.query_one(r#"SELECT x,y,z FROM entity WHERE "itemID"=$1"#,&[&(f.id as i32)]).await.unwrap();assert_eq!((row.get::<_,f64>(0),row.get::<_,f64>(1),row.get::<_,f64>(2)),(123.0,456.0,789.0));
    let loaded=crate::svc::inflight::ship_for_space(&f.db.ctx(&f.session),f.pilot,f.id).await.unwrap();assert_eq!(loaded.fight.unwrap().vitals.hull_damage,27.0);
    f.clean().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn reconnect_claims_current_live_hull_without_rewinding_or_late_removal() {
    let f=Fixture::new().await;let mut stale=f.db.world.lock().parks[&f.system].ships[&f.id].clone();stale.client_id=22;
    {let mut w=f.db.world.lock();let live=w.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.id).unwrap();live.pos=Vec3::new(70.0,80.0,90.0);live.fight.as_mut().unwrap().vitals.hull_damage=33.0;
        crate::conn::location::publish_reconnect(&mut w,f.system,stale).unwrap();let current=&w.parks[&f.system].ships[&f.id];
        assert_eq!(current.client_id,22);assert_eq!(current.pos,Vec3::new(70.0,80.0,90.0));assert_eq!(current.fight.as_ref().unwrap().vitals.hull_damage,33.0);assert!(!w.retained_logouts.contains_key(&f.id));}
    let mut writer=crate::db::connect(&f.db.url).await.unwrap();retry_one(&f.db.world,&mut writer,f.id).await.unwrap();
    assert!(f.db.world.lock().parks[&f.system].ships.contains_key(&f.id));assert!(!blocked(&f.db.db,f.id).await.unwrap());f.clean().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn final_snapshot_failure_preserves_marker_and_retries_same_snapshot() {
    let f=Fixture::new().await;let mut writer=crate::db::connect(&f.db.url).await.unwrap();
    retry_one(&f.db.world,&mut writer,f.id).await.unwrap();
    let trigger=format!(r#"CREATE FUNCTION logout_failure_{}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW."itemID"={} THEN RAISE EXCEPTION 'injected logout checkpoint failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER logout_failure_{} BEFORE INSERT OR UPDATE ON "shipSpaceMotion" FOR EACH ROW EXECUTE FUNCTION logout_failure_{}();"#,f.id,f.id,f.id,f.id);
    f.db.db.batch_execute(&trigger).await.unwrap();
    {let mut w=f.db.world.lock();w.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.id).unwrap().fight.as_mut().unwrap().vitals.hull_damage=41.0;}
    assert!(retry_one(&f.db.world,&mut writer,f.id).await.is_err());assert!(blocked(&f.db.db,f.id).await.unwrap());assert!(blocked_character_in_memory(&f.db.world,f.pilot));
    assert!(!f.db.world.lock().parks[&f.system].ships.contains_key(&f.id));
    let restarted=World::default();assert!(!blocked_character_in_memory(&restarted,f.pilot));assert!(crate::losses::pending::blocked_character(&f.db.db,f.pilot).await.unwrap(),"marker-only restart cannot invent survival");
    f.db.db.batch_execute(&format!("DROP TRIGGER logout_failure_{} ON \"shipSpaceMotion\"; DROP FUNCTION logout_failure_{}();",f.id,f.id)).await.unwrap();
    retry_one(&f.db.world,&mut writer,f.id).await.unwrap();assert!(!blocked(&f.db.db,f.id).await.unwrap());assert!(!blocked_character_in_memory(&f.db.world,f.pilot));
    let loaded=crate::svc::inflight::ship_for_space(&f.db.ctx(&f.session),f.pilot,f.id).await.unwrap();assert_eq!(loaded.fight.unwrap().vitals.hull_damage,41.0);f.clean().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn actual_connection_logout_keeps_portal_source_live_without_a_client() {
    let f=Fixture::new().await;let mut writer=crate::db::connect(&f.db.url).await.unwrap();
    let (id,pilot,system)=(f.id,f.pilot,f.system);
    f.db.world.lock().retained_logouts.clear();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client,accepted)=tokio::join!(tokio::net::TcpStream::connect(listener.local_addr().unwrap()),listener.accept());
    let (socket,peer)=accepted.unwrap();let client=client.unwrap();
    let server=std::sync::Arc::new(Server {table:Default::default(),db:crate::db::Shared::new(f.db.db,f.db.url.clone()),db_url:f.db.url,node_id:1,
        bulk:f.db.bulk,dogma:f.db.dogma,services:crate::svc::registry(),world:f.db.world,dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0});
    let mut conn=crate::conn::Conn::new(socket,peer,server,1);conn.session=f.session;
    let client_id=conn.client_id;
    let _receiver={let mut w=conn.server.world.lock();let rx=w.connect(client_id);
        w.set_presence(client_id,Some(crate::world::Presence{char_id:pilot,corp_id:1000002,solar_system_id:system,docked:false,station_id:None,role:0}));
        w.parks.get_mut(&system).unwrap().ships.get_mut(&id).unwrap().client_id=client_id;
        w.portal_cycles.insert(id,crate::world::Bridge {generation:20,started_ms:combat::now_ms(),until_ms:combat::now_ms()+60000,
            titan_pilot:pilot,generator_id:123,system,to_system:30000141,beacon:99,member:None,mass_factor:1.0,harmonics:0});rx};
    drop(client);conn.leave_world().await;
    {let w=conn.server.world.lock();assert!(w.clients_of(pilot).is_empty());assert!(w.parks[&system].ships.contains_key(&id));assert_eq!(w.retained_logouts.get(&id),Some(&(pilot,client_id)));}
    retry_one(&conn.server.world,&mut writer,id).await.unwrap();assert!(!blocked(&writer,id).await.unwrap());
    conn.server.world.lock().portal_cycles.get_mut(&id).unwrap().until_ms=combat::now_ms()-1;
    retry_one(&conn.server.world,&mut writer,id).await.unwrap();retry_one(&conn.server.world,&mut writer,id).await.unwrap();
    assert!(!conn.server.world.lock().parks[&system].ships.contains_key(&id));assert!(!blocked(&writer,id).await.unwrap());
    writer.execute(r#"DELETE FROM entity_attributes WHERE "itemID"=$1"#,&[&(id as i32)]).await.unwrap();
    writer.execute(r#"DELETE FROM entity WHERE "itemID"=$1"#,&[&(id as i32)]).await.unwrap();
    writer.execute(r#"DELETE FROM "character_" WHERE "characterID"=$1"#,&[&(pilot as i32)]).await.unwrap();
}
