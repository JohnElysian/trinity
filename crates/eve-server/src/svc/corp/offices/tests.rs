use super::*;
use crate::store::Value as V;
use crate::{services::Call,session::Session,test_support::Database,world::{Presence,Push}};

/// A station type of the static data with four office slots.
const STATION_TYPE:i64=21644;
const STATION_SLOTS:i64=4;

struct Fixture { db:Database, station:i64 }
impl Fixture {
    /// A station with `slots` free office slots: the station types are static
    /// now, so it is of a four-slot type with the slots above `slots` taken
    /// by another corporation's offices.
    async fn new(slots:i64)->Self {
        let db=Database::load().await;let station=i64::from(db.id().await);
        let f=Self{db,station};
        f.insert("staStations",&[("stationID",V::Int(station)),("stationTypeID",V::Int(STATION_TYPE)),("officeRentalCost",V::Int(100)),("stationName",V::Text("Office rental fixture".into())),
            ("corporationID",V::Int(1000002)),("solarSystemID",V::Int(30000141)),("x",V::Float(0.0)),("y",V::Float(0.0)),("z",V::Float(0.0))]);
        for slot in slots+1..=STATION_SLOTS {
            let corp=f.id().await;let item=f.id().await;let folder=f.id().await;
            f.insert("corporation",&[("corporationID",V::Int(corp)),("corporationName",V::Text(format!("Rental filler {corp}").into())),("balance",V::Float(1000.0))]);
            f.insert("entity",&[("itemID",V::Int(item)),("typeID",V::Int(27)),("ownerID",V::Int(corp)),("locationID",V::Int(station)),("flag",V::Int(0)),("quantity",V::Int(1)),("singleton",V::Int(1))]);
            f.insert("crpOffices",&[("corporationID",V::Int(corp)),("stationID",V::Int(station)),("itemID",V::Int(item)),("typeID",V::Int(27)),("officeFolderID",V::Int(folder)),("rentalSlot",V::Int(slot))]);
        }
        f
    }
    async fn id(&self)->i64 {i64::from(self.db.id().await)}
    fn insert(&self,table:&str,sets:&[(&str,V)]) {self.db.store.write(|tx|tx.insert(table,sets).map(|_|())).unwrap();}
    /// Set columns of the row whose single-column primary key is `id`.
    fn set(&self,table:&str,id:i64,sets:&[(&str,V)]) {self.db.store.write(|tx|{let row=tx.table(table).get_id(id).expect("fixture row").id;tx.update(table,row,sets)}).unwrap();}
    async fn corporation(&self)->(i64,Session) {
        let corp=self.id().await;let pilot=self.id().await;
        self.insert("corporation",&[("corporationID",V::Int(corp)),("corporationName",V::Text(format!("Rental fixture {corp}").into())),("ceoID",V::Int(pilot)),("balance",V::Float(1000.0))]);
        let session=self.member(corp,Some(pilot),0).await;
        (corp,session)
    }
    async fn member(&self,corp:i64,pilot:Option<i64>,roles:i64)->Session {
        let pilot=match pilot {Some(id)=>id,None=>self.id().await};
        self.insert("character_",&[("characterID",V::Int(pilot)),("characterName",V::Text(format!("Rental pilot {pilot}").into())),("corporationID",V::Int(corp)),("stationID",V::Int(self.station)),("solarSystemID",V::Int(30000141))]);
        self.insert("chrCorporationRoles",&[("characterID",V::Int(pilot)),("rolesAtAll",V::Int(roles))]);
        Session{char_id:Some(pilot),corp_id:Some(corp),station_id:Some(self.station),solar_system_id2:Some(30000141),roles_at_all:0,..Default::default()}
    }
    fn call(&self,method:&str,args:Vec<Value>)->Call {Call{service:crate::svc::bound::object_id(1,"corpStationMgr",&[self.station]),method:method.into(),args,kwargs:vec![]}}
    async fn invoke(&self,session:&Session,method:&str,args:Vec<Value>)->Outcome {crate::svc::corp::station_mgr(&self.call(method,args),&self.db.ctx(session)).await.unwrap()}
    async fn rent(&self,session:&Session,cost:f64)->Outcome {self.invoke(session,"RentOffice",vec![Value::Double(cost)]).await}
    fn balance(&self,corp:i64)->f64 {self.db.store.read(|db|db.table("corporation").get_id(corp).and_then(|c|c.float("balance")).unwrap())}
    fn offices(&self,corp:i64)->usize {self.db.store.read(|db|db.table("crpOffices").find(&["corporationID"],&[V::Int(corp)]).count())}
    fn journal(&self,corp:i64)->usize {self.db.store.read(|db|db.table("market_journal").find(&["characterID"],&[V::Int(corp)]).count())}
    fn watch(&self,client:i64,session:&Session)->tokio::sync::mpsc::UnboundedReceiver<Push> {
        let mut w=self.db.world.lock();let rx=w.connect(client);w.set_presence(client,Some(Presence{char_id:session.char_id.unwrap(),corp_id:session.corp_id.unwrap(),solar_system_id:30000141,docked:true,station_id:session.station_id,role:0}));rx
    }
}
fn office(out:&Outcome)->Option<i64> {if let Outcome::Value(Value::Int(id))=out {Some(*id)} else {None}}
fn is_error(out:&Outcome,name:&str)->bool {matches!(out,Outcome::UserError{msg,..} if msg==name)}

#[tokio::test]
async fn office_rental_native_contract_and_notifications_follow_committed_payment() {
    let f=Fixture::new(2).await;let (corp,ceo)=f.corporation().await;
    let director=f.member(corp,None,1).await;let (_,other)=f.corporation().await;
    let mut elsewhere=f.member(corp,None,0).await;elsewhere.station_id=Some(60003760);
    let mut caller=f.watch(1,&ceo);let mut colleague=f.watch(2,&director);let mut stranger=f.watch(3,&other);let mut remote=f.watch(4,&elsewhere);
    assert!(matches!(f.invoke(&ceo,"GetQuoteForRentingAnOffice",vec![]).await,Outcome::Value(Value::Double(100.0))),"CEO authority comes from the current corporation row, not session role bits");
    assert!(matches!(f.invoke(&director,"GetQuoteForRentingAnOffice",vec![]).await,Outcome::Value(Value::Double(100.0))),"current director can quote despite stale session role bits");
    let id=office(&f.rent(&director,125.0).await).expect("RentOffice returns the office ID");
    assert_eq!(f.balance(corp),900.0,"only the fresh actual fee is debited");assert_eq!(f.journal(corp),1);
    let folder=f.db.store.read(|db|{
        let o=db.table("crpOffices").find_one(&["itemID"],&[V::Int(id)]).unwrap();
        let e=db.table("entity").get_id(id).unwrap();
        let j=db.table("market_journal").find_one(&["characterID"],&[V::Int(corp)]).unwrap();
        assert_eq!(e.int("typeID"),Some(27));assert_eq!(e.int("ownerID"),Some(corp));assert_eq!(e.int("locationID"),Some(f.station));assert_eq!(e.int("singleton"),Some(1));assert_eq!(e.int("quantity"),Some(1));
        assert_eq!(j.float("amount"),Some(-100.0));assert_eq!(j.int("refTypeID"),Some(i64::from(REF_OFFICE_RENTAL_FEE)));
        let r=db.table("rentalInfo").get(&[V::Int(f.station),o.value("rentalSlot").clone()]).unwrap();
        assert_eq!(r.int("typeID"),Some(26));assert_eq!(r.int("slotNumber"),Some(1));assert!(r.int("startDate").unwrap()>0);
        assert_eq!(r.int("rentPeriodInDays"),Some(30));assert_eq!(r.float("periodCost"),Some(100.0));
        assert_eq!(r.int("balanceDueDate").unwrap()-r.int("startDate").unwrap(),30*24*60*60*10_000_000_i64);
        o.int("officeFolderID").unwrap()
    });
    let display=crate::svc::corp::items_rented(&f.db.ctx(&ceo)).await.unwrap();
    let Value::Instance{state,..}=display else {panic!("rental rowset");};
    let header=state.get_str("header").and_then(Value::seq).unwrap();
    assert!(header.iter().any(|v|v.as_string().as_deref()==Some("periodCost")));assert!(header.iter().any(|v|v.as_string().as_deref()==Some("balanceDueDate")));
    for rx in [&mut caller,&mut colleague] {
        let mut rental=false;let mut wallet=false;
        while let Ok(push)=rx.try_recv() {if let Push::Notify(note)=push {
            if note.event=="OnOfficeRentalChanged" {assert_eq!(note.args,Value::Tuple(vec![Value::Int(corp),Value::Int(id),Value::Int(folder)]));rental=true;}
            if note.event=="OnAccountChange" {assert_eq!(note.args,Value::Tuple(vec![Value::bytes("cash"),Value::Int(corp),Value::Double(900.0)]));wallet=true;}
        }}assert!(rental && wallet);
    }
    assert!(matches!(stranger.try_recv(),Ok(Push::Notify(note)) if note.event=="OnOfficeRentalChanged" && note.args==Value::Tuple(vec![Value::Int(corp),Value::Int(id),Value::Int(folder)])),"other corporations at this station update their office list");
    assert!(stranger.try_recv().is_err(),"other corporations do not receive this wallet update");
    assert!(matches!(remote.try_recv(),Ok(Push::Notify(note)) if note.event=="OnAccountChange"),"remote members receive their wallet update only");
    assert!(remote.try_recv().is_err(),"a different station's office cache must not receive this stationless event");
    assert!(is_error(&f.rent(&ceo,100.0).await,"RentingYouHaveAnOfficeHere"));assert_eq!(f.balance(corp),900.0);assert_eq!(f.journal(corp),1);
}

#[tokio::test]
async fn duplicate_and_final_slot_rentals_charge_only_the_winners() {
    let f=Fixture::new(2).await;let (corp,ceo)=f.corporation().await;let director=f.member(corp,None,1).await;
    let a=f.rent(&ceo,100.0).await;let b=f.rent(&director,100.0).await;
    assert!(office(&a).is_some());assert!(is_error(&b,"RentingYouHaveAnOfficeHere"));
    assert_eq!(f.offices(corp),1);assert_eq!(f.balance(corp),900.0);assert_eq!(f.journal(corp),1);
    let (second,boss2)=f.corporation().await;let (third,boss3)=f.corporation().await;
    let a=f.rent(&boss2,100.0).await;let b=f.rent(&boss3,100.0).await;
    assert!(office(&a).is_some());assert!(is_error(&b,"NoOfficesAreAvailableForRenting"));
    assert_eq!(f.offices(second)+f.offices(third),1);assert_eq!(f.balance(second)+f.balance(third),1900.0);assert_eq!(f.journal(second)+f.journal(third),1);
}

#[tokio::test]
async fn office_payment_failure_leaves_no_office_charge_or_notification() {
    let f=Fixture::new(1).await;let (corp,ceo)=f.corporation().await;let mut rx=f.watch(1,&ceo);
    f.set("corporation",corp,&[("balance",V::Float(50.0))]);
    assert!(is_error(&f.rent(&ceo,100.0).await,"NotEnoughMoney"));assert_eq!(f.balance(corp),50.0);assert_eq!(f.offices(corp),0);assert_eq!(f.journal(corp),0);assert!(rx.try_recv().is_err());
    f.db.store.read(|db|{
        assert_eq!(db.table("entity").find(&["ownerID","typeID"],&[V::Int(corp),V::Int(27)]).count(),0);
        assert_eq!(db.table("rentalInfo").find(&["stationID"],&[V::Int(f.station)]).count(),0,"failed payments also roll back slot materialization and the rental claim");
    });
    f.set("corporation",corp,&[("balance",V::Float(1000.0))]);
    assert!(office(&f.rent(&ceo,100.0).await).is_some());
}

#[tokio::test]
async fn office_rental_rechecks_bound_station_membership_directorship_and_fee() {
    let f=Fixture::new(2).await;let (corp,ceo)=f.corporation().await;let mut director=f.member(corp,None,1).await;director.roles_at_all=1;
    let remote=Call{service:crate::svc::bound::object_id(1,"corpStationMgr",&[f.station+1]),..f.call("RentOffice",vec![Value::Int(100)])};
    assert!(is_error(&crate::svc::corp::station_mgr(&remote,&f.db.ctx(&director)).await.unwrap(),"RentingOfficeRequestDenied"));
    for invalid in [f64::NAN,f64::INFINITY,-1.0] {assert!(is_error(&f.rent(&director,invalid).await,"RentingOfficeRequestDenied"));}
    let pilot=director.char_id.unwrap();let ceo_id=ceo.char_id.unwrap();
    f.set("chrCorporationRoles",pilot,&[("rolesAtAll",V::Int(0))]);
    assert!(is_error(&f.rent(&director,100.0).await,"RentingOfficeRequestDenied"));assert_eq!(f.balance(corp),1000.0);
    assert!(is_error(&f.invoke(&director,"GetQuoteForRentingAnOffice",vec![]).await,"RentingOfficeQuotesOnlyGivenToActiveCEOsOrEquivale"));
    f.set("character_",ceo_id,&[("stationID",V::Int(f.station+1))]);
    assert!(is_error(&f.rent(&ceo,100.0).await,"RentingOfficeRequestDenied"));
    f.set("character_",ceo_id,&[("stationID",V::Int(f.station))]);
    let (other_corp,_)=f.corporation().await;
    f.set("character_",ceo_id,&[("corporationID",V::Int(other_corp))]);
    assert!(is_error(&f.rent(&ceo,100.0).await,"RentingOfficeRequestDenied"),"stale CEO/corporation session cannot spend the former corporation's wallet");
    f.set("character_",ceo_id,&[("corporationID",V::Int(corp))]);
    f.set("staStations",f.station,&[("officeRentalCost",V::Int(125))]);
    assert!(is_error(&f.rent(&ceo,100.0).await,"RentingAnOfficeCostsMore"));assert_eq!(f.offices(corp),0);assert_eq!(f.journal(corp),0);
    f.set("staStations",f.station,&[("officeRentalCost",V::Int(75))]);
    assert!(office(&f.rent(&ceo,100.0).await).is_some());assert_eq!(f.balance(corp),925.0);
}

#[tokio::test]
async fn private_slots_and_legacy_rentals_keep_identity_and_historical_values() {
    let f=Fixture::new(3).await;let (legacy,legacy_ceo)=f.corporation().await;let (corp,ceo)=f.corporation().await;
    let item=f.id().await;let folder=f.id().await;
    f.insert("entity",&[("itemID",V::Int(item)),("typeID",V::Int(27)),("ownerID",V::Int(legacy)),("locationID",V::Int(f.station)),("flag",V::Int(0)),("quantity",V::Int(1)),("singleton",V::Int(1))]);
    f.insert("crpOffices",&[("corporationID",V::Int(legacy)),("stationID",V::Int(f.station)),("itemID",V::Int(item)),("typeID",V::Int(27)),("officeFolderID",V::Int(folder))]);
    f.insert("rentalInfo",&[("stationID",V::Int(f.station)),("slotNumber",V::Int(2)),("typeID",V::Int(26)),("renterID",V::Int(legacy)),("publiclyAvailable",V::Int(0)),
        ("startDate",V::Int(123)),("balanceDueDate",V::Int(456)),("periodCost",V::Float(17.5)),("rentPeriodInDays",V::Int(12))]);
    f.insert("rentalInfo",&[("stationID",V::Int(f.station)),("slotNumber",V::Int(1)),("typeID",V::Int(26)),("renterID",V::Int(0)),("publiclyAvailable",V::Int(0)),
        ("startDate",V::Int(0)),("balanceDueDate",V::Int(0)),("periodCost",V::Float(0.0)),("rentPeriodInDays",V::Int(0))]);
    let Value::Instance{state,..}=crate::svc::corp::items_rented(&f.db.ctx(&legacy_ceo)).await.unwrap() else {panic!("original rental rowset");};
    let rows=state.get_str("lines").and_then(Value::seq).unwrap();assert_eq!(rows.len(),1);let row=rows[0].seq().unwrap();
    assert_eq!(row[2].as_i64(),Some(123));assert_eq!(row[5].as_f64(),Some(17.5));assert_eq!(row[6].as_i64(),Some(456));
    let id=office(&f.rent(&ceo,100.0).await).expect("third slot remains public");
    f.db.store.read(|db|{
        let mut rows:Vec<store::Row>=db.table("rentalInfo").find(&["stationID"],&[V::Int(f.station)]).collect();rows.sort_by_key(|r|r.int("slotNumber"));
        // The fourth slot is the filler's, whose office claims it without a rental row of its own.
        assert_eq!(rows.len(),4);assert_eq!(rows[0].int("renterID"),Some(0));assert_eq!(rows[1].int("renterID"),Some(legacy));assert_eq!(rows[2].int("renterID"),Some(corp));assert_eq!(rows[3].int("renterID"),Some(0));
        assert_eq!(rows[1].int("startDate"),Some(123));assert_eq!(rows[1].int("balanceDueDate"),Some(456));assert_eq!(rows[1].float("periodCost"),Some(17.5));
        let mut linked:Vec<store::Row>=db.table("crpOffices").find(&["stationID"],&[V::Int(f.station)]).collect();linked.sort_by_key(|o|o.int("rentalSlot"));
        assert_eq!(linked[0].int("itemID"),Some(item));assert_eq!(linked[0].int("rentalSlot"),Some(2));assert_eq!(linked[1].int("itemID"),Some(id));assert_eq!(linked[1].int("rentalSlot"),Some(3));
    });
    let (other,caller)=f.corporation().await;
    assert!(is_error(&f.rent(&caller,100.0).await,"NoOfficesAreAvailableForRenting"));assert_eq!(f.balance(other),1000.0);assert_eq!(f.journal(other),0);
}
