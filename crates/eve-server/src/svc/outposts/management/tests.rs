use super::*;
use crate::{session::Session,test_support::Database};

struct Fixture {db:Database,station:i32,station_type:i32,pilot:i32,corp:i32,system:i32,old_level:Option<i32>}
impl Fixture {
    async fn new()->Self {
        let db=Database::load().await;
        let (pilot,corp,station_type)=(db.id().await,db.id().await,db.id().await);
        let station=db.db.query_one(r#"SELECT candidate FROM generate_series(61000000,61009999) candidate WHERE NOT EXISTS(SELECT 1 FROM "staStations" WHERE "stationID"=candidate) ORDER BY candidate LIMIT 1"#,&[]).await.unwrap().get::<_,i32>(0);
        let system=30000141;
        let old_level=db.db.query_one(r#"SELECT "sovereigntyLevel" FROM "mapSolarSystems" WHERE "solarSystemID"=$1"#,&[&system]).await.unwrap().get(0);
        db.db.execute(r#"UPDATE "mapSolarSystems" SET "sovereigntyLevel"=3 WHERE "solarSystemID"=$1"#,&[&system]).await.unwrap();
        db.db.execute(r#"INSERT INTO corporation("corporationID","corporationName","ceoID") VALUES($1,'Station manager fixture',1)"#,&[&corp]).await.unwrap();
        db.db.execute(r#"INSERT INTO "character_"("characterID","characterName","corporationID","stationID","solarSystemID") VALUES($1,$5,$2,$3,$4)"#,&[&pilot,&corp,&station,&system,&format!("Station manager fixture {pilot}")]).await.unwrap();
        db.db.execute(r#"INSERT INTO "chrCorporationRoles"("characterID","rolesAtAll") VALUES($1,2048)"#,&[&pilot]).await.unwrap();
        db.db.execute(r#"INSERT INTO "staStationTypes"("stationTypeID","officeSlots") VALUES($1,3)"#,&[&station_type]).await.unwrap();
        db.db.execute(r#"INSERT INTO "staStations"("stationID","stationName","stationTypeID","corporationID","solarSystemID","constellationID","regionID",x,y,z,"officeRentalCost","dockingCostPerVolume")
            VALUES($1,'Original manager station',$2,$3,$4,20000020,10000002,0,0,0,100,0)"#,&[&station,&station_type,&corp,&system]).await.unwrap();
        Self{db,station,station_type,pilot,corp,system,old_level}
    }
    fn session(&self)->Session {Session{char_id:Some(self.pilot as i64),corp_id:Some(self.corp as i64),station_id:Some(self.station as i64),roles_at_all:0,..Default::default()}}
    async fn call(&self,method:&str,args:&[Value])->Refused<Value> {super::super::station_mgr(&self.db.ctx(&self.session()),method,args,self.station as i64).await.unwrap()}
    async fn cleanup(self) {
        self.db.db.execute(r#"DELETE FROM "rentalInfo" WHERE "stationID"=$1"#,&[&self.station]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "stationServiceRules" WHERE "stationID"=$1"#,&[&self.station]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "entityStatic" WHERE "itemID"=$1"#,&[&self.station]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "staStations" WHERE "stationID"=$1"#,&[&self.station]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "staStationTypes" WHERE "stationTypeID"=$1"#,&[&self.station_type]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "chrCorporationRoles" WHERE "characterID"=$1"#,&[&self.pilot]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM "character_" WHERE "characterID"=$1"#,&[&self.pilot]).await.unwrap();
        self.db.db.execute(r#"DELETE FROM corporation WHERE "corporationID"=$1"#,&[&self.corp]).await.unwrap();
        self.db.db.execute(r#"UPDATE "mapSolarSystems" SET "sovereigntyLevel"=$2 WHERE "solarSystemID"=$1"#,&[&self.system,&self.old_level]).await.unwrap();
    }
}
fn settings()->Vec<Value> {vec![Value::Dict(vec![]),Value::List(vec![]),Value::List(vec![]),Value::text("Trinity station"),Value::text("Exact native request"),Value::Double(12.375),Value::Int(1234567),Value::Double(0.025),Value::Int(118),Value::None]}
fn rental(number:i64,public:bool)->Value {key_val(vec![("typeID",Value::Int(26)),("number",Value::Int(number)),("publiclyAvailable",Value::Bool(public))])}
fn rule()->Value {Value::Dict(vec![(Value::Int(2097152),key_val(vec![("serviceID",Value::Int(2097152)),("minimumStanding",Value::Double(4.5)),
    ("minimumCharSecurity",Value::Double(-5.0)),("maximumCharSecurity",Value::Double(9.0)),("minimumCorpSecurity",Value::Double(-4.0)),("maximumCorpSecurity",Value::Double(8.0))]))])}

#[test]
fn original_ten_argument_decoder_rejects_truncation_nonfinite_and_wrong_units() {
    assert!(Settings::parse(&settings()).is_ok());
    for count in [0,5,9] {assert!(Settings::parse(&settings()[..count]).is_err());}
    for (index,value) in [(5,Value::Double(f64::NAN)),(5,Value::Double(1000.01)),(5,Value::Double(-0.1)),(6,Value::Double(1.1)),
        (6,Value::Long(i32::MAX as i64+1)),(7,Value::Double(2.0)),(7,Value::Double(f64::INFINITY)),(8,Value::Int(5)),
        (9,Value::Double(134_381_000_000_000_001.0)),(9,Value::Int(-1)),(3,Value::text(" "))] {
        let mut args=settings();args[index]=value;assert!(Settings::parse(&args).is_err(),"invalid index {index}");
    }
    let mut args=settings();args[0]=rule();args[1]=Value::List(vec![key_val(vec![("serviceID",Value::Int(2097152)),("discountPerGoodStandingPoint",Value::Double(0.125)),("surchargePerBadStandingPoint",Value::Double(9.5))])]);args[2]=Value::List(vec![rental(1,false)]);assert!(Settings::parse(&args).is_ok());
    args[2]=Value::List(vec![rental(1,false),rental(1,true)]);assert!(Settings::parse(&args).is_err());
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn native_station_settings_preserve_precision_capital_and_stable_rental_metadata() {
    let f=Fixture::new().await;let stamp=134_381_000_000_000_001i64;
    let initial=f.call("GetStationDetails",&[Value::Int(f.station as i64)]).await.unwrap();assert_eq!(field(&initial,"capitalStation"),Some(&Value::None));
    let first=f.call("GetRentableItems",&[]).await.unwrap();let rows=first.seq().unwrap();assert_eq!(rows.len(),3);
    for (n,row) in rows.iter().enumerate(){assert_eq!(field(row,"number"),Some(&Value::Int(n as i64+1)));assert_eq!(field(row,"typeID"),Some(&Value::Int(26)));assert_eq!(field(row,"rentedToID"),Some(&Value::None));assert_eq!(field(row,"startDate"),Some(&Value::None));}
    f.db.db.execute(r#"UPDATE "rentalInfo" SET "renterID"=$2,"startDate"=$3,"balanceDueDate"=$4,"periodCost"=654.25,"rentPeriodInDays"=30,"billID"=123 WHERE "stationID"=$1 AND "slotNumber"=2"#,&[&f.station,&f.corp,&stamp,&(stamp+1000)]).await.unwrap();
    let mut args=settings();args[0]=rule();args[9]=Value::Long(stamp);args[1]=Value::List(vec![key_val(vec![("serviceID",Value::Int(2097152)),("discountPerGoodStandingPoint",Value::Double(0.125)),("surchargePerBadStandingPoint",Value::Double(1.75))])]);
    args[2]=Value::List(vec![key_val(vec![("typeID",Value::Int(26)),("number",Value::Int(2)),("publiclyAvailable",Value::Int(0)),("rentedToID",Value::Int(1)),("periodCost",Value::Double(1.0)),("startDate",Value::Int(1)),("balanceDueDate",Value::Int(1))])]);
    assert_eq!(f.call("UpdateStationManagementSettings",&args).await.unwrap(),Value::None);
    let actual=f.call("GetStationDetails",&[Value::Int(f.station as i64)]).await.unwrap();
    for (name,value) in [("dockingCostPerVolume",Value::Double(12.375)),("officeRentalCost",Value::Int(1234567)),("reprocessingStationsTake",Value::Double(0.025)),("reprocessingHangarFlag",Value::Int(118)),("capitalStation",Value::Long(stamp))] {assert_eq!(field(&actual,name),Some(&value),"{name}");}
    let access=f.call("GetStationServiceAccessRule",&[Value::Int(f.station as i64),Value::Int(2097152)]).await.unwrap();assert_eq!(field(&access,"minimumStanding"),Some(&Value::Double(4.5)));
    let modifiers=f.call("GetStationManagementServiceCostModifiers",&[Value::Int(f.station as i64)]).await.unwrap();
    let modifier=modifiers.seq().unwrap().iter().find(|r|field(r,"serviceID")==Some(&Value::Int(2097152))).unwrap();assert_eq!(field(modifier,"discountPerGoodStandingPoint"),Some(&Value::Double(0.125)));assert_eq!(field(modifier,"surchargePerBadStandingPoint"),Some(&Value::Double(1.75)));
    assert!(f.call("GetOwnerIDsOfClonesAtStation",&[Value::Int(-1)]).await.unwrap().seq().unwrap().is_empty());
    assert!(f.call("RevokeCloneContractsAtStation",&[Value::Long(i32::MAX as i64+1),Value::Int(-1)]).await.is_err());
    assert_eq!(f.call("RevokeCloneContractsAtStation",&[Value::Int(f.corp as i64),Value::Int(-1)]).await.unwrap(),Value::None);
    let later=f.call("GetRentableItems",&[]).await.unwrap();let rows=later.seq().unwrap();assert_eq!(rows.len(),3);let row=&rows[1];
    for (name,value) in [("number",Value::Int(2)),("rentedToID",Value::Int(f.corp as i64)),("publiclyAvailable",Value::Int(0)),("periodCost",Value::Double(654.25)),("startDate",Value::Long(stamp)),("balanceDueDate",Value::Long(stamp+1000))] {assert_eq!(field(row,name),Some(&value),"echoed {name} cannot rewrite the lease");}
    f.db.db.execute(r#"UPDATE "mapSolarSystems" SET "sovereigntyLevel"=0 WHERE "solarSystemID"=$1"#,&[&f.system]).await.unwrap();
    f.call("UpdateStationManagementSettings",&args).await.unwrap();
    args[8]=Value::None;args[9]=Value::None;f.call("UpdateStationManagementSettings",&args).await.unwrap();
    let actual=f.call("GetStationDetails",&[Value::Int(f.station as i64)]).await.unwrap();assert_eq!(field(&actual,"capitalStation"),Some(&Value::None));assert_eq!(field(&actual,"reprocessingHangarFlag"),Some(&Value::None));
    args[9]=Value::Long(stamp);assert!(f.call("UpdateStationManagementSettings",&args).await.is_err(),"a new capital designation requires current eligible sovereignty");
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn station_settings_rentable_validation_and_commit_failure_roll_back_every_change() {
    let f=Fixture::new().await;f.call("GetRentableItems",&[]).await.unwrap();
    f.db.db.execute(r#"INSERT INTO "rentalInfo"("stationID","slotNumber","typeID","renterID","publiclyAvailable") VALUES($1,4,26,$2,0)"#,&[&f.station,&f.corp]).await.unwrap();
    assert_eq!(f.call("GetRentableItems",&[]).await.unwrap().seq().unwrap().len(),4,"historical leases remain readable beyond current capacity");
    let mut args=settings();args[2]=Value::List(vec![rental(1,false),rental(4,true)]);
    assert!(f.call("UpdateStationManagementSettings",&args).await.is_err());
    assert_eq!(f.db.db.query_one(r#"SELECT "publiclyAvailable" FROM "rentalInfo" WHERE "stationID"=$1 AND "slotNumber"=1"#,&[&f.station]).await.unwrap().get::<_,i16>(0),1);
    args[2]=Value::List(vec![key_val(vec![("typeID",Value::Int(27)),("number",Value::Int(1)),("publiclyAvailable",Value::Int(0))])]);assert!(f.call("UpdateStationManagementSettings",&args).await.is_err());
    args[2]=Value::List(vec![rental(1,false)]);args[0]=rule();
    let trigger=format!("station_settings_commit_fail_{}",f.pilot);
    f.db.db.batch_execute(&format!(r#"CREATE FUNCTION {trigger}() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'forced station commit failure'; END $$;
        CREATE CONSTRAINT TRIGGER {trigger} AFTER UPDATE ON "staStations" DEFERRABLE INITIALLY DEFERRED FOR EACH ROW WHEN(NEW."stationID"={}) EXECUTE FUNCTION {trigger}();"#,f.station)).await.unwrap();
    assert!(f.call("UpdateStationManagementSettings",&args).await.is_err());
    f.db.db.batch_execute(&format!(r#"DROP TRIGGER {trigger} ON "staStations"; DROP FUNCTION {trigger}();"#)).await.unwrap();
    assert_eq!(f.db.db.query_one(r#"SELECT "stationName" FROM "staStations" WHERE "stationID"=$1"#,&[&f.station]).await.unwrap().get::<_,String>(0),"Original manager station");
    assert_eq!(f.db.db.query_one(r#"SELECT "publiclyAvailable" FROM "rentalInfo" WHERE "stationID"=$1 AND "slotNumber"=1"#,&[&f.station]).await.unwrap().get::<_,i16>(0),1);
    assert!(f.db.db.query_opt(r#"SELECT 1 FROM "stationServiceRules" WHERE "stationID"=$1"#,&[&f.station]).await.unwrap().is_none());
    f.call("UpdateStationManagementSettings",&args).await.unwrap();f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn station_management_rechecks_role_docked_custody_membership_and_owner_after_waits() {
    let f=Fixture::new().await;let args=settings();
    let lost=f.db.id().await;
    f.db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,587,$2,$3,0,1,1)"#,&[&lost,&f.pilot,&f.system]).await.unwrap();
    f.db.db.execute(r#"INSERT INTO "pendingShipLosses"("shipID","characterID","solarSystemID","shipTypeID",occupied,"createdAt",loss)
        VALUES($1,$2,$3,587,true,0,'{}')"#,&[&lost,&f.pilot,&f.system]).await.unwrap();
    assert!(f.call("UpdateStationManagementSettings",&args).await.unwrap_err().contains("pending"),"direct calls observe durable lifecycle quarantine");
    f.db.db.execute(r#"DELETE FROM "pendingShipLosses" WHERE "shipID"=$1"#,&[&lost]).await.unwrap();
    f.db.db.execute(r#"DELETE FROM entity WHERE "itemID"=$1"#,&[&lost]).await.unwrap();
    let mut stale=f.session();stale.roles_at_all=ROLE_STATION_MANAGER;
    let mut blocker=crate::db::connect(&f.db.url).await.unwrap();
    let scenarios=[("role",0), ("station",f.station+1), ("corporation",1000002), ("owner",1000002)];
    for (change,value) in scenarios {
        let tx=blocker.transaction().await.unwrap();
        let (sql,params):(String,Vec<&(dyn tokio_postgres::types::ToSql+Sync)>)=match change {
            "role"=>(r#"UPDATE "chrCorporationRoles" SET "rolesAtAll"=0 WHERE "characterID"=$1"#.into(),vec![&f.pilot]),
            "station"=>(r#"UPDATE "character_" SET "stationID"=$2 WHERE "characterID"=$1"#.into(),vec![&f.pilot,&value]),
            "corporation"=>(r#"UPDATE "character_" SET "corporationID"=$2 WHERE "characterID"=$1"#.into(),vec![&f.pilot,&value]),
            _=>(r#"UPDATE "staStations" SET "corporationID"=$2 WHERE "stationID"=$1"#.into(),vec![&f.station,&value]),
        };
        tx.execute(&sql,&params).await.unwrap();
        let ctx=f.db.ctx(&stale);let mut call=std::pin::pin!(update(&ctx,f.station as i64,&args));
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50),&mut call).await.is_err(),"{change} must be locked");
        tx.commit().await.unwrap();assert!(call.await.is_err(),"stale {change} cannot authorize a settings change");
        f.db.db.execute(r#"UPDATE "chrCorporationRoles" SET "rolesAtAll"=2048 WHERE "characterID"=$1"#,&[&f.pilot]).await.unwrap();
        f.db.db.execute(r#"UPDATE "character_" SET "stationID"=$2,"corporationID"=$3 WHERE "characterID"=$1"#,&[&f.pilot,&f.station,&f.corp]).await.unwrap();
        f.db.db.execute(r#"UPDATE "staStations" SET "corporationID"=$2 WHERE "stationID"=$1"#,&[&f.station,&f.corp]).await.unwrap();
    }
    let mut remote=f.session();remote.station_id=None;assert!(update(&f.db.ctx(&remote),f.station as i64,&args).await.is_err());
    assert!(f.call("GetStationDetails",&[Value::Int(f.station as i64+1)]).await.is_err());
    f.call("UpdateStationManagementSettings",&args).await.unwrap();f.cleanup().await;
}
