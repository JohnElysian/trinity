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
        Self::after(url,0).await
    }
    async fn after(url:&str,skip:usize)->Self {
        assert!(url.contains("127.0.0.1:55432"),"only the disposable database may be proxied");
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address=listener.local_addr().unwrap();
        let url=format!("{}{}sslmode=disable",url.replace("127.0.0.1:55432",&address.to_string()),if url.contains('?'){"&"}else{"?"});
        let armed=Arc::new(AtomicUsize::new(skip+1));let dropped=Arc::new(AtomicUsize::new(0));let count=dropped.clone();
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


use super::super::tests::{Fixture,FROM,TO,blocked as wait_lock};

fn req(f:&Fixture,url:&str)->Request {Request {world:f.db.world.clone(),url:url.into(),pilot:f.session.char_id.unwrap(),client:1,ship:f.ship,from:FROM,
    member:Some(f.member),beacon:f.beacon,to:TO,alliance:f.session.alliance_id,bonuses:BTreeMap::new()}}
async fn server(f:&Fixture)->Server {
    let db=crate::db::connect(&f.db.url).await.unwrap();let dogma=Catalog::load(&db).await.unwrap();
    Server {table:Default::default(),db:crate::db::Shared::new(db,f.db.url.clone()),db_url:f.db.url.clone(),node_id:1,
        bulk:crate::bulkdata::BulkData {entries:Default::default(),node_id:1},dogma,world:f.db.world.clone(),services:crate::svc::registry(),
        dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0}
}
async fn hold(f:&Fixture,reject:bool)->(Client,i32,i64,String) {
    let key=i64::from(f.db.id().await);let name=format!("portal_ack_{key}");
    let rejection=if reject {"RAISE EXCEPTION 'deliberate portal COMMIT rollback';"}else{""};
    f.db.db.batch_execute(&format!(r#"CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock({key}::bigint); {rejection} RETURN NEW; END $$;
        CREATE CONSTRAINT TRIGGER {name} AFTER INSERT ON "portalReceipts" DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN(NEW."shipID"={}) EXECUTE FUNCTION {name}();"#,f.ship)).await.unwrap();
    let blocker=crate::db::connect(&f.db.url).await.unwrap();let pid=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    blocker.query_one("SELECT pg_advisory_lock($1)",&[&key]).await.unwrap();(blocker,pid,key,name)
}
async fn unhold(f:&Fixture,name:&str) {f.db.db.batch_execute(&format!(r#"DROP TRIGGER {name} ON "portalReceipts"; DROP FUNCTION {name}();"#)).await.unwrap();}
async fn receipt(f:&Fixture)->Receipt {let row=f.db.db.query_one(r#"SELECT generation,"characterID","shipID",intent FROM "portalReceipts" WHERE "shipID"=$1 ORDER BY generation DESC LIMIT 1"#,&[&(f.ship as i32)]).await.unwrap();decode(&row).unwrap()}
async fn phase(f:&Fixture)->i16 {f.db.db.query_one(r#"SELECT state FROM "portalReceipts" WHERE "shipID"=$1 ORDER BY generation DESC LIMIT 1"#,&[&(f.ship as i32)]).await.unwrap().get(0)}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable database at127.0.0.1:55432"]
async fn lost_commit_acknowledgement_pays_standard_and_covert_once_with_current_combat() {
    for covert in [false,true] {
        let f=Fixture::new(covert).await;let (blocker,pid,key,name)=hold(&f,false).await;
        // 73 differs deliberately: original effect2152 chooses attribute1002.
        f.db.db.execute(r#"INSERT INTO entity_attributes("itemID","attributeID","valueInt") VALUES($1,73,123),($1,1002,45000)
            ON CONFLICT("itemID","attributeID") DO UPDATE SET "valueInt"=EXCLUDED."valueInt","valueFloat"=NULL"#,&[&(f.module as i32)]).await.unwrap();
        let proxy=Proxy::new(&f.db.url).await;
        let (result,())=tokio::time::timeout(std::time::Duration::from_secs(30),async {tokio::join!(run(req(&f,&proxy.url)),async {
            wait_lock(&blocker,pid).await;assert!(blocked_in_memory(&f.db.world,f.ship));
            {let mut w=f.db.world.lock();let fighter=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap();
                fighter.vitals.hull_damage=17.0;assert!(fighter.vitals.capacitor.spend(combat::now_ms(),50.0));}
            blocker.query_one("SELECT pg_advisory_unlock($1)",&[&key]).await.unwrap();
        })}).await.unwrap();result.unwrap();unhold(&f,&name).await;
        assert_eq!(proxy.dropped.load(Ordering::SeqCst),1);assert_eq!(f.fuel().await,if covert{1500}else{1000});
        let expected=if covert{9850.0}else{8950.0};assert_eq!(f.cap(),expected);f.released();assert_eq!(phase(&f).await,1);
        let r=receipt(&f).await;assert_eq!(r.intent.expires-r.intent.started,45000);
        {let w=f.db.world.lock();let b=&w.bridges[&f.ship];assert_eq!((b.generation,b.started_ms,b.until_ms),(r.generation,r.intent.started,r.intent.expires));}
        let rows=f.db.db.query(r#"SELECT "attributeID","valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID" IN(3,18) ORDER BY "attributeID""#,&[&(f.ship as i32)]).await.unwrap();
        assert_eq!(rows[0].get::<_,f64>(1),17.0);assert_eq!(rows[1].get::<_,f64>(1),expected);
        assert!(!blocked(&f.db.db,f.ship).await.unwrap());assert!(!blocked_in_memory(&f.db.world,f.ship));
        assert!(f.open().await.is_err());assert_eq!(f.fuel().await,if covert{1500}else{1000});
    }
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn delayed_definite_rollback_restores_exact_recharge_curve_without_replaying_payment() {
    let f=Fixture::new(false).await;
    let before={let mut w=f.db.world.lock();let cap=&mut w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.capacitor;
        *cap=Charged::full(10000.0,5000.0,combat::now_ms());cap.set(combat::now_ms(),5000.0);*cap};
    let (blocker,pid,key,name)=hold(&f,true).await;
    let (result,())=tokio::time::timeout(std::time::Duration::from_secs(30),async {tokio::join!(f.open(),async {
        wait_lock(&blocker,pid).await;tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        f.db.world.lock().parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.hull_damage=21.0;
        blocker.query_one("SELECT pg_advisory_unlock($1)",&[&key]).await.unwrap();
    })}).await.unwrap();assert!(result.is_err());unhold(&f,&name).await;
    assert_eq!(f.fuel().await,1500);f.released();let w=f.db.world.lock();let fighter=w.parks[&FROM].ships[&f.ship].fight.as_ref().unwrap();
    assert_eq!(fighter.vitals.capacitor,before);assert_eq!(fighter.vitals.hull_damage,21.0);assert!(w.bridges.is_empty());
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn accepted_portal_expiry_during_ack_recovery_does_not_start_a_fresh_cycle() {
    let f=Fixture::new(false).await;
    f.db.db.execute(r#"INSERT INTO entity_attributes("itemID","attributeID","valueInt") VALUES($1,1002,100)"#,&[&(f.module as i32)]).await.unwrap();
    let (blocker,pid,key,name)=hold(&f,false).await;let proxy=Proxy::new(&f.db.url).await;
    let (result,())=tokio::time::timeout(std::time::Duration::from_secs(30),async {tokio::join!(run(req(&f,&proxy.url)),async {
        wait_lock(&blocker,pid).await;tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        blocker.query_one("SELECT pg_advisory_unlock($1)",&[&key]).await.unwrap();
    })}).await.unwrap();result.unwrap();unhold(&f,&name).await;
    assert_eq!(proxy.dropped.load(Ordering::SeqCst),1);assert_eq!(f.cap(),9000.0);assert_eq!(f.fuel().await,1000);assert_eq!(phase(&f).await,2);
    assert!(f.db.world.lock().bridges.is_empty());assert!(f.db.world.lock().portal_cycles.is_empty());f.released();
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn failed_final_checkpoint_retries_fresh_live_state_and_lethal_commit_keeps_original_loss() {
    let f=Fixture::new(false).await;let name=format!("portal_write_{}",f.ship);
    f.db.world.lock().parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.hull_damage=1.0;
    f.db.db.batch_execute(&format!(r#"ALTER TABLE entity_attributes ADD CONSTRAINT {name} CHECK("itemID"<>{} OR "attributeID"<>3) NOT VALID"#,f.ship)).await.unwrap();
    let (result,())=tokio::time::timeout(std::time::Duration::from_secs(30),async {tokio::join!(f.open(),async {
        loop {if blocked(&f.db.db,f.ship).await.unwrap(){break;}tokio::time::sleep(std::time::Duration::from_millis(10)).await;}
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(f.db.db.query_one(r#"SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=18"#,&[&(f.ship as i32)]).await.unwrap().get::<_,f64>(0),9000.0,
            "initial accepted payment already persisted charge while final checkpoint is blocked");
        {let mut w=f.db.world.lock();let fight=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap();
            fight.vitals.hull_damage=31.0;fight.vitals.capacitor.spend(combat::now_ms(),80.0);}
        f.db.db.batch_execute(&format!("ALTER TABLE entity_attributes DROP CONSTRAINT {name}")).await.unwrap();
    })}).await.unwrap();result.unwrap();assert_eq!(f.cap(),8920.0);assert_eq!(f.fuel().await,1000);f.released();
    assert_eq!(f.db.db.query_one(r#"SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(f.ship as i32)]).await.unwrap().get::<_,f64>(0),31.0);
    let f=Fixture::new(false).await;let (blocker,pid,key,name)=hold(&f,false).await;let proxy=Proxy::new(&f.db.url).await;
    let (result,())=tokio::time::timeout(std::time::Duration::from_secs(30),async {tokio::join!(run(req(&f,&proxy.url)),async {
        wait_lock(&blocker,pid).await;{let mut w=f.db.world.lock();let ship=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap();
            ship.pos=Vec3::new(123.0,456.0,789.0);let f=ship.fight.as_mut().unwrap();f.vitals.hull_damage=f.vitals.hull_max;}
        blocker.query_one("SELECT pg_advisory_unlock($1)",&[&key]).await.unwrap();
    })}).await.unwrap();result.unwrap();unhold(&f,&name).await;
    assert_eq!(phase(&f).await,2);assert_eq!(f.fuel().await,1000);assert_eq!(f.cap(),9000.0);f.released();assert!(f.db.world.lock().bridges.is_empty());
    let raw:String=f.db.db.query_one(r#"SELECT loss FROM "pendingShipLosses" WHERE "shipID"=$1"#,&[&(f.ship as i32)]).await.unwrap().get(0);
    let loss:Loss=serde_json::from_str(&raw).unwrap();assert_eq!(loss.system,FROM);assert_eq!(loss.pos,Vec3::new(123.0,456.0,789.0));assert!(loss.arrival_custody.is_none());
    assert!(f.db.world.lock().pending_ship_losses.contains_key(&f.ship));
}

async fn seed(f:&Fixture)->Receipt {
    let mut db=crate::db::connect(&f.db.url).await.unwrap();let tx=db.transaction().await.unwrap();
    let r=begin(&tx,&req(f,&f.db.url),&Quote {generator_id:f.module,generator_type:23953,fuel_type:16275,fuel:500,capacitor:1000.0,duration:60000,mass_factor:1e-9,range:10.0,harmonics:0},11567).await.unwrap();
    tx.commit().await.unwrap();r
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn restart_keeps_unknown_marker_quarantined_and_never_relinks_a_reused_member_field_id() {
    let f=Fixture::new(false).await;
    f.db.db.execute(r#"INSERT INTO "pendingShipLogouts"("shipID","characterID","solarSystemID","createdAt") VALUES($1,$2,$3,$4)"#,
        &[&(f.ship as i32),&(f.session.char_id.unwrap() as i32),&(FROM as i32),&combat::now_ms()]).await.unwrap();
    assert!(run(req(&f,&f.db.url)).await.is_err(),"direct portal runner obeys shared logout quarantine");assert_eq!(f.cap(),10000.0);assert_eq!(f.fuel().await,1500);
    f.db.db.execute(r#"DELETE FROM "pendingShipLogouts" WHERE "shipID"=$1"#,&[&(f.ship as i32)]).await.unwrap();
    let r=seed(&f).await;assert!(run(req(&f,&f.db.url)).await.is_err(),"direct runner cannot bypass an earlier pending operation");
    let mut s=server(&f).await;s.world=World::default();let mut db=crate::db::connect(&f.db.url).await.unwrap();
    retry(&s,&mut db).await;assert!(blocked(&db,f.ship).await.unwrap());assert_eq!(phase(&f).await,0);
    // A completed checkpoint is not authority to rebuild a disappeared old
    // module/field; even an identical numeric field ID in a new World is not it.
    db.execute(r#"UPDATE "portalReceipts" SET state=1 WHERE generation=$1"#,&[&r.generation]).await.unwrap();
    s.world.lock().parks=std::mem::take(&mut f.db.world.lock().parks);
    assert!(s.world.lock().parks[&TO].beacons.contains_key(&f.beacon),"reused numeric field exists in reconstructed world");
    retry(&s,&mut db).await;assert_eq!(phase(&f).await,2);assert!(s.world.lock().bridges.is_empty());
    assert_eq!(f.fuel().await,1500,"recovery never executes the payment recipe");
    // A stale historical acknowledgement cannot overwrite a newer generation.
    let newer=seed(&f).await;let tx=db.transaction().await.unwrap();assert_eq!(locked(&tx,&r).await.unwrap().resolution,Resolution::Superseded);drop(tx);
    assert!(blocked(&db,f.ship).await.unwrap());assert!(newer.generation>r.generation);
    let current={let mut w=s.world.lock();let hull=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap();hull.identity_reserved=true;
        let cap=hull.fight.as_ref().unwrap().vitals.capacitor;w.pending_bridges.insert(f.ship);
        let mut old=Pending::owned(r.clone(),1,Charged::full(10000.0,0.0,0),Charged::full(9000.0,0.0,0));old.busy=0;
        w.pending_portals.insert(r.generation,old);w.pending_portals.insert(newer.generation,Pending::owned(newer.clone(),1,cap,cap));
        w.portal_cycles.insert(f.ship,bridge(&newer));cap};
    retry(&s,&mut db).await;
    {let w=s.world.lock();assert!(!w.pending_portals.contains_key(&r.generation));assert!(w.pending_portals.contains_key(&newer.generation));
        assert!(w.pending_bridges.contains(&f.ship));assert!(w.parks[&FROM].ships[&f.ship].identity_reserved);
        assert_eq!(w.parks[&FROM].ships[&f.ship].fight.as_ref().unwrap().vitals.capacitor,current);assert_eq!(w.portal_cycles[&f.ship].generation,newer.generation);}
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn durable_known_portal_loss_recovers_after_restart_and_corrupt_rows_cannot_starve_it() {
    let f=Fixture::new(false).await;
    let mut corrupt=Vec::new();for _ in 0..64 {let ship=f.db.id().await;corrupt.push(f.db.db.query_one(r#"INSERT INTO "portalReceipts"("characterID","shipID",intent) VALUES($1,$1,'invalid') RETURNING generation"#,&[&ship]).await.unwrap().get::<_,i64>(0));}
    let r=seed(&f).await;let loss={let w=f.db.world.lock();Loss::of(&w.parks[&FROM].ships[&f.ship],FROM,false)};
    f.db.db.execute(r#"UPDATE "portalReceipts" SET loss=$2 WHERE generation=$1"#,&[&r.generation,&serde_json::to_string(&loss).unwrap()]).await.unwrap();
    let mut s=server(&f).await;s.world=World::default();s.world.lock().portal_retry_cursor=*corrupt.first().unwrap()-1;
    let mut db=crate::db::connect(&f.db.url).await.unwrap();for _ in 0..3 {retry(&s,&mut db).await;}
    assert_eq!(phase(&f).await,2);assert!(db.query_opt(r#"SELECT 1 FROM "pendingShipLosses" WHERE "shipID"=$1"#,&[&(f.ship as i32)]).await.unwrap().is_some());
    assert!(s.world.lock().bridges.is_empty());assert_eq!(f.fuel().await,1500);
    db.execute(r#"DELETE FROM "portalReceipts" WHERE generation=ANY($1)"#,&[&corrupt]).await.unwrap();
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn cancelled_retry_releases_only_its_claim_and_leaves_unvisited_receipts_retryable() {
    let f=Fixture::new(false).await;let r=seed(&f).await;let other=Fixture::new(false).await;let second=seed(&other).await;
    let s=Arc::new(server(&f).await);
    {let mut w=s.world.lock();for receipt in [r.clone(),second.clone()] {let mut p=Pending::owned(receipt,1,Charged::full(0.0,0.0,0),Charged::full(0.0,0.0,0));p.busy=0;w.pending_portals.insert(p.receipt.generation,p);}}
    let blocker=crate::db::connect(&f.db.url).await.unwrap();let pid=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    blocker.batch_execute("BEGIN").await.unwrap();blocker.query_one(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#,&[&(r.intent.pilot as i32)]).await.unwrap();
    let task={let s=s.clone();tokio::spawn(async move {let mut db=crate::db::connect(&s.db_url).await.unwrap();retry(&s,&mut db).await;})};
    wait_lock(&blocker,pid).await;
    {let w=s.world.lock();assert_ne!(w.pending_portals[&r.generation].busy,0);assert_eq!(w.pending_portals[&second.generation].busy,0);}
    task.abort();assert!(task.await.unwrap_err().is_cancelled());blocker.batch_execute("ROLLBACK").await.unwrap();
    let w=s.world.lock();assert_eq!(w.pending_portals[&r.generation].busy,0);assert_eq!(w.pending_portals[&second.generation].busy,0);
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable database at127.0.0.1:55432"]
async fn lost_checkpoint_ack_recaptures_later_combat_without_another_payment() {
    let f=Fixture::new(false).await;let key=i64::from(f.db.id().await);let name=format!("portal_checkpoint_{key}");
    f.db.db.batch_execute(&format!(r#"CREATE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock({key}::bigint); RETURN NEW; END $$;
        CREATE CONSTRAINT TRIGGER {name} AFTER INSERT ON "shipSpaceMotion" DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN(NEW."itemID"={}) EXECUTE FUNCTION {name}();"#,f.ship)).await.unwrap();
    let blocker=crate::db::connect(&f.db.url).await.unwrap();let pid=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    blocker.query_one("SELECT pg_advisory_lock($1)",&[&key]).await.unwrap();let proxy=Proxy::after(&f.db.url,1).await;
    let (result,())=tokio::time::timeout(std::time::Duration::from_secs(30),async {tokio::join!(run(req(&f,&proxy.url)),async {
        wait_lock(&blocker,pid).await;
        assert_eq!(f.fuel().await,1000);assert_eq!(phase(&f).await,0);
        assert_eq!(f.db.db.query_one(r#"SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=18"#,&[&(f.ship as i32)]).await.unwrap().get::<_,f64>(0),9000.0);
        {let mut w=f.db.world.lock();let f=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap();
            f.vitals.hull_damage=37.0;assert!(f.vitals.capacitor.spend(combat::now_ms(),70.0));}
        blocker.query_one("SELECT pg_advisory_unlock($1)",&[&key]).await.unwrap();
    })}).await.unwrap();result.unwrap();
    f.db.db.batch_execute(&format!(r#"DROP TRIGGER {name} ON "shipSpaceMotion"; DROP FUNCTION {name}();"#)).await.unwrap();
    assert_eq!(proxy.dropped.load(Ordering::SeqCst),1);assert_eq!(f.fuel().await,1000);assert_eq!(f.cap(),8930.0);f.released();assert_eq!(phase(&f).await,1);
    let rows=f.db.db.query(r#"SELECT "attributeID","valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID" IN(3,18) ORDER BY "attributeID""#,&[&(f.ship as i32)]).await.unwrap();
    assert_eq!(rows[0].get::<_,f64>(1),37.0);assert_eq!(rows[1].get::<_,f64>(1),8930.0);
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn ordinary_loss_settlement_between_portal_phases_closes_only_proven_original_receipt() {
    for retained_payload in [true,false] {
    let f=Fixture::new(false).await;let r=seed(&f).await;let mut s=server(&f).await;let mut db=crate::db::connect(&f.db.url).await.unwrap();
    let loss={let mut w=s.world.lock();let ship=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap();
        ship.identity_reserved=true;ship.pos=Vec3::new(101.0,202.0,303.0);let fighter=ship.fight.as_mut().unwrap();fighter.vitals.hull_damage=fighter.vitals.hull_max;
        Loss::of(ship,FROM,false)};
    let cap=f.db.world.lock().parks[&FROM].ships[&f.ship].fight.as_ref().unwrap().vitals.capacitor;
    let mut p=Pending::owned(r.clone(),1,cap,cap);p.loss=Some(loss.clone());p.busy=0;
    s.world.lock().pending_portals.insert(r.generation,p);s.world.lock().pending_bridges.insert(f.ship);
    if retained_payload {db.execute(r#"UPDATE "portalReceipts" SET loss=$2 WHERE generation=$1"#,&[&r.generation,&serde_json::to_string(&loss).unwrap()]).await.unwrap();}
    // Force the interleaving: a generic recovery owner settles the retained
    // source loss after receipt retention but before portal loss staging.
    crate::losses::journal::handle(&s,&mut db,loss.clone()).await;
    assert!(db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(f.ship as i32)]).await.unwrap().is_none());
    let capsule:i32=db.query_one(r#"SELECT "shipID" FROM "character_" WHERE "characterID"=$1"#,&[&(r.intent.pilot as i32)]).await.unwrap().get(0);
    assert_ne!(i64::from(capsule),f.ship);assert_eq!(phase(&f).await,0);
    if !retained_payload {s.world=World::default();}
    // A different attribution is not sufficient proof to release quarantine.
    db.execute(r#"UPDATE "killLog" SET "solarSystemID"=$2 WHERE "victimItemID"=$1"#,&[&(f.ship as i32),&(TO as i32)]).await.unwrap();
    s.world.lock().portal_retry_cursor=r.generation-1;
    retry(&s,&mut db).await;assert_eq!(phase(&f).await,0);assert!(blocked_character(&db,r.intent.pilot).await.unwrap());
    db.execute(r#"UPDATE "killLog" SET "solarSystemID"=$2 WHERE "victimItemID"=$1"#,&[&(f.ship as i32),&(FROM as i32)]).await.unwrap();
    s.world.lock().portal_retry_cursor=r.generation-1;
    retry(&s,&mut db).await;assert_eq!(phase(&f).await,2);assert!(!blocked_character(&db,r.intent.pilot).await.unwrap());
    assert!(!blocked_character_in_memory(&s.world,r.intent.pilot));assert!(!s.world.lock().pending_bridges.contains(&f.ship));assert!(s.world.lock().bridges.is_empty());
    assert_eq!(db.query_one(r#"SELECT "shipID" FROM "character_" WHERE "characterID"=$1"#,&[&(r.intent.pilot as i32)]).await.unwrap().get::<_,i32>(0),capsule);
    assert_eq!(db.query_one(r#"SELECT count(*) FROM "killLog" WHERE "victimItemID"=$1"#,&[&(f.ship as i32)]).await.unwrap().get::<_,i64>(0),1);
    }
}
