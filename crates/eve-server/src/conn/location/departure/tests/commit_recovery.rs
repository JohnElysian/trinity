use super::*;
use std::sync::{Arc,atomic::{AtomicBool,AtomicUsize,Ordering}};
use tokio::io::{AsyncReadExt,AsyncWriteExt};

struct Proxy {url:String,dropped:Arc<AtomicUsize>,task:tokio::task::JoinHandle<()>}
impl Drop for Proxy {fn drop(&mut self){self.task.abort();}}

async fn packet(reader:&mut (impl tokio::io::AsyncRead+Unpin))->std::io::Result<Option<(u8,Vec<u8>)>> {
    let tag=match reader.read_u8().await {Ok(tag)=>tag,Err(error) if error.kind()==std::io::ErrorKind::UnexpectedEof=>return Ok(None),Err(error)=>return Err(error)};
    let size=reader.read_u32().await?;
    if !(4..=16_777_216).contains(&size) {return Err(std::io::Error::other("invalid PostgreSQL frame"));}
    let mut body=vec![0u8;(size-4) as usize];reader.read_exact(&mut body).await?;Ok(Some((tag,body)))
}
async fn forward(writer:&mut (impl tokio::io::AsyncWrite+Unpin),tag:u8,body:&[u8])->std::io::Result<()> {
    writer.write_u8(tag).await?;writer.write_u32(body.len() as u32+4).await?;writer.write_all(body).await
}

impl Proxy {
    async fn new(url:&str)->Self {
        assert!(url.contains("127.0.0.1:55432"),"only the disposable database may be proxied");
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let url=format!("{}{}sslmode=disable",url.replace("127.0.0.1:55432",&address.to_string()),if url.contains('?'){"&"}else{"?"});
        let armed=Arc::new(AtomicBool::new(true));let dropped=Arc::new(AtomicUsize::new(0));let count=dropped.clone();
        let task=tokio::spawn(async move {
            let mut copies=tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let (mut inbound,_)=accepted.unwrap();let armed=armed.clone();let dropped=count.clone();
                        copies.spawn(async move {
                            let result:std::io::Result<()>=async {
                                let mut backend=tokio::net::TcpStream::connect("127.0.0.1:55432").await?;
                                // SSL is explicitly disabled, so the first frame is StartupMessage.
                                let n=inbound.read_u32().await?;
                                if !(8..=65536).contains(&n){return Err(std::io::Error::other("invalid PostgreSQL startup"));}
                                let mut startup=vec![0u8;(n-4) as usize];inbound.read_exact(&mut startup).await?;
                                backend.write_u32(n).await?;backend.write_all(&startup).await?;
                                let (mut incoming,mut replies)=inbound.split();let (mut responses,mut requests)=backend.split();
                                let committing=AtomicBool::new(false);
                                let to_server=async {
                                    while let Some((tag,body))=packet(&mut incoming).await? {
                                        if tag==b'Q' && body==b"COMMIT\0" {committing.store(true,Ordering::SeqCst);}
                                        forward(&mut requests,tag,&body).await?;
                                    }Ok::<(),std::io::Error>(())
                                };
                                let to_client=async {
                                    while let Some((tag,body))=packet(&mut responses).await? {
                                        // CommandComplete is emitted only after COMMIT succeeds.
                                        // Drop it before the client can distinguish success from loss.
                                        if tag==b'C' && body==b"COMMIT\0" && committing.load(Ordering::SeqCst)
                                            && armed.swap(false,Ordering::SeqCst) {
                                            dropped.fetch_add(1,Ordering::SeqCst);
                                            return Err(std::io::Error::other("test discarded committed acknowledgement"));
                                        }
                                        forward(&mut replies,tag,&body).await?;
                                    }Ok::<(),std::io::Error>(())
                                };
                                tokio::select! {result=to_server=>result,result=to_client=>result}
                            }.await;
                            let _=result;
                        });
                    }
                    _=copies.join_next(), if !copies.is_empty()=>{},
                }
            }
        });
        Self {url,dropped,task}
    }
}

async fn hold_commit(f:&Fixture,reject:bool)->(tokio_postgres::Client,i32,i64,String) {
    // Keep the fixture's capacitor capacity/recharge identical through the
    // arrival profile, making a mistaken refund or duplicate debit observable.
    f.db.db.execute(r#"INSERT INTO entity_attributes("itemID","attributeID","valueFloat") VALUES($1,482,1000),($1,55,0)
        ON CONFLICT("itemID","attributeID") DO UPDATE SET "valueFloat"=EXCLUDED."valueFloat","valueInt"=NULL"#,&[&(f.ship.id as i32)]).await.unwrap();
    let key=f.db.id().await as i64;let name=format!("commit_outcome_{key}");
    let failure=if reject{"RAISE EXCEPTION 'test rejects deferred COMMIT';"}else{""};
    f.db.db.batch_execute(&format!(r#"CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        IF NEW."itemID"={} AND NEW."locationID"=30000143 THEN PERFORM pg_advisory_xact_lock({key}::bigint); {failure} END IF; RETURN NEW; END $$;
        CREATE CONSTRAINT TRIGGER {name} AFTER UPDATE ON entity DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {name}();"#,f.ship.id)).await.unwrap();
    let blocker=crate::db::connect(&f.db.url).await.unwrap();
    let pid=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    blocker.query_one("SELECT pg_advisory_lock($1)",&[&key]).await.unwrap();
    (blocker,pid,key,name)
}
async fn cleanup(f:&Fixture,name:&str) {
    f.db.db.batch_execute(&format!("DROP TRIGGER {name} ON entity; DROP FUNCTION {name}();")).await.unwrap();
}
async fn quantity(f:&Fixture)->i32 {f.db.db.query_one("SELECT quantity FROM entity WHERE \"itemID\"=$1",&[&f.fuel]).await.unwrap().get(0)}
fn request(f:&Fixture)->JumpRequest {JumpRequest::Beacon {member:f.member,beacon:f.beacon}}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable database at127.0.0.1:55432"]
async fn committed_jump_with_lost_acknowledgement_keeps_latest_combat_and_pays_once() {
    let f=Fixture::new().await;let (blocker,pid,key,name)=hold_commit(&f,false).await;
    let proxy=Proxy::new(&f.db.url).await;let mut server=f.server().await;server.db_url=proxy.url.clone();
    let arrival=f.arrival();let quote=f.quote();
    let (result,())=tokio::time::timeout(Duration::from_secs(30),async {tokio::join!(
        super::super::reserved::run(&server,&f.session,1,request(&f),&arrival,&quote,"effects.JumpDriveIn"),async {
            wait_for_lock(&blocker,pid).await;
            {let mut w=f.db.world.lock();let ship=w.parks.get_mut(&arrival.from).unwrap().ships.get_mut(&f.ship.id).unwrap();
                assert!(ship.identity_reserved);ship.pos=Vec3::new(111.0,222.0,333.0);
                let fight=ship.fight.as_mut().unwrap();fight.vitals.hull_damage=19.0;assert!(fight.vitals.capacitor.spend(crate::combat::now_ms(),50.0));}
            assert!(crate::travel::blocked_in_memory(&server.world,f.ship.id));
            blocker.query_one("SELECT pg_advisory_unlock($1)",&[&key]).await.unwrap();
        })}).await.unwrap();
    let arrived=result.unwrap();assert_eq!(arrived.ship_id,f.ship.id);assert_eq!(arrived.place.system,arrival.system);
    assert_eq!(proxy.dropped.load(Ordering::SeqCst),1);assert_eq!(quantity(&f).await,5);
    {let w=f.db.world.lock();assert!(!w.parks[&arrival.from].ships.contains_key(&f.ship.id));
        let ship=&w.parks[&arrival.system].ships[&f.ship.id];assert!(!ship.identity_reserved);
        let fight=ship.fight.as_ref().unwrap();assert_eq!(fight.vitals.hull_damage,19.0);assert_eq!(fight.vitals.capacitor.at(crate::combat::now_ms()),850.0);}
    let values=f.db.db.query_one(r#"SELECT
        (SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3),
        (SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=18),
        (SELECT count(*) FROM "travelReceipts" WHERE "shipID"=$1 AND state=1)"#,&[&(f.ship.id as i32)]).await.unwrap();
    assert_eq!(values.get::<_,f64>(0),19.0);assert_eq!(values.get::<_,f64>(1),850.0);assert_eq!(values.get::<_,i64>(2),1);
    assert!(!crate::losses::pending::blocked(&f.db.db,f.ship.id).await.unwrap());
    assert!(!crate::travel::blocked_in_memory(&server.world,f.ship.id));
    assert!(super::super::reserved::run(&server,&f.session,1,request(&f),&arrival,&quote,"effects.JumpDriveIn").await.is_err());
    assert_eq!(quantity(&f).await,5);cleanup(&f,&name).await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn deferred_commit_failure_proves_rollback_and_refunds_only_its_capacitor_once() {
    let f=Fixture::new().await;let (blocker,pid,key,name)=hold_commit(&f,true).await;
    let server=f.server().await;let arrival=f.arrival();let quote=f.quote();
    let (result,())=tokio::time::timeout(Duration::from_secs(30),async {tokio::join!(
        super::super::reserved::run(&server,&f.session,1,request(&f),&arrival,&quote,"effects.JumpDriveIn"),async {
            wait_for_lock(&blocker,pid).await;
            {let mut w=f.db.world.lock();let ship=w.parks.get_mut(&arrival.from).unwrap().ships.get_mut(&f.ship.id).unwrap();
                let fight=ship.fight.as_mut().unwrap();fight.vitals.hull_damage=17.0;assert!(fight.vitals.capacitor.spend(crate::combat::now_ms(),50.0));}
            blocker.query_one("SELECT pg_advisory_unlock($1)",&[&key]).await.unwrap();
        })}).await.unwrap();
    assert!(result.unwrap_err().to_string().contains("rolled back"));assert_eq!(quantity(&f).await,8);
    {let w=f.db.world.lock();let ship=&w.parks[&arrival.from].ships[&f.ship.id];assert!(!ship.identity_reserved);
        let fight=ship.fight.as_ref().unwrap();assert_eq!(fight.vitals.hull_damage,17.0);assert_eq!(fight.vitals.capacitor.at(crate::combat::now_ms()),950.0);
        assert!(!w.parks[&arrival.system].ships.contains_key(&f.ship.id));}
    assert!(!crate::travel::blocked_in_memory(&server.world,f.ship.id));
    assert_eq!(f.db.db.query_one(r#"SELECT count(*) FROM "travelReceipts" WHERE "shipID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap().get::<_,i64>(0),0);
    cleanup(&f,&name).await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable database at127.0.0.1:55432"]
async fn lethal_combat_during_accepted_commit_keeps_source_kill_and_destination_capsule() {
    let f=Fixture::new().await;let (blocker,pid,key,name)=hold_commit(&f,false).await;
    let proxy=Proxy::new(&f.db.url).await;let mut server=f.server().await;server.db_url=proxy.url.clone();
    let arrival=f.arrival();let quote=f.quote();let source_pos=Vec3::new(222.0,333.0,444.0);
    let (result,())=tokio::time::timeout(Duration::from_secs(30),async {tokio::join!(
        super::super::reserved::run(&server,&f.session,1,request(&f),&arrival,&quote,"effects.JumpDriveIn"),async {
            wait_for_lock(&blocker,pid).await;
            {let mut w=f.db.world.lock();let ship=w.parks.get_mut(&arrival.from).unwrap().ships.get_mut(&f.ship.id).unwrap();
                assert!(ship.identity_reserved);ship.pos=source_pos;let fight=ship.fight.as_mut().unwrap();
                fight.vitals.hull_damage=fight.vitals.hull_max;fight.last_attacker=Some((-587,999_587));
                fight.damage_log.insert(-587,(587,34,fight.vitals.hull_max));}
            blocker.query_one("SELECT pg_advisory_unlock($1)",&[&key]).await.unwrap();
        })}).await.unwrap();
    let arrived=result.unwrap();assert_ne!(arrived.ship_id,f.ship.id);assert_eq!(arrived.place.system,arrival.system);
    assert_eq!(proxy.dropped.load(Ordering::SeqCst),1);
    let capsule=f.db.db.query_one(r#"SELECT "typeID","locationID",x,y,z FROM entity WHERE "itemID"=$1"#,&[&(arrived.ship_id as i32)]).await.unwrap();
    assert_eq!(capsule.get::<_,i32>(0),670);assert_eq!(capsule.get::<_,i32>(1),arrival.system as i32);
    assert_eq!(Vec3::new(capsule.get(2),capsule.get(3),capsule.get(4)),arrival.at);
    let kill=f.db.db.query_one(r#"SELECT "solarSystemID","finalBlowID","killBlob" FROM "killLog" WHERE "victimItemID"=$1"#,&[&(f.ship.id as i32)]).await.unwrap();
    assert_eq!(kill.get::<_,i32>(0),arrival.from as i32);assert_eq!(kill.get::<_,i32>(1),-587);
    let blob=kill.get::<_,String>(2);let fuel=blob.split("<i ").find(|i|i.starts_with("t=\"16273\" ")).unwrap();
    let count=|key:&str|fuel.split(&format!(" {key}=\"")).nth(1).unwrap().split('"').next().unwrap().parse::<i64>().unwrap();
    assert_eq!(count("d")+count("x"),5,"only the fuel remaining after the one committed debit can be lost");
    let raw:String=f.db.db.query_one(r#"SELECT loss FROM "travelReceipts" WHERE "shipID"=$1 AND state=1"#,&[&(f.ship.id as i32)]).await.unwrap().get(0);
    let saved:crate::losses::Loss=serde_json::from_str(&raw).unwrap();assert_eq!(saved.pos,source_pos);assert_eq!(saved.system,arrival.from);
    assert_eq!(saved.custody_system(),arrival.system);assert_eq!(saved.custody_position(),arrival.at);
    assert!(!crate::losses::pending::blocked_character(&f.db.db,f.ship.pilot.char_id).await.unwrap());
    assert!(!crate::travel::blocked_in_memory(&server.world,f.ship.id));
    cleanup(&f,&name).await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable database at127.0.0.1:55432"]
async fn station_arrival_with_lost_commit_acknowledgement_resolves_without_replaying_the_move() {
    let f=Fixture::new().await;
    let row=f.db.db.query_one(r#"SELECT "stationID","constellationID","regionID" FROM "staStations"
        WHERE "solarSystemID"=30000142 ORDER BY "stationID" LIMIT 1"#,&[]).await.unwrap();
    let station=i64::from(row.get::<_,i32>(0));
    let to=super::super::transition::Place {station:Some(station),system:30000142,
        constellation:i64::from(row.get::<_,i32>(1)),region:i64::from(row.get::<_,i32>(2)),at:Vec3::ZERO};
    let proxy=Proxy::new(&f.db.url).await;let mut server=f.server().await;server.db_url=proxy.url.clone();
    let mut session=f.session.clone();session.role=8;
    f.db.world.lock().parks.get_mut(&30000142).unwrap().ships.get_mut(&f.ship.id).unwrap()
        .fight.as_mut().unwrap().vitals.hull_damage=23.0;
    let result=tokio::time::timeout(Duration::from_secs(30),super::super::transition::run(
        &server,&session,1,super::super::transition::Request::Admin {to})).await.unwrap().unwrap();
    assert_eq!(proxy.dropped.load(Ordering::SeqCst),1);
    assert_eq!(result.place.station,Some(station));assert_eq!(result.place.system,30000142);assert_eq!(result.ship_id,f.ship.id);
    let saved=f.db.db.query_one(r#"SELECT c."stationID",e."locationID",e.flag,
        (SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=e."itemID" AND "attributeID"=3)
        FROM "character_" c JOIN entity e ON e."itemID"=c."shipID" WHERE c."characterID"=$1"#,
        &[&(f.ship.pilot.char_id as i32)]).await.unwrap();
    assert_eq!(saved.get::<_,i32>(0),station as i32);assert_eq!(saved.get::<_,i32>(1),station as i32);
    assert_eq!(saved.get::<_,i32>(2),4);assert_eq!(saved.get::<_,f64>(3),23.0);
    assert!(!f.db.world.lock().parks[&30000142].ships.contains_key(&f.ship.id));
    assert!(f.db.world.lock().pending_station_arrivals.is_empty());
    assert!(!crate::losses::pending::blocked_character(&f.db.db,f.ship.pilot.char_id).await.unwrap());
    assert_eq!(quantity(&f).await,8,"station arrival does not consume jump fuel");
    assert!(super::super::transition::run(&server,&session,1,super::super::transition::Request::Admin {to}).await.is_err(),
        "a delayed callback with the old space session cannot repeat the station move");
    assert_eq!(quantity(&f).await,8);
}
