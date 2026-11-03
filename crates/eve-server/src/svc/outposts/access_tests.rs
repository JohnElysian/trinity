use super::*;

struct Fixture {db:crate::test_support::Database,station:i32,manager:i32,visitor:i32}
impl Fixture {
    async fn new()->Self {
        let db=crate::test_support::Database::load().await;
        let (station,manager,visitor)=(db.id().await,db.id().await,db.id().await);
        for (pilot,corp) in [(manager,1000167),(visitor,1000002)] {
            db.db.execute(r#"INSERT INTO "character_"("characterID","characterName","corporationID","securityRating") VALUES($1,$2,$3,1)"#,
                &[&pilot,&format!("Access fixture {pilot}"),&corp]).await.unwrap();
        }
        db.db.execute(r#"INSERT INTO "chrCorporationRoles"("characterID","rolesAtAll") VALUES($1,$2)"#,&[&manager,&ROLE_DIRECTOR]).await.unwrap();
        db.db.execute(r#"INSERT INTO "staStations"("stationID","stationName","corporationID","solarSystemID","constellationID","regionID")
            VALUES($1,'Access fixture station',1000167,30000141,20000020,10000002)"#,&[&station]).await.unwrap();
        db.db.execute(r#"UPDATE "character_" SET "stationID"=$2 WHERE "characterID"=$1"#,&[&manager,&station]).await.unwrap();
        Self {db,station,manager,visitor}
    }
    fn session(&self)->crate::session::Session {
        crate::session::Session{char_id:Some(self.manager as i64),corp_id:Some(1000167),station_id:Some(self.station as i64),roles_at_all:ROLE_DIRECTOR,..Default::default()}
    }
    fn settings(&self)->Vec<Value> {
        vec![Value::Dict(vec![(Value::Int(2097152),Value::Dict(vec![(Value::bytes("minimumStanding"),Value::Double(5.0)),
            (Value::bytes("minimumCharSecurity"),Value::Double(-10.0)),(Value::bytes("maximumCharSecurity"),Value::Double(10.0)),
            (Value::bytes("minimumCorpSecurity"),Value::Double(-10.0)),(Value::bytes("maximumCorpSecurity"),Value::Double(10.0))]))]),
            Value::List(vec![]),Value::List(vec![]),Value::text("Access fixture renamed"),Value::text("New access policy"),
            Value::Double(0.0),Value::Int(100),Value::Double(0.05),Value::None,Value::None]
    }
    async fn cleanup(&self) {
        self.db.db.execute(r#"DELETE FROM "stationServiceRules" WHERE "stationID"=$1"#,&[&self.station]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "staStations" WHERE "stationID"=$1"#,&[&self.station]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "chrCorporationRoles" WHERE "characterID"=$1"#,&[&self.manager]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "character_" WHERE "characterID"=ANY($1)"#,&[&vec![self.manager,self.visitor]]).await.unwrap();
    }
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn station_access_fails_closed_and_rechecks_rules_after_waiting_for_a_lock() {
    let f=Fixture::new().await;
    assert!(may_use(&f.db.db,f.station as i64,2097152,f.visitor as i64).await);
    assert!(!may_use(&f.db.db,-1,2097152,f.visitor as i64).await);
    assert!(!may_use(&f.db.db,f.station as i64,2097152,-1).await);
    let mut db=crate::db::connect(&f.db.url).await.unwrap();
    let tx=db.transaction().await.unwrap();
    assert!(tx.batch_execute("SELECT 1/0").await.is_err());
    assert!(may_use_checked(&tx,f.station as i64,2097152,f.visitor as i64).await.is_err());
    assert!(!may_use(&tx,f.station as i64,2097152,f.visitor as i64).await);
    tx.rollback().await.unwrap();
    f.db.db.execute(r#"INSERT INTO "stationServiceRules"("stationID","serviceID") VALUES($1,2097152)"#,&[&f.station]).await.unwrap();
    let mut blocker=crate::db::connect(&f.db.url).await.unwrap();let lock=blocker.transaction().await.unwrap();
    lock.execute(r#"UPDATE "stationServiceRules" SET "minimumStanding"=5 WHERE "stationID"=$1"#,&[&f.station]).await.unwrap();
    let tx=db.transaction().await.unwrap();
    {
        let mut check=std::pin::pin!(may_use_locked(&tx,f.station as i64,2097152,f.visitor as i64));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50),&mut check).await.is_err());
        lock.commit().await.unwrap();assert!(!check.await.unwrap());
    }
    assert!(may_use_locked(&tx,f.station as i64,2097152,f.manager as i64).await.unwrap());
    drop(tx);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn station_management_rolls_back_all_fields_and_rejects_revoked_directors() {
    let f=Fixture::new().await;let session=f.session();let ctx=f.db.ctx(&session);let settings=f.settings();
    let trigger=format!("reject_station_rules_{}",f.station);
    f.db.db.batch_execute(&format!(r#"CREATE FUNCTION {trigger}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        IF NEW."stationID"={} THEN RAISE EXCEPTION 'forced station rule failure'; END IF; RETURN NEW; END $$;
        CREATE TRIGGER {trigger} BEFORE INSERT OR UPDATE ON "stationServiceRules" FOR EACH ROW EXECUTE FUNCTION {trigger}();"#,f.station)).await.unwrap();
    let result=update(&ctx,f.station as i64,&settings).await;
    f.db.db.batch_execute(&format!(r#"DROP TRIGGER {trigger} ON "stationServiceRules"; DROP FUNCTION {trigger}();"#)).await.unwrap();
    assert!(result.is_err());
    assert_eq!(f.db.db.query_one(r#"SELECT "stationName" FROM "staStations" WHERE "stationID"=$1"#,&[&f.station]).await.unwrap().get::<_,String>(0),"Access fixture station");
    assert!(may_use(&f.db.db,f.station as i64,2097152,f.visitor as i64).await);
    update(&ctx,f.station as i64,&settings).await.unwrap();
    assert!(!may_use(&f.db.db,f.station as i64,2097152,f.visitor as i64).await);
    f.db.db.execute(r#"UPDATE "chrCorporationRoles" SET "rolesAtAll"=0 WHERE "characterID"=$1"#,&[&f.manager]).await.unwrap();
    assert!(update(&ctx,f.station as i64,&settings).await.unwrap_err().contains("current station managers"));
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn accepted_docking_lock_serializes_even_previously_absent_station_rules() {
    let f=Fixture::new().await;let session=f.session();let ctx=f.db.ctx(&session);let settings=f.settings();
    let mut db=crate::db::connect(&f.db.url).await.unwrap();let tx=db.transaction().await.unwrap();
    assert!(may_use_locked(&tx,f.station as i64,2097152,f.visitor as i64).await.unwrap());
    let mut saving=std::pin::pin!(update(&ctx,f.station as i64,&settings));
    assert!(tokio::time::timeout(std::time::Duration::from_millis(50),&mut saving).await.is_err());
    tx.commit().await.unwrap();saving.await.unwrap();
    assert!(!may_use(&f.db.db,f.station as i64,2097152,f.visitor as i64).await);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn station_access_serializes_absent_standing_inserts_and_existing_updates_without_row_lock_inversion(){
    let f=Fixture::new().await;
    let owner=f.db.id().await;
    f.db.db.execute(r#"INSERT INTO corporation("corporationID","corporationName") VALUES($1,'Standing access fixture')"#,&[&owner]).await.unwrap();
    f.db.db.execute(r#"UPDATE "staStations" SET "corporationID"=$2 WHERE "stationID"=$1"#,&[&f.station,&owner]).await.unwrap();
    f.db.db.execute(r#"INSERT INTO "stationServiceRules"("stationID","serviceID","minimumStanding") VALUES($1,2097152,0)"#,&[&f.station]).await.unwrap();
    let mut arriving=crate::db::connect(&f.db.url).await.unwrap();
    let writer=crate::db::connect(&f.db.url).await.unwrap();
    let reader_pid:i32=arriving.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    let writer_pid:i32=writer.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    let tx=arriving.transaction().await.unwrap();
    assert!(may_use_locked(&tx,f.station as i64,2097152,f.visitor as i64).await.unwrap());
    {
        let params:[&(dyn tokio_postgres::types::ToSql+Sync);2]=[&f.visitor,&owner];
        let mut insert=std::pin::pin!(writer.execute(r#"INSERT INTO "ownerStandings"("fromID","toID",standing,reason,"setAt") VALUES($2,$1,-5,'access race',0)"#,&params));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50),&mut insert).await.is_err(),"even an absent pair is guarded");
        assert!(f.db.db.query_one("SELECT $1=ANY(pg_blocking_pids($2))",&[&reader_pid,&writer_pid]).await.unwrap().get::<_,bool>(0));
        tx.commit().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5),&mut insert).await.unwrap().unwrap();
    }
    let tx=arriving.transaction().await.unwrap();
    assert!(!may_use_locked(&tx,f.station as i64,2097152,f.visitor as i64).await.unwrap());
    {
        let params:[&(dyn tokio_postgres::types::ToSql+Sync);2]=[&f.visitor,&owner];
        let mut update=std::pin::pin!(writer.execute(r#"UPDATE "ownerStandings" SET standing=5 WHERE "fromID"=$2 AND "toID"=$1"#,&params));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50),&mut update).await.is_err());
        assert!(f.db.db.query_one("SELECT $1=ANY(pg_blocking_pids($2))",&[&reader_pid,&writer_pid]).await.unwrap().get::<_,bool>(0));
        // Writer owns the row lock while waiting for our advisory lock. The
        // reader must still finish a second check instead of acquiring that row.
        assert!(!tokio::time::timeout(std::time::Duration::from_secs(1),may_use_locked(&tx,f.station as i64,2097152,f.visitor as i64)).await.unwrap().unwrap());
        tx.commit().await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5),&mut update).await.unwrap().unwrap();
    }
    // Conversely, a writer that already holds the key wins: the reader waits
    // and then queries the new committed value, not a pre-wait snapshot.
    writer.batch_execute("BEGIN").await.unwrap();
    writer.execute(r#"UPDATE "ownerStandings" SET standing=-8 WHERE "fromID"=$2 AND "toID"=$1"#,&[&f.visitor,&owner]).await.unwrap();
    let tx=arriving.transaction().await.unwrap();
    {
        let mut check=std::pin::pin!(may_use_locked(&tx,f.station as i64,2097152,f.visitor as i64));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50),&mut check).await.is_err());
        assert!(f.db.db.query_one("SELECT $1=ANY(pg_blocking_pids($2))",&[&writer_pid,&reader_pid]).await.unwrap().get::<_,bool>(0));
        writer.batch_execute("COMMIT").await.unwrap();
        assert!(!tokio::time::timeout(std::time::Duration::from_secs(5),&mut check).await.unwrap().unwrap());
    }
    tx.rollback().await.unwrap();
    f.db.db.execute(r#"DELETE FROM "ownerStandings" WHERE "fromID"=$2 AND "toID"=$1"#,&[&f.visitor,&owner]).await.unwrap();
    f.cleanup().await;
    f.db.db.execute(r#"DELETE FROM corporation WHERE "corporationID"=$1"#,&[&owner]).await.unwrap();
}
