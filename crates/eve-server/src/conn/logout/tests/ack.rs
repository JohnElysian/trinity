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
        let armed=Arc::new(AtomicUsize::new(1));let dropped=Arc::new(AtomicUsize::new(0));let count=dropped.clone();
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
                                            && armed.fetch_update(Ordering::SeqCst,Ordering::SeqCst,|n|n.checked_sub(1))==Ok(1) {
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

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable database at127.0.0.1:55432"]
async fn lost_final_ack_then_lethal_settlement_clears_only_obsolete_logout() {
    let f=Fixture::new().await;
    let (pilot,id,system)=(f.pilot,f.id,f.system);
    f.db.db.execute(r#"UPDATE "character_" SET "corporationID"=1000167,balance=1000,
        "constellationID"=s."constellationID","regionID"=s."regionID" FROM "mapSolarSystems" s
        WHERE "characterID"=$1 AND s."solarSystemID"=$2"#,&[&(pilot as i32),&(system as i32)]).await.unwrap();
    f.db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton)
        VALUES($1,1373,$1,$2,57,1,1)"#,&[&(pilot as i32),&(system as i32)]).await.unwrap();
    let mut writer=crate::db::connect(&f.db.url).await.unwrap();
    retry_one(&f.db.world,&mut writer,id).await.unwrap();
    assert!(blocked(&writer,id).await.unwrap());
    {
        let mut w=f.db.world.lock();let hull=w.parks.get_mut(&system).unwrap().ships.get_mut(&id).unwrap();
        hull.pos=Vec3::new(11.0,22.0,33.0);
        let vitals=&mut hull.fight.as_mut().unwrap().vitals;
        // The active profile's confirmed threshold must survive an inactive
        // profile with a larger maximum when the loss is retried.
        vitals.hull_max=10.0;vitals.hull_damage=11.0;
    }
    let proxy=Proxy::new(&f.db.url).await;
    let mut uncertain=crate::db::connect(&proxy.url).await.unwrap();
    assert!(tokio::time::timeout(std::time::Duration::from_secs(30),retry_one(&f.db.world,&mut uncertain,id)).await.unwrap().is_err());
    assert_eq!(proxy.dropped.load(Ordering::SeqCst),1,"the successful COMMIT reply was actually discarded");
    assert!(!blocked(&writer,id).await.unwrap(),"the logout deletion committed despite the missing reply");
    assert!(writer.query_opt(r#"SELECT 1 FROM "pendingShipLosses" WHERE "shipID"=$1"#,&[&(id as i32)]).await.unwrap().is_some());
    assert!(blocked_character_in_memory(&f.db.world,pilot));
    assert!(!f.db.world.lock().parks[&system].ships.contains_key(&id));

    let server=Server {table:Default::default(),db:crate::db::Shared::new(f.db.db,f.db.url.clone()),db_url:f.db.url,node_id:1,
        bulk:f.db.bulk,dogma:f.db.dogma,services:crate::svc::registry(),world:f.db.world,dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0};
    // This is the production heartbeat order: loss settlement precedes the
    // next logout retry and replaces the old active hull with a capsule.
    crate::losses::journal::retry(&server,&mut writer).await;
    assert!(writer.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(id as i32)]).await.unwrap().is_none());
    let capsule=i64::from(writer.query_one(r#"SELECT "shipID" FROM "character_" WHERE "characterID"=$1"#,&[&(pilot as i32)]).await.unwrap().get::<_,i32>(0));
    assert_ne!(capsule,id);
    assert!(blocked_character_in_memory(&server.world,pilot),"the lost reply still has its old in-memory obligation");
    let latest=Vec3::new(701.0,802.0,903.0);
    let (current,tick)={let mut w=server.world.lock();let park=w.parks.get_mut(&system).unwrap();let hull=park.ships.get_mut(&capsule).unwrap();
        hull.pos=latest;hull.fight.as_mut().unwrap().vitals.hull_damage=7.0;(hull.clone(),park.tick)};
    assert!(crate::space_state::save_detached(&mut writer,system,&current,tick).await.unwrap());
    retry_one(&server.world,&mut writer,id).await.unwrap();
    assert!(!blocked_character_in_memory(&server.world,pilot));
    {let w=server.world.lock();assert!(!w.pending_logouts.contains_key(&id));assert!(!w.retained_logouts.contains_key(&id));
        assert!(!w.parks[&system].ships.contains_key(&id));assert_eq!(w.parks[&system].ships[&capsule].pos,latest);}
    let row=writer.query_one(r#"SELECT c."shipID",e.x,e.y,e.z,a."valueFloat" FROM "character_" c
        JOIN entity e ON e."itemID"=c."shipID" JOIN entity_attributes a ON a."itemID"=e."itemID" AND a."attributeID"=3
        WHERE c."characterID"=$1"#,&[&(pilot as i32)]).await.unwrap();
    assert_eq!(i64::from(row.get::<_,i32>(0)),capsule);assert_eq!(Vec3::new(row.get(1),row.get(2),row.get(3)),latest);assert_eq!(row.get::<_,f64>(4),7.0);
    assert_eq!(writer.query_one(r#"SELECT count(*) FROM "killLog" WHERE "victimItemID"=$1"#,&[&(id as i32)]).await.unwrap().get::<_,i64>(0),1);
    assert!(!crate::losses::pending::blocked_character(&writer,pilot).await.unwrap());
    writer.execute(r#"DELETE FROM "killLogParticipants" WHERE "killID" IN(SELECT "killID" FROM "killLog" WHERE "victimCharID"=$1)"#,&[&(pilot as i32)]).await.unwrap();
    writer.execute(r#"DELETE FROM "killLog" WHERE "victimCharID"=$1"#,&[&(pilot as i32)]).await.unwrap();
    writer.execute(r#"DELETE FROM entity_attributes WHERE "itemID" IN(SELECT "itemID" FROM entity WHERE "ownerID"=$1)"#,&[&(pilot as i32)]).await.unwrap();
    writer.execute(r#"DELETE FROM entity WHERE "ownerID"=$1"#,&[&(pilot as i32)]).await.unwrap();
    writer.execute(r#"DELETE FROM "character_" WHERE "characterID"=$1"#,&[&(pilot as i32)]).await.unwrap();
}

