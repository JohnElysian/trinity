use super::*;
use crate::{services::Call,session::Session,test_support::Database,world::{Presence,Push}};

struct Fixture { db:Database, station:i32, station_type:i32, corporations:Vec<i32>, pilots:Vec<i32> }
impl Fixture {
    async fn new(slots:i32)->Self {
        let db=Database::load().await;let station=db.id().await;let station_type=db.id().await;
        db.db.execute(r#"INSERT INTO "staStationTypes"("stationTypeID","officeSlots") VALUES($1,$2)"#,&[&station_type,&slots]).await.unwrap();
        db.db.execute(r#"INSERT INTO "staStations"("stationID","stationTypeID","officeRentalCost","stationName","corporationID","solarSystemID",x,y,z) VALUES($1,$2,100,'Office rental fixture',1000002,30000141,0,0,0)"#,&[&station,&station_type]).await.unwrap();
        Self{db,station,station_type,corporations:vec![],pilots:vec![]}
    }
    async fn corporation(&mut self)->(i32,Session) {
        let corp=self.db.id().await;let pilot=self.db.id().await;
        self.db.db.execute(r#"INSERT INTO corporation("corporationID","corporationName","ceoID",balance) VALUES($1,$2,$3,1000)"#,&[&corp,&format!("Rental fixture {corp}"),&pilot]).await.unwrap();
        self.corporations.push(corp);
        let session=self.member(corp,Some(pilot),0).await;
        (corp,session)
    }
    async fn member(&mut self,corp:i32,pilot:Option<i32>,roles:i64)->Session {
        let pilot=match pilot {Some(id)=>id,None=>self.db.id().await};self.pilots.push(pilot);
        self.db.db.execute(r#"INSERT INTO "character_"("characterID","characterName","corporationID","stationID","solarSystemID") VALUES($1,$2,$3,$4,30000141)"#,&[&pilot,&format!("Rental pilot {pilot}"),&corp,&self.station]).await.unwrap();
        self.db.db.execute(r#"INSERT INTO "chrCorporationRoles"("characterID","rolesAtAll") VALUES($1,$2)"#,&[&pilot,&roles]).await.unwrap();
        Session{char_id:Some(pilot as i64),corp_id:Some(corp as i64),station_id:Some(self.station as i64),solar_system_id2:Some(30000141),roles_at_all:0,..Default::default()}
    }
    fn call(&self,method:&str,args:Vec<Value>)->Call {Call{service:crate::svc::bound::object_id(1,"corpStationMgr",&[self.station as i64]),method:method.into(),args,kwargs:vec![]}}
    async fn invoke(&self,session:&Session,method:&str,args:Vec<Value>)->Outcome {crate::svc::corp::station_mgr(&self.call(method,args),&self.db.ctx(session)).await.unwrap()}
    async fn rent(&self,session:&Session,cost:f64)->Outcome {self.invoke(session,"RentOffice",vec![Value::Double(cost)]).await}
    async fn balance(&self,corp:i32)->f64 {self.db.db.query_one(r#"SELECT balance FROM corporation WHERE "corporationID"=$1"#,&[&corp]).await.unwrap().get(0)}
    async fn offices(&self,corp:i32)->i64 {self.db.db.query_one(r#"SELECT count(*) FROM "crpOffices" WHERE "corporationID"=$1"#,&[&corp]).await.unwrap().get(0)}
    async fn journal(&self,corp:i32)->i64 {self.db.db.query_one(r#"SELECT count(*) FROM market_journal WHERE "characterID"=$1"#,&[&corp]).await.unwrap().get(0)}
    fn watch(&self,client:i64,session:&Session)->tokio::sync::mpsc::UnboundedReceiver<Push> {
        let mut w=self.db.world.lock();let rx=w.connect(client);w.set_presence(client,Some(Presence{char_id:session.char_id.unwrap(),corp_id:session.corp_id.unwrap(),solar_system_id:30000141,docked:true,station_id:session.station_id,role:0}));rx
    }
    async fn cleanup(self) {
        self.db.db.execute(r#"DELETE FROM "rentalInfo" WHERE "stationID"=$1"#,&[&self.station]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM market_journal WHERE "characterID"=ANY($1)"#,&[&self.corporations]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "crpOffices" WHERE "stationID"=$1"#,&[&self.station]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM entity WHERE "ownerID"=ANY($1) AND "typeID"=27"#,&[&self.corporations]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "chrCorporationRoles" WHERE "characterID"=ANY($1)"#,&[&self.pilots]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "character_" WHERE "characterID"=ANY($1)"#,&[&self.pilots]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM corporation WHERE "corporationID"=ANY($1)"#,&[&self.corporations]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "staStations" WHERE "stationID"=$1"#,&[&self.station]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "staStationTypes" WHERE "stationTypeID"=$1"#,&[&self.station_type]).await.unwrap();
    }
}
fn office(out:&Outcome)->Option<i64> {if let Outcome::Value(Value::Int(id))=out {Some(*id)} else {None}}
fn is_error(out:&Outcome,name:&str)->bool {matches!(out,Outcome::UserError{msg,..} if msg==name)}
async fn wait_blocked(db:&tokio_postgres::Client,blocker:i32) {
    tokio::time::timeout(std::time::Duration::from_secs(10),async {loop {
        if db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",&[&blocker]).await.unwrap().get::<_,bool>(0){break;}
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }}).await.expect("rental reaches the held row lock");
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn office_rental_native_contract_and_notifications_follow_committed_payment() {
    let mut f=Fixture::new(2).await;let (corp,ceo)=f.corporation().await;
    let director=f.member(corp,None,1).await;let (_,other)=f.corporation().await;
    let mut elsewhere=f.member(corp,None,0).await;elsewhere.station_id=Some(60003760);
    let mut caller=f.watch(1,&ceo);let mut colleague=f.watch(2,&director);let mut stranger=f.watch(3,&other);let mut remote=f.watch(4,&elsewhere);
    assert!(matches!(f.invoke(&ceo,"GetQuoteForRentingAnOffice",vec![]).await,Outcome::Value(Value::Double(100.0))),"CEO authority comes from the current corporation row, not session role bits");
    assert!(matches!(f.invoke(&director,"GetQuoteForRentingAnOffice",vec![]).await,Outcome::Value(Value::Double(100.0))),"current director can quote despite stale session role bits");
    let id=office(&f.rent(&director,125.0).await).expect("RentOffice returns the office ID");
    assert_eq!(f.balance(corp).await,900.0,"only the fresh actual fee is debited");assert_eq!(f.journal(corp).await,1);
    let row=f.db.db.query_one(r#"SELECT o."officeFolderID",e."typeID",e."ownerID",e."locationID",e.singleton,e.quantity,j.amount,j."refTypeID" FROM "crpOffices" o JOIN entity e ON e."itemID"=o."itemID" JOIN market_journal j ON j."characterID"=o."corporationID" WHERE o."itemID"=$1"#,&[&(id as i32)]).await.unwrap();
    let folder:i32=row.get(0);assert_eq!(row.get::<_,i32>(1),27);assert_eq!(row.get::<_,i32>(2),corp);assert_eq!(row.get::<_,i32>(3),f.station);assert_eq!(row.get::<_,i32>(4),1);assert_eq!(row.get::<_,i32>(5),1);assert_eq!(row.get::<_,f64>(6),-100.0);assert_eq!(row.get::<_,i32>(7),REF_OFFICE_RENTAL_FEE);
    let rental=f.db.db.query_one(r#"SELECT r."typeID",r."slotNumber",r."startDate",r."rentPeriodInDays",r."periodCost",r."balanceDueDate"
        FROM "rentalInfo" r JOIN "crpOffices" o ON o."stationID"=r."stationID" AND o."rentalSlot"=r."slotNumber" WHERE o."itemID"=$1"#,&[&(id as i32)]).await.unwrap();
    assert_eq!(rental.get::<_,i32>(0),26);assert_eq!(rental.get::<_,i32>(1),1);assert!(rental.get::<_,i64>(2)>0);
    assert_eq!(rental.get::<_,i32>(3),30);assert_eq!(rental.get::<_,f64>(4),100.0);
    assert_eq!(rental.get::<_,i64>(5)-rental.get::<_,i64>(2),30*24*60*60*10_000_000_i64);
    let display=crate::svc::corp::items_rented(&f.db.ctx(&ceo)).await.unwrap();
    let Value::Instance{state,..}=display else {panic!("rental rowset");};
    let header=state.get_str("header").and_then(Value::seq).unwrap();
    assert!(header.iter().any(|v|v.as_string().as_deref()==Some("periodCost")));assert!(header.iter().any(|v|v.as_string().as_deref()==Some("balanceDueDate")));
    for rx in [&mut caller,&mut colleague] {
        let mut rental=false;let mut wallet=false;
        while let Ok(push)=rx.try_recv() {if let Push::Notify(note)=push {
            if note.event=="OnOfficeRentalChanged" {assert_eq!(note.args,Value::Tuple(vec![Value::Int(corp as i64),Value::Int(id),Value::Int(folder as i64)]));rental=true;}
            if note.event=="OnAccountChange" {assert_eq!(note.args,Value::Tuple(vec![Value::bytes("cash"),Value::Int(corp as i64),Value::Double(900.0)]));wallet=true;}
        }}assert!(rental && wallet);
    }
    assert!(matches!(stranger.try_recv(),Ok(Push::Notify(note)) if note.event=="OnOfficeRentalChanged" && note.args==Value::Tuple(vec![Value::Int(corp as i64),Value::Int(id),Value::Int(folder as i64)])),"other corporations at this station update their office list");
    assert!(stranger.try_recv().is_err(),"other corporations do not receive this wallet update");
    assert!(matches!(remote.try_recv(),Ok(Push::Notify(note)) if note.event=="OnAccountChange"),"remote members receive their wallet update only");
    assert!(remote.try_recv().is_err(),"a different station's office cache must not receive this stationless event");
    assert!(is_error(&f.rent(&ceo,100.0).await,"RentingYouHaveAnOfficeHere"));assert_eq!(f.balance(corp).await,900.0);assert_eq!(f.journal(corp).await,1);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn concurrent_duplicate_and_final_slot_rentals_charge_only_the_winners() {
    let mut f=Fixture::new(2).await;let (corp,ceo)=f.corporation().await;let director=f.member(corp,None,1).await;
    let (a,b)=tokio::join!(f.rent(&ceo,100.0),f.rent(&director,100.0));
    assert_ne!(office(&a).is_some(),office(&b).is_some());assert!(is_error(&a,"RentingYouHaveAnOfficeHere") || is_error(&b,"RentingYouHaveAnOfficeHere"));
    assert_eq!(f.offices(corp).await,1);assert_eq!(f.balance(corp).await,900.0);assert_eq!(f.journal(corp).await,1);
    let (second,boss2)=f.corporation().await;let (third,boss3)=f.corporation().await;
    let (a,b)=tokio::join!(f.rent(&boss2,100.0),f.rent(&boss3,100.0));
    assert_ne!(office(&a).is_some(),office(&b).is_some());assert!(is_error(&a,"NoOfficesAreAvailableForRenting") || is_error(&b,"NoOfficesAreAvailableForRenting"));
    assert_eq!(f.offices(second).await+f.offices(third).await,1);assert_eq!(f.balance(second).await+f.balance(third).await,1900.0);assert_eq!(f.journal(second).await+f.journal(third).await,1);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn office_payment_and_deferred_commit_failures_leave_no_office_charge_or_notification() {
    let mut f=Fixture::new(1).await;let (corp,ceo)=f.corporation().await;let mut rx=f.watch(1,&ceo);
    f.db.db.execute("UPDATE corporation SET balance=50 WHERE \"corporationID\"=$1",&[&corp]).await.unwrap();
    assert!(is_error(&f.rent(&ceo,100.0).await,"NotEnoughMoney"));assert_eq!(f.balance(corp).await,50.0);assert_eq!(f.offices(corp).await,0);assert_eq!(f.journal(corp).await,0);assert!(rx.try_recv().is_err());
    f.db.db.execute("UPDATE corporation SET balance=1000 WHERE \"corporationID\"=$1",&[&corp]).await.unwrap();
    let guard=format!("office_ledger_fail_{corp}");
    f.db.db.batch_execute(&format!(r#"ALTER TABLE market_journal ADD CONSTRAINT {guard} CHECK("characterID"<>{corp} OR "refTypeID"<>13) NOT VALID"#)).await.unwrap();
    assert!(office(&f.rent(&ceo,100.0).await).is_none());
    f.db.db.batch_execute(&format!("ALTER TABLE market_journal DROP CONSTRAINT {guard}")).await.unwrap();
    assert_eq!(f.balance(corp).await,1000.0);assert_eq!(f.offices(corp).await,0);assert_eq!(f.journal(corp).await,0);assert!(rx.try_recv().is_err());
    let trigger=format!("office_commit_fail_{corp}");
    f.db.db.batch_execute(&format!(r#"CREATE FUNCTION {trigger}() RETURNS trigger LANGUAGE plpgsql AS $fn$ BEGIN RAISE EXCEPTION 'forced office commit failure'; END $fn$; CREATE CONSTRAINT TRIGGER {trigger} AFTER INSERT ON entity DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN(NEW."ownerID"={corp} AND NEW."typeID"=27) EXECUTE FUNCTION {trigger}();"#)).await.unwrap();
    assert!(office(&f.rent(&ceo,100.0).await).is_none());
    f.db.db.batch_execute(&format!("DROP TRIGGER {trigger} ON entity; DROP FUNCTION {trigger}()")).await.unwrap();
    assert_eq!(f.balance(corp).await,1000.0);assert_eq!(f.offices(corp).await,0);assert_eq!(f.journal(corp).await,0);assert!(rx.try_recv().is_err());
    assert_eq!(f.db.db.query_one(r#"SELECT count(*) FROM entity WHERE "ownerID"=$1 AND "typeID"=27"#,&[&corp]).await.unwrap().get::<_,i64>(0),0);
    assert_eq!(f.db.db.query_one(r#"SELECT count(*) FROM "rentalInfo" WHERE "stationID"=$1"#,&[&f.station]).await.unwrap().get::<_,i64>(0),0,"failed payments also roll back slot materialization and the rental claim");
    assert!(office(&f.rent(&ceo,100.0).await).is_some());f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn office_rental_rechecks_bound_station_membership_directorship_and_fee_after_waits() {
    let mut f=Fixture::new(2).await;let (corp,ceo)=f.corporation().await;let mut director=f.member(corp,None,1).await;director.roles_at_all=1;
    let remote=Call{service:crate::svc::bound::object_id(1,"corpStationMgr",&[(f.station+1) as i64]),..f.call("RentOffice",vec![Value::Int(100)])};
    assert!(is_error(&crate::svc::corp::station_mgr(&remote,&f.db.ctx(&director)).await.unwrap(),"RentingOfficeRequestDenied"));
    for invalid in [f64::NAN,f64::INFINITY,-1.0] {assert!(is_error(&f.rent(&director,invalid).await,"RentingOfficeRequestDenied"));}
    let mut blocker=crate::db::connect(&f.db.url).await.unwrap();
    let tx=blocker.transaction().await.unwrap();let pilot=director.char_id.unwrap() as i32;
    tx.execute(r#"UPDATE "chrCorporationRoles" SET "rolesAtAll"=0 WHERE "characterID"=$1"#,&[&pilot]).await.unwrap();let pid:i32=tx.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    let release=async{wait_blocked(&f.db.db,pid).await;tx.commit().await.unwrap();};
    let (out,())=tokio::join!(f.rent(&director,100.0),release);assert!(is_error(&out,"RentingOfficeRequestDenied"));assert_eq!(f.balance(corp).await,1000.0);
    assert!(is_error(&f.invoke(&director,"GetQuoteForRentingAnOffice",vec![]).await,"RentingOfficeQuotesOnlyGivenToActiveCEOsOrEquivale"));
    let tx=blocker.transaction().await.unwrap();tx.execute(r#"UPDATE "character_" SET "stationID"=$2 WHERE "characterID"=$1"#,&[&(ceo.char_id.unwrap() as i32),&(f.station+1)]).await.unwrap();
    let release=async{wait_blocked(&f.db.db,pid).await;tx.commit().await.unwrap();};
    let (out,())=tokio::join!(f.rent(&ceo,100.0),release);assert!(is_error(&out,"RentingOfficeRequestDenied"));
    f.db.db.execute(r#"UPDATE "character_" SET "stationID"=$2 WHERE "characterID"=$1"#,&[&(ceo.char_id.unwrap() as i32),&f.station]).await.unwrap();
    let (other_corp,_)=f.corporation().await;
    let tx=blocker.transaction().await.unwrap();tx.execute(r#"UPDATE "character_" SET "corporationID"=$2 WHERE "characterID"=$1"#,&[&(ceo.char_id.unwrap() as i32),&other_corp]).await.unwrap();
    let release=async{wait_blocked(&f.db.db,pid).await;tx.commit().await.unwrap();};
    let (out,())=tokio::join!(f.rent(&ceo,100.0),release);assert!(is_error(&out,"RentingOfficeRequestDenied"),"stale CEO/corporation session cannot spend the former corporation's wallet");
    f.db.db.execute(r#"UPDATE "character_" SET "corporationID"=$2 WHERE "characterID"=$1"#,&[&(ceo.char_id.unwrap() as i32),&corp]).await.unwrap();
    let tx=blocker.transaction().await.unwrap();tx.execute(r#"UPDATE "staStations" SET "officeRentalCost"=125 WHERE "stationID"=$1"#,&[&f.station]).await.unwrap();
    let release=async{wait_blocked(&f.db.db,pid).await;tx.commit().await.unwrap();};
    let (out,())=tokio::join!(f.rent(&ceo,100.0),release);assert!(is_error(&out,"RentingAnOfficeCostsMore"));assert_eq!(f.offices(corp).await,0);assert_eq!(f.journal(corp).await,0);
    f.db.db.execute(r#"UPDATE "staStations" SET "officeRentalCost"=75 WHERE "stationID"=$1"#,&[&f.station]).await.unwrap();
    assert!(office(&f.rent(&ceo,100.0).await).is_some());assert_eq!(f.balance(corp).await,925.0);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn actual_wallet_deadlock_aborts_rental_then_retry_revalidates_the_changed_fee() {
    let mut f=Fixture::new(1).await;let (corp,ceo)=f.corporation().await;let pilot=ceo.char_id.unwrap();
    assert!((corp as i64)<pilot,"existing wallet numeric ordering takes corporation first");
    let mut rx=f.watch(1,&ceo);
    let mut client=crate::db::connect(&f.db.url).await.unwrap();let tx=client.transaction().await.unwrap();
    // Let the rental's normal detector run first, so the actual PostgreSQL
    // deadlock victim is deterministically the operation being tested.
    tx.batch_execute("SET LOCAL deadlock_timeout='10s'").await.unwrap();
    let payment=Entry{ref_type:10,payer:corp as i64,payee:pilot,arg_id1:"",reason:"office deadlock regression"};
    assert!(wallet::post(&tx,corp as i64,-7.0,&payment).await.is_ok());
    let pid:i32=tx.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    let transfer=async {
        wait_blocked(&f.db.db,pid).await;
        // Rental owns character SHARE and awaits our corporation wallet;
        // this ordinary second wallet write closes the deadlock cycle.
        assert!(wallet::post(&tx,pilot,7.0,&payment).await.is_ok());
        tx.execute(r#"UPDATE "staStations" SET "officeRentalCost"=125 WHERE "stationID"=$1"#,&[&f.station]).await.unwrap();
        tx.commit().await.unwrap();
    };
    let (out,())=tokio::time::timeout(std::time::Duration::from_secs(15),async {tokio::join!(f.rent(&ceo,100.0),transfer)}).await.expect("confirmed aborted rental retries without retaining locks");
    assert!(is_error(&out,"RentingAnOfficeCostsMore"),"the retry must reread the changed fee rather than replay its old authorization");
    assert_eq!(f.balance(corp).await,993.0);assert_eq!(f.offices(corp).await,0);assert_eq!(f.journal(corp).await,1);assert!(rx.try_recv().is_err());
    assert!(office(&f.rent(&ceo,125.0).await).is_some());assert_eq!(f.balance(corp).await,868.0);assert_eq!(f.journal(corp).await,2);
    f.db.db.execute(r#"DELETE FROM market_journal WHERE "characterID"=$1"#,&[&(pilot as i32)]).await.unwrap();
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn private_slots_and_legacy_rentals_keep_identity_and_historical_values() {
    let mut f=Fixture::new(3).await;let (legacy,legacy_ceo)=f.corporation().await;let (corp,ceo)=f.corporation().await;
    let item=f.db.id().await;let folder=f.db.id().await;
    f.db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,27,$2,$3,0,1,1)"#,&[&item,&legacy,&f.station]).await.unwrap();
    f.db.db.execute(r#"INSERT INTO "crpOffices"("corporationID","stationID","itemID","typeID","officeFolderID") VALUES($1,$2,$3,27,$4)"#,&[&legacy,&f.station,&item,&folder]).await.unwrap();
    f.db.db.execute(r#"INSERT INTO "rentalInfo"("stationID","slotNumber","typeID","renterID","publiclyAvailable","startDate","balanceDueDate","periodCost","rentPeriodInDays")
        VALUES($1,2,26,$2,0,123,456,17.5,12),($1,1,26,0,0,0,0,0,0)"#,&[&f.station,&legacy]).await.unwrap();
    let Value::Instance{state,..}=crate::svc::corp::items_rented(&f.db.ctx(&legacy_ceo)).await.unwrap() else {panic!("original rental rowset");};
    let rows=state.get_str("lines").and_then(Value::seq).unwrap();assert_eq!(rows.len(),1);let row=rows[0].seq().unwrap();
    assert_eq!(row[2].as_i64(),Some(123));assert_eq!(row[5].as_f64(),Some(17.5));assert_eq!(row[6].as_i64(),Some(456));
    let id=office(&f.rent(&ceo,100.0).await).expect("third slot remains public");
    let rows=f.db.db.query(r#"SELECT "slotNumber","renterID","startDate","balanceDueDate","periodCost" FROM "rentalInfo" WHERE "stationID"=$1 ORDER BY "slotNumber""#,&[&f.station]).await.unwrap();
    assert_eq!(rows.len(),3);assert_eq!(rows[0].get::<_,i32>(1),0);assert_eq!(rows[1].get::<_,i32>(1),legacy);assert_eq!(rows[2].get::<_,i32>(1),corp);
    assert_eq!(rows[1].get::<_,i64>(2),123);assert_eq!(rows[1].get::<_,i64>(3),456);assert_eq!(rows[1].get::<_,f64>(4),17.5);
    let linked=f.db.db.query(r#"SELECT "itemID","rentalSlot" FROM "crpOffices" WHERE "stationID"=$1 ORDER BY "rentalSlot""#,&[&f.station]).await.unwrap();
    assert_eq!(linked[0].get::<_,i32>(0),item);assert_eq!(linked[0].get::<_,i32>(1),2);assert_eq!(linked[1].get::<_,i32>(0),id as i32);assert_eq!(linked[1].get::<_,i32>(1),3);
    let (other,caller)=f.corporation().await;
    assert!(is_error(&f.rent(&caller,100.0).await,"NoOfficesAreAvailableForRenting"));assert_eq!(f.balance(other).await,1000.0);assert_eq!(f.journal(other).await,0);
    f.cleanup().await;
}
