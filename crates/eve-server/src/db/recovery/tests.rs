use super::*;

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable database; terminates only its own backend"]
async fn reconnect_coalesces_leases_without_replaying_a_failed_transaction() {
    let url=std::env::var("TRINITY_TEST_DATABASE").expect("disposable test database");
    let admin=crate::db::connect(&url).await.unwrap();
    let first=crate::db::connect(&url).await.unwrap();
    let pid:i32=first.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    let table=format!("reconnect_probe_{pid}");
    admin.batch_execute(&format!("CREATE TABLE {table}(id integer PRIMARY KEY)")).await.unwrap();
    let shared=Arc::new(Shared::new(first,url));
    let original=shared.lease().await.unwrap();
    original.execute(&format!("INSERT INTO {table} VALUES(1)"),&[]).await.unwrap();
    // There is an uncommitted mutation on the pinned backend when it dies.
    original.batch_execute(&format!("BEGIN; INSERT INTO {table} VALUES(2)")).await.unwrap();
    assert!(admin.query_one("SELECT pg_terminate_backend($1)",&[&pid]).await.unwrap().get::<_,bool>(0));
    tokio::time::timeout(Duration::from_secs(3),async {
        while !original.is_closed() {tokio::task::yield_now().await;}
    }).await.unwrap();
    assert!(original.batch_execute("COMMIT").await.is_err());
    let attempts=(0..20).map(|_|{
        let shared=shared.clone();tokio::spawn(async move{shared.lease().await.unwrap()})
    }).collect::<Vec<_>>();
    let mut leases=Vec::new();for attempt in attempts {leases.push(attempt.await.unwrap());}
    assert!(leases.iter().all(|lease|Arc::ptr_eq(lease,&leases[0])));
    assert!(!Arc::ptr_eq(&original,&leases[0]));
    assert!(original.is_closed(),"an existing operation is never switched to a new backend");
    let ids=leases[0].query(&format!("SELECT id FROM {table} ORDER BY id"),&[]).await.unwrap();
    assert_eq!(ids.iter().map(|row|row.get::<_,i32>(0)).collect::<Vec<_>>(),vec![1]);
    admin.batch_execute(&format!("DROP TABLE {table}")).await.unwrap();
}

#[tokio::test]
async fn stalled_connection_attempt_does_not_block_worker_polls_and_is_cancelled_on_drop() {
    use tokio::io::AsyncReadExt;
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address=listener.local_addr().unwrap();
    let url=format!("host=127.0.0.1 port={} user=test dbname=test sslmode=disable",address.port());
    let mut reconnect=Reconnect::default();
    assert!(reconnect.poll(&url).await.is_none());
    let (mut socket,_)=tokio::time::timeout(Duration::from_secs(2),listener.accept()).await.unwrap().unwrap();
    let mut startup=[0u8;1024];assert!(socket.read(&mut startup).await.unwrap()>0);
    // The fake backend never answers its startup packet. Polling must not wait
    // for it; the caller remains free to advance space and retain pending work.
    tokio::time::timeout(Duration::from_millis(100),async {
        for _ in 0..100 {assert!(reconnect.poll(&url).await.is_none());tokio::task::yield_now().await;}
    }).await.unwrap();
    drop(reconnect);
    assert_eq!(tokio::time::timeout(Duration::from_secs(2),socket.read(&mut startup)).await.unwrap().unwrap(),0);
}
