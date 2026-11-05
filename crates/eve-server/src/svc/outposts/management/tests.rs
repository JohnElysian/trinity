use super::*;
use crate::{session::Session,test_support::Database};

struct Fixture {db:Database,station:i64,slots:i64,pilot:i64,corp:i64,system:i64}
impl Fixture {
    async fn new()->Self {
        let db=Database::load().await;
        let (pilot,corp)=(db.id().await as i64,db.id().await as i64);
        // The station type with the fewest office slots the data gives any.
        let (station_type,slots)=crate::sde::table("staStationTypes").rows()
            .filter_map(|t|Some((t.int("stationTypeID")?,t.int("officeSlots").filter(|n|*n>0)?))).min_by_key(|(_,n)|*n).unwrap();
        let station=db.store.read(|db|(61000000..=61009999).find(|candidate|db.table("staStations").get_id(*candidate).is_none())).unwrap();
        let system=30000141;
        db.store.write(|tx|->Result<(),StoreError> {
            let row=tx.table("mapSolarSystems").get_id(system).unwrap().id;
            tx.update("mapSolarSystems",row,&[("sovereigntyLevel",int(3))])?;
            tx.insert("corporation",&[("corporationID",int(corp)),("corporationName",text("Station manager fixture")),("ceoID",int(1))])?;
            tx.insert("character_",&[("characterID",int(pilot)),("characterName",text(&format!("Station manager fixture {pilot}"))),("corporationID",int(corp)),
                ("stationID",int(station)),("solarSystemID",int(system))])?;
            tx.insert("chrCorporationRoles",&[("characterID",int(pilot)),("rolesAtAll",int(2048))])?;
            tx.insert("staStations",&[("stationID",int(station)),("stationName",text("Original manager station")),("stationTypeID",int(station_type)),
                ("corporationID",int(corp)),("solarSystemID",int(system)),("constellationID",int(20000020)),("regionID",int(10000002)),
                ("x",float(0.0)),("y",float(0.0)),("z",float(0.0)),("officeRentalCost",int(100)),("dockingCostPerVolume",float(0.0))])?;
            Ok(())
        }).unwrap();
        Self{db,station,slots,pilot,corp,system}
    }
    fn session(&self)->Session {Session{char_id:Some(self.pilot),corp_id:Some(self.corp),station_id:Some(self.station),roles_at_all:0,..Default::default()}}
    async fn call(&self,method:&str,args:&[Value])->Refused<Value> {super::super::station_mgr(&self.db.ctx(&self.session()),method,args,self.station).await.unwrap()}
    /// Sets columns of the row with this primary key.
    fn set(&self,table:&str,key:&[store::Value],sets:store::Sets) {
        self.db.store.write(|tx|{let row=tx.table(table).get(key).unwrap().id;tx.update(table,row,sets)}).unwrap();
    }
    fn slot_public(&self,slot:i64)->i64 {
        self.db.store.read(|db|db.table("rentalInfo").get(&[int(self.station),int(slot)]).unwrap().int("publiclyAvailable").unwrap())
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
async fn native_station_settings_preserve_precision_capital_and_stable_rental_metadata() {
    let f=Fixture::new().await;let stamp=134_381_000_000_000_001i64;
    let initial=f.call("GetStationDetails",&[Value::Int(f.station)]).await.unwrap();assert_eq!(field(&initial,"capitalStation"),Some(&Value::None));
    let first=f.call("GetRentableItems",&[]).await.unwrap();let rows=first.seq().unwrap();assert_eq!(rows.len(),f.slots as usize);
    for (n,row) in rows.iter().enumerate(){assert_eq!(field(row,"number"),Some(&Value::Int(n as i64+1)));assert_eq!(field(row,"typeID"),Some(&Value::Int(26)));assert_eq!(field(row,"rentedToID"),Some(&Value::None));assert_eq!(field(row,"startDate"),Some(&Value::None));}
    f.set("rentalInfo",&[int(f.station),int(2)],&[("renterID",int(f.corp)),("startDate",int(stamp)),("balanceDueDate",int(stamp+1000)),("periodCost",float(654.25)),("rentPeriodInDays",int(30)),("billID",int(123))]);
    let mut args=settings();args[0]=rule();args[9]=Value::Long(stamp);args[1]=Value::List(vec![key_val(vec![("serviceID",Value::Int(2097152)),("discountPerGoodStandingPoint",Value::Double(0.125)),("surchargePerBadStandingPoint",Value::Double(1.75))])]);
    args[2]=Value::List(vec![key_val(vec![("typeID",Value::Int(26)),("number",Value::Int(2)),("publiclyAvailable",Value::Int(0)),("rentedToID",Value::Int(1)),("periodCost",Value::Double(1.0)),("startDate",Value::Int(1)),("balanceDueDate",Value::Int(1))])]);
    assert_eq!(f.call("UpdateStationManagementSettings",&args).await.unwrap(),Value::None);
    let actual=f.call("GetStationDetails",&[Value::Int(f.station)]).await.unwrap();
    for (name,value) in [("dockingCostPerVolume",Value::Double(12.375)),("officeRentalCost",Value::Int(1234567)),("reprocessingStationsTake",Value::Double(0.025)),("reprocessingHangarFlag",Value::Int(118)),("capitalStation",Value::Long(stamp))] {assert_eq!(field(&actual,name),Some(&value),"{name}");}
    let access=f.call("GetStationServiceAccessRule",&[Value::Int(f.station),Value::Int(2097152)]).await.unwrap();assert_eq!(field(&access,"minimumStanding"),Some(&Value::Double(4.5)));
    let modifiers=f.call("GetStationManagementServiceCostModifiers",&[Value::Int(f.station)]).await.unwrap();
    let modifier=modifiers.seq().unwrap().iter().find(|r|field(r,"serviceID")==Some(&Value::Int(2097152))).unwrap();assert_eq!(field(modifier,"discountPerGoodStandingPoint"),Some(&Value::Double(0.125)));assert_eq!(field(modifier,"surchargePerBadStandingPoint"),Some(&Value::Double(1.75)));
    assert!(f.call("GetOwnerIDsOfClonesAtStation",&[Value::Int(-1)]).await.unwrap().seq().unwrap().is_empty());
    assert!(f.call("RevokeCloneContractsAtStation",&[Value::Long(i32::MAX as i64+1),Value::Int(-1)]).await.is_err());
    assert_eq!(f.call("RevokeCloneContractsAtStation",&[Value::Int(f.corp),Value::Int(-1)]).await.unwrap(),Value::None);
    let later=f.call("GetRentableItems",&[]).await.unwrap();let rows=later.seq().unwrap();assert_eq!(rows.len(),f.slots as usize);let row=&rows[1];
    for (name,value) in [("number",Value::Int(2)),("rentedToID",Value::Int(f.corp)),("publiclyAvailable",Value::Int(0)),("periodCost",Value::Double(654.25)),("startDate",Value::Long(stamp)),("balanceDueDate",Value::Long(stamp+1000))] {assert_eq!(field(row,name),Some(&value),"echoed {name} cannot rewrite the lease");}
    f.set("mapSolarSystems",&[int(f.system)],&[("sovereigntyLevel",int(0))]);
    f.call("UpdateStationManagementSettings",&args).await.unwrap();
    args[8]=Value::None;args[9]=Value::None;f.call("UpdateStationManagementSettings",&args).await.unwrap();
    let actual=f.call("GetStationDetails",&[Value::Int(f.station)]).await.unwrap();assert_eq!(field(&actual,"capitalStation"),Some(&Value::None));assert_eq!(field(&actual,"reprocessingHangarFlag"),Some(&Value::None));
    args[9]=Value::Long(stamp);assert!(f.call("UpdateStationManagementSettings",&args).await.is_err(),"a new capital designation requires current eligible sovereignty");
}

#[tokio::test]
async fn station_settings_rentable_validation_rolls_back_every_change() {
    let f=Fixture::new().await;f.call("GetRentableItems",&[]).await.unwrap();
    f.db.store.write(|tx|tx.insert("rentalInfo",&[("stationID",int(f.station)),("slotNumber",int(f.slots+1)),("typeID",int(26)),("renterID",int(f.corp)),("publiclyAvailable",int(0))])).unwrap();
    assert_eq!(f.call("GetRentableItems",&[]).await.unwrap().seq().unwrap().len(),f.slots as usize+1,"historical leases remain readable beyond current capacity");
    let mut args=settings();args[2]=Value::List(vec![rental(1,false),rental(f.slots+1,true)]);
    assert!(f.call("UpdateStationManagementSettings",&args).await.is_err());
    assert_eq!(f.slot_public(1),1);
    args[2]=Value::List(vec![key_val(vec![("typeID",Value::Int(27)),("number",Value::Int(1)),("publiclyAvailable",Value::Int(0))])]);assert!(f.call("UpdateStationManagementSettings",&args).await.is_err());
    args[2]=Value::List(vec![rental(1,false)]);args[0]=rule();
    assert_eq!(f.db.store.read(|db|db.table("staStations").get_id(f.station).unwrap().text("stationName").unwrap().to_string()),"Original manager station");
    assert_eq!(f.slot_public(1),1);
    assert!(f.db.store.read(|db|db.table("stationServiceRules").find(&["stationID"],&[int(f.station)]).next().is_none()));
    f.call("UpdateStationManagementSettings",&args).await.unwrap();
    assert_eq!(f.slot_public(1),0);
}

#[tokio::test]
async fn station_management_rechecks_role_docked_custody_membership_and_owner() {
    let f=Fixture::new().await;let args=settings();
    let lost=f.db.id().await as i64;
    f.db.store.write(|tx|->Result<(),StoreError> {
        tx.insert("entity",&[("itemID",int(lost)),("typeID",int(587)),("ownerID",int(f.pilot)),("locationID",int(f.system)),("flag",int(0)),("quantity",int(1)),("singleton",int(1))])?;
        tx.insert("pendingShipLosses",&[("shipID",int(lost)),("characterID",int(f.pilot)),("solarSystemID",int(f.system)),("shipTypeID",int(587)),
            ("occupied",store::Value::Bool(true)),("createdAt",int(0)),("loss",text("{}"))])?;
        Ok(())
    }).unwrap();
    assert!(f.call("UpdateStationManagementSettings",&args).await.unwrap_err().contains("pending"),"direct calls observe durable lifecycle quarantine");
    f.db.store.write(|tx|tx.delete_where("entity",&["itemID"],&[int(lost)])).unwrap();
    let mut stale=f.session();stale.roles_at_all=ROLE_STATION_MANAGER;
    let scenarios=[("role",0), ("station",f.station+1), ("corporation",1000002), ("owner",1000002)];
    for (change,value) in scenarios {
        match change {
            "role"=>f.set("chrCorporationRoles",&[int(f.pilot)],&[("rolesAtAll",int(0))]),
            "station"=>f.set("character_",&[int(f.pilot)],&[("stationID",int(value))]),
            "corporation"=>f.set("character_",&[int(f.pilot)],&[("corporationID",int(value))]),
            _=>f.set("staStations",&[int(f.station)],&[("corporationID",int(value))]),
        }
        let ctx=f.db.ctx(&stale);
        assert!(update(&ctx,f.station,&args).await.is_err(),"stale {change} cannot authorize a settings change");
        f.set("chrCorporationRoles",&[int(f.pilot)],&[("rolesAtAll",int(2048))]);
        f.set("character_",&[int(f.pilot)],&[("stationID",int(f.station)),("corporationID",int(f.corp))]);
        f.set("staStations",&[int(f.station)],&[("corporationID",int(f.corp))]);
    }
    let mut remote=f.session();remote.station_id=None;assert!(update(&f.db.ctx(&remote),f.station,&args).await.is_err());
    assert!(f.call("GetStationDetails",&[Value::Int(f.station+1)]).await.is_err());
    f.call("UpdateStationManagementSettings",&args).await.unwrap();
}
