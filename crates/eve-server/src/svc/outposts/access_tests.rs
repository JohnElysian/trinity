use super::*;

struct Fixture {db:crate::test_support::Database,station:i64,manager:i64,visitor:i64}
impl Fixture {
    async fn new()->Self {
        let db=crate::test_support::Database::load().await;
        let (station,manager,visitor)=(db.id().await as i64,db.id().await as i64,db.id().await as i64);
        db.store.write(|tx|->Result<(),StoreError> {
            for (pilot,corp) in [(manager,1000167),(visitor,1000002)] {
                tx.insert("character_",&[("characterID",int(pilot)),("characterName",text(&format!("Access fixture {pilot}"))),("corporationID",int(corp)),("securityRating",float(1.0))])?;
            }
            tx.insert("chrCorporationRoles",&[("characterID",int(manager)),("rolesAtAll",int(ROLE_DIRECTOR))])?;
            tx.insert("staStations",&[("stationID",int(station)),("stationName",text("Access fixture station")),("corporationID",int(1000167)),
                ("solarSystemID",int(30000141)),("constellationID",int(20000020)),("regionID",int(10000002))])?;
            let row=tx.table("character_").get_id(manager).unwrap().id;
            tx.update("character_",row,&[("stationID",int(station))])
        }).unwrap();
        Self {db,station,manager,visitor}
    }
    fn session(&self)->crate::session::Session {
        crate::session::Session{char_id:Some(self.manager),corp_id:Some(1000167),station_id:Some(self.station),roles_at_all:ROLE_DIRECTOR,..Default::default()}
    }
    fn settings(&self)->Vec<Value> {
        vec![Value::Dict(vec![(Value::Int(2097152),Value::Dict(vec![(Value::bytes("minimumStanding"),Value::Double(5.0)),
            (Value::bytes("minimumCharSecurity"),Value::Double(-10.0)),(Value::bytes("maximumCharSecurity"),Value::Double(10.0)),
            (Value::bytes("minimumCorpSecurity"),Value::Double(-10.0)),(Value::bytes("maximumCorpSecurity"),Value::Double(10.0))]))]),
            Value::List(vec![]),Value::List(vec![]),Value::text("Access fixture renamed"),Value::text("New access policy"),
            Value::Double(0.0),Value::Int(100),Value::Double(0.05),Value::None,Value::None]
    }
    fn may_use(&self,station:i64,char_id:i64)->bool {
        self.db.store.read(|db|may_use(db,station,2097152,char_id))
    }
    fn may_use_locked(&self,char_id:i64)->bool {
        self.db.store.read(|db|may_use_locked(db,self.station,2097152,char_id)).unwrap()
    }
    fn set(&self,table:&str,key:&[store::Value],sets:store::Sets) {
        self.db.store.write(|tx|{let row=tx.table(table).get(key).unwrap().id;tx.update(table,row,sets)}).unwrap();
    }
}

#[tokio::test]
async fn station_access_fails_closed_and_follows_the_rules() {
    let f=Fixture::new().await;
    assert!(f.may_use(f.station,f.visitor));
    assert!(!f.may_use(-1,f.visitor));
    assert!(!f.may_use(f.station,-1));
    f.db.store.write(|tx|tx.insert("stationServiceRules",&[("stationID",int(f.station)),("serviceID",int(2097152)),("minimumStanding",float(5.0))])).unwrap();
    assert!(!f.may_use_locked(f.visitor));
    assert!(f.may_use_locked(f.manager));
}

#[tokio::test]
async fn station_management_saves_the_rules_and_rejects_revoked_directors() {
    let f=Fixture::new().await;let session=f.session();let ctx=f.db.ctx(&session);let settings=f.settings();
    assert!(f.may_use(f.station,f.visitor));
    assert!(f.may_use_locked(f.visitor));
    update(&ctx,f.station,&settings).await.unwrap();
    assert!(!f.may_use(f.station,f.visitor));
    f.set("chrCorporationRoles",&[int(f.manager)],&[("rolesAtAll",int(0))]);
    let mut renamed=f.settings();renamed[3]=Value::text("Revoked rename");
    assert!(update(&ctx,f.station,&renamed).await.unwrap_err().contains("current station managers"));
    let name=f.db.store.read(|db|db.table("staStations").get_id(f.station).unwrap().text("stationName").unwrap().to_string());
    assert_eq!(name,"Access fixture renamed");
}

#[tokio::test]
async fn station_access_follows_the_owners_standings_towards_the_pilot(){
    let f=Fixture::new().await;
    let owner=f.db.id().await as i64;
    f.db.store.write(|tx|->Result<(),StoreError> {
        tx.insert("corporation",&[("corporationID",int(owner)),("corporationName",text("Standing access fixture"))])?;
        let row=tx.table("staStations").get_id(f.station).unwrap().id;
        tx.update("staStations",row,&[("corporationID",int(owner))])?;
        tx.insert("stationServiceRules",&[("stationID",int(f.station)),("serviceID",int(2097152)),("minimumStanding",float(0.0))])?;
        Ok(())
    }).unwrap();
    assert!(f.may_use_locked(f.visitor),"no standing is a standing of 0");
    f.db.store.write(|tx|tx.insert("ownerStandings",&[("fromID",int(owner)),("toID",int(f.visitor)),("standing",float(-5.0)),("reason",text("access race")),("setAt",int(0))])).unwrap();
    assert!(!f.may_use_locked(f.visitor));
    f.set("ownerStandings",&[int(owner),int(f.visitor)],&[("standing",float(5.0))]);
    assert!(f.may_use_locked(f.visitor));
    f.set("ownerStandings",&[int(owner),int(f.visitor)],&[("standing",float(-8.0))]);
    assert!(!f.may_use_locked(f.visitor));
}
