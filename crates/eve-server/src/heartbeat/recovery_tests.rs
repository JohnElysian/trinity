use super::*;
use std::sync::atomic::{AtomicBool,Ordering};

#[test]
fn confirmed_loss_blocks_relogin_before_its_database_job_is_dispatched() {
    let world=World::default();
    let loss=crate::losses::Loss {client_id:10,char_id:100,ship_id:101,ship_type:587,system:30_000_001,
        pos:Vec3::ZERO,arrival_custody:None,capsule:false,unpiloted:false,final_blow:None,attackers:vec![],items:vec![]};
    world.lock().pending_jobs.push(Job::Lost(Box::new(loss)));
    let jobs=pulse(&world,combat::now_ms());
    assert!(matches!(jobs.as_slice(),[Job::Lost(_)]),"the SQL job remains undispatched");
    assert!(crate::losses::journal::blocked_character_in_memory(&world,100));
    assert!(crate::losses::journal::blocked_in_memory(&world,101));
}

/// Reject new sessions or withhold replies on an established one without
/// stopping PostgreSQL or interfering with another test's backend.
struct Fixture {
    server:Arc<Server>,admin:Arc<Client>,original_url:String,pilot:i32,system:i64,app:String,
    available:Arc<AtomicBool>,stalled:tokio::sync::watch::Sender<bool>,
    worker:Option<tokio::task::JoinHandle<()>>,proxy:Option<tokio::task::JoinHandle<()>>,
}
impl Fixture {
    async fn new()->Self {
        use crate::ballpark::{Park,Pilot,Ship,ShipStats};
        use tokio::io::{AsyncReadExt,AsyncWriteExt};
        let f=crate::test_support::Database::load().await;
        assert!(f.url.contains("127.0.0.1:55432"),"proxy test is restricted to the disposable test database");
        let pilot=f.id().await;
        f.db.execute(r#"INSERT INTO "character_"("characterID","characterName","securityRating") VALUES($1,$2,0)"#,
            &[&pilot,&format!("Reconnect test {pilot}")]).await.unwrap();
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let available=Arc::new(AtomicBool::new(true));
        let app=format!("heartbeat_recovery_{pilot}");
        let proxy_url=format!("{}?application_name={app}",f.url.replace("127.0.0.1:55432",&listener.local_addr().unwrap().to_string()));
        let accepting=available.clone();
        let (stalled,stall)=tokio::sync::watch::channel(false);
        let proxy=tokio::spawn(async move {
            let mut copies=tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted=listener.accept()=>{
                        let (mut inbound,_)=accepted.unwrap();
                        if !accepting.load(Ordering::SeqCst) {continue;}
                        let mut stall=stall.clone();
                        copies.spawn(async move {
                            let mut backend=tokio::net::TcpStream::connect("127.0.0.1:55432").await.unwrap();
                            let (mut upstream,mut reply)=inbound.split();
                            let (mut downstream,mut request)=backend.split();
                            let receiving=async {
                                let mut raw=[0u8;8192];
                                loop {
                                    let n=downstream.read(&mut raw).await?;
                                    if n==0 {return Ok::<(),std::io::Error>(());}
                                    while *stall.borrow_and_update() {
                                        if stall.changed().await.is_err() {return Ok(());}
                                    }
                                    reply.write_all(&raw[..n]).await?;
                                }
                            };
                            let _=tokio::try_join!(tokio::io::copy(&mut upstream,&mut request),receiving);
                        });
                    }
                    _=copies.join_next(), if !copies.is_empty()=>{},
                }
            }
        });
        let original_url=f.url.clone();
        let server=Arc::new(Server {table:Default::default(),db:crate::db::Shared::new(f.db,f.url.clone()),
            db_url:proxy_url,node_id:1,bulk:f.bulk,dogma:f.dogma,world:f.world,services:crate::svc::registry(),
            dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0});
        let system=30_000_001;
        let mut park=Park::new(system,vec![],crate::destiny::tick_now());
        park.ships.insert(pilot as i64,Ship::new(pilot as i64,10,ShipStats::new(10.0,1.0e6,1.0,300.0,1.0),Pilot{
            type_id:587,group_id:25,name:"Recovery flight".into(),char_id:pilot as i64,corp_id:1000002,
            alliance_id:None,security:0.0,bounty:0.0},Vec3::ZERO,Vec3::new(100.0,0.0,0.0)));
        server.world.lock().parks.insert(system,park);
        let admin=server.db.lease().await.unwrap();
        let worker=tokio::spawn(run(server.clone()));
        Self {server,admin,original_url,pilot,system,app,available,stalled,worker:Some(worker),proxy:Some(proxy)}
    }
    async fn backend_pid(&self)->i32 {
        tokio::time::timeout(Duration::from_secs(5),async {
            loop {
                if let Some(row)=self.admin.query_opt("SELECT pid FROM pg_stat_activity WHERE application_name=$1",&[&self.app]).await.unwrap() {
                    break row.get::<_,i32>(0);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.unwrap()
    }
    fn tick(&self)->i64 {self.server.world.lock().parks[&self.system].tick}
    fn enqueue(&self,delta:f64) {self.server.world.lock().pending_jobs.push(Job::Security {char_id:self.pilot as i64,delta});}
    async fn rating(&self)->f64 {
        self.admin.query_one(r#"SELECT "securityRating" FROM "character_" WHERE "characterID"=$1"#,&[&self.pilot]).await.unwrap().get(0)
    }
    async fn wait_rating(&self,expected:f64) {
        tokio::time::timeout(Duration::from_secs(10),async {
            while self.rating().await!=expected {tokio::time::sleep(Duration::from_millis(30)).await;}
        }).await.unwrap();
    }
    async fn finish(mut self) {
        let worker=self.worker.take().unwrap();worker.abort();let _=worker.await;
        let stopped=self.tick();
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert_eq!(self.tick(),stopped,"cancelling heartbeat also cancels its simulation future");
        let proxy=self.proxy.take().unwrap();proxy.abort();let _=proxy.await;
        self.admin.execute(r#"DELETE FROM "character_" WHERE "characterID"=$1"#,&[&self.pilot]).await.unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(task)=self.worker.take(){task.abort();}
        if let Some(task)=self.proxy.take(){task.abort();}
    }
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE at the disposable 127.0.0.1:55432 database"]
async fn outage_keeps_space_advancing_and_dispatches_waiting_jobs_once_after_recovery() {
    let f=Fixture::new().await;
    let pid=f.backend_pid().await;
    f.available.store(false,Ordering::SeqCst);
    f.admin.query_one("SELECT pg_terminate_backend($1)",&[&pid]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let before=f.tick();f.enqueue(1.25);
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert!(f.tick()>before,"space keeps advancing during the outage");
    assert!(f.server.world.lock().pending_jobs.is_empty(),"simulation transferred the job to its persistence queue");
    assert_eq!(f.rating().await,0.0);
    f.available.store(true,Ordering::SeqCst);
    f.wait_rating(1.25).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(f.rating().await,1.25);
    f.finish().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE at the disposable 127.0.0.1:55432 database"]
async fn established_query_and_lost_acknowledgement_do_not_stall_space_or_replay_mutations() {
    let f=Fixture::new().await;
    let pid=f.backend_pid().await;
    let mut blocker=crate::db::connect(&f.original_url).await.unwrap();
    let tx=blocker.transaction().await.unwrap();
    tx.query_one(r#"SELECT "characterID" FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#,&[&f.pilot]).await.unwrap();
    f.enqueue(1.25);
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            if f.admin.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock' AND query LIKE '%securityRating%')",&[&pid]).await.unwrap().get::<_,bool>(0) {break;}
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    let before=f.tick();
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert!(f.tick()>before,"a dispatched query waiting on a row lock must not block space");
    // Let PostgreSQL execute and commit the mutation but suppress its response.
    // The worker cannot know it succeeded and must not dispatch it a second time.
    f.stalled.send(true).unwrap();
    tx.commit().await.unwrap();
    f.wait_rating(1.25).await;
    f.enqueue(0.25);let before=f.tick();
    tokio::time::sleep(Duration::from_millis(1400)).await;
    assert!(f.tick()>before,"an established connection blackhole must not block space");
    assert_eq!(f.rating().await,1.25,"the next job stays queued while the first acknowledgement is withheld");
    f.stalled.send(false).unwrap();
    f.wait_rating(1.5).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(f.rating().await,1.5,"neither mutation is replayed");
    f.finish().await;
}
