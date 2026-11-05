use super::*;
use std::collections::HashSet;
use crate::{test_support::Database,session::Session,store::Sets};

struct Fixture { db:Database, session:Session, pilot:i64, ship:i64 }

fn entity(id:i64,kind:i64,owner:i64,location:i64,flag:i64,quantity:i64,singleton:i64)->Vec<(&'static str,store::Value)> {
    vec![("itemID",store::Value::Int(id)),("typeID",store::Value::Int(kind)),("ownerID",store::Value::Int(owner)),("locationID",store::Value::Int(location)),
        ("flag",store::Value::Int(flag)),("quantity",store::Value::Int(quantity)),("singleton",store::Value::Int(singleton))]
}

impl Fixture {
    async fn new()->Self {
        let db=Database::load().await;
        let pilot=i64::from(db.id().await);let ship=i64::from(db.id().await);
        let (station,system)=db.store.read(|d|d.table("staStations").rows().filter_map(|r|Some((r.int("stationID")?,r.int("solarSystemID")?))).min()).unwrap();
        db.store.write(|tx|->Result<(),StoreError> {
            tx.insert("character_",&[("characterID",store::Value::Int(pilot)),("characterName",store::Value::Text(format!("Booster fixture {pilot}").into())),
                ("shipID",store::Value::Int(ship)),("stationID",store::Value::Int(station)),("solarSystemID",store::Value::Int(system))])?;
            tx.insert("entity",&entity(pilot,1373,pilot,station,57,1,1))?;
            tx.insert("entity",&entity(ship,587,pilot,station,4,1,1))?;
            Ok(())
        }).unwrap();
        Self{db,session:Session{char_id:Some(pilot),ship_id:Some(ship),station_id:Some(station),solar_system_id2:Some(system),..Default::default()},pilot,ship}
    }
    async fn item(&self,kind:i64,location:i64,flag:i64,quantity:i64,singleton:i64)->i64 {
        let id=i64::from(self.db.id().await);
        self.db.store.write(|tx|tx.insert("entity",&entity(id,kind,self.pilot,location,flag,quantity,singleton))).unwrap();id
    }
    async fn train(&self,level:i64) {
        let skill=self.item(i64::from(BIOLOGY),self.pilot,7,1,1).await;
        self.db.store.write(|tx|tx.insert("entity_attributes",&[("itemID",store::Value::Int(skill)),("attributeID",store::Value::Int(280)),("valueInt",store::Value::Int(level))])).unwrap();
    }
    async fn consume(&self,id:i64)->Outcome {consume(&self.db.ctx(&self.session),&[Value::Int(id)]).await}
    async fn count(&self,id:i64)->i64 {self.db.store.read(|d|d.table("entity").get_id(id).and_then(|r|r.int("quantity"))).unwrap()}
    async fn expire_now(&self) {
        let now=crate::bulkdata::filetime_now();
        self.db.store.write(|tx|{
            let rows:Vec<_>=tx.table("character_boosters").find(&["character"],&[store::Value::Int(self.pilot)]).map(|r|r.id).collect();
            for row in rows {tx.update("character_boosters",row,&[("started",store::Value::Int(now-2)),("expires",store::Value::Int(now-1))])?;}
            Ok::<_,StoreError>(())
        }).unwrap();
    }
    /// Set columns of the row whose single-column key is `id`.
    fn set(&self,table:&str,id:i64,sets:Sets) {
        self.db.store.write(|tx|{let row=tx.table(table).get_id(id).unwrap().id;tx.update(table,row,sets)}).unwrap();
    }
    fn delete(&self,table:&str,id:i64) {
        self.db.store.write(|tx|{let row=tx.table(table).get_id(id).unwrap().id;tx.delete(table,row)}).unwrap();
    }
    fn snapshot(&self)->std::collections::HashMap<i64,std::collections::HashMap<i32,f64>> {
        self.db.store.read(|d|crate::dogma::Loadout::load(d,&self.db.dogma,self.pilot,Some(self.ship),&HashSet::new())).unwrap().snapshot()
    }
    fn refresh_pending(&self)->bool {self.db.store.read(|d|d.table("character_booster_refresh").get_id(self.pilot).is_some())}
}

fn success(out:&Outcome)->bool {matches!(out,Outcome::Value(Value::None))}
fn refusal(out:Outcome,needle:&str)->bool {matches!(out,Outcome::UserError{args,..} if args.iter().any(|(_,v)|v.as_string().is_some_and(|s|s.contains(needle))))}

#[test]
fn original_biology_duration_and_supported_effects_are_bounded() {
    assert_eq!(expiry(100,1_800_000.0,20.0,0),Some(18_000_000_100));
    assert_eq!(expiry(100,1_800_000.0,20.0,1),Some(21_600_000_100));
    assert_eq!(expiry(100,1_800_000.0,20.0,5),Some(36_000_000_100));
    assert_eq!(expiry(i64::MAX-1,1_800_000.0,20.0,5),None);
    for invalid in [0.0,-1.0,f64::NAN,f64::INFINITY] {assert_eq!(expiry(100,invalid,20.0,1),None);}
    for unsupported in [9947,9950,28670,28674,28676] {assert_eq!(modifier(unsupported),None);}
}

#[tokio::test]
async fn original_synth_effects_consume_expire_and_reload_without_later_modifier_contamination() {
    let f=Fixture::new().await;f.train(5).await;
    let gun=f.item(484,f.ship,27,1,1).await;
    let launcher=f.item(499,f.ship,28,1,1).await;
    let missile=f.item(211,f.ship,28,10,0).await;
    for module in [gun,launcher] {f.db.store.write(|tx|tx.insert("entity_attributes",&[("itemID",store::Value::Int(module)),("attributeID",store::Value::Int(2)),("valueInt",store::Value::Int(1))])).unwrap();}
    for (kind,target,attribute,factor) in [(28672,missile,654,0.97),(28678,gun,54,1.03),(28680,f.ship,482,1.03),(28682,f.ship,37,1.03),(28684,gun,158,1.03)] {
        let before=f.snapshot();
        let stack=f.item(kind,f.session.station_id.unwrap(),4,2,0).await;
        let result=f.consume(stack).await;assert!(success(&result),"type {kind} consumed");assert_eq!(f.count(stack).await,1);
        let (active,duration,expires)=f.db.store.read(|d|{
            let b=d.table("character_boosters").find(&["character"],&[store::Value::Int(f.pilot)]).next().unwrap();
            (b.int("item").unwrap(),b.int("expires").unwrap()-b.int("started").unwrap(),b.int("expires").unwrap())
        });
        assert_ne!(active,stack);assert_eq!(duration,36_000_000_000);
        let stamp=f.db.store.read(|d|d.table("entity_attributes").get(&[store::Value::Int(active),store::Value::Int(1088)]).and_then(|r|r.int("valueInt"))).unwrap();
        assert_eq!(stamp,expires);
        let after=f.snapshot();
        let baseline=before[&target][&attribute];assert!(baseline>0.0);
        assert!((after[&target][&attribute]/baseline-factor).abs()<1e-9,"original effect {kind} applies exactly once");
        // Other period-specific roles must not leak from the later catalogue.
        for other in [51,64,164,165,166,167,168] {
            for id in [f.pilot,gun] {assert_eq!(before[&id].get(&other),after[&id].get(&other));}
        }
        let call=crate::services::Call{service:"dogmaIM".into(),method:"CharGetInfo".into(),args:vec![],kwargs:vec![]};
        let info=super::super::dogma::dogma_im(&call,&f.db.ctx(&f.session)).await.unwrap();
        assert!(matches!(info,Outcome::Value(Value::Dict(ref rows)) if rows.iter().any(|(id,_)|*id==Value::Int(active))));
        fn contains_exact_expiry(value:&Value,stamp:i64)->bool {
            match value {
                Value::Dict(pairs)=>pairs.iter().any(|(key,value)|(*key==Value::Int(1088) && *value==Value::Long(stamp)) || contains_exact_expiry(value,stamp)),
                Value::List(values)|Value::Tuple(values)=>values.iter().any(|v|contains_exact_expiry(v,stamp)),
                Value::Instance{state,..}=>contains_exact_expiry(state,stamp),_=>false,
            }
        }
        assert!(matches!(info,Outcome::Value(ref value) if contains_exact_expiry(value,stamp)),"original expiryTime remains exact FILETIME beyond f64 integer precision");
        f.expire_now().await;
        let expired=f.snapshot();
        assert_eq!(expired[&target][&attribute],baseline,"expired effects are absent before cleanup/reconnect");
        let info=super::super::dogma::dogma_im(&call,&f.db.ctx(&f.session)).await.unwrap();
        assert!(matches!(info,Outcome::Value(Value::Dict(ref rows)) if rows.iter().all(|(id,_)|*id!=Value::Int(active))));
        expire(&f.db.store,&f.db.world).await.unwrap();
        assert!(f.db.store.read(|d|d.table("entity").get_id(active).is_none()));
    }
    // A legacy/injected unsupported booster must not inherit the later
    // catalogue's duplicate Crash or learning/ROF mappings.
    for (kind,slot) in [(9947,3),(15457,1)] {
        let before=f.snapshot();
        let active=f.item(kind,f.pilot,88,1,1).await;let now=crate::bulkdata::filetime_now();
        f.db.store.write(|tx|tx.insert("character_boosters",&[("item",store::Value::Int(active)),("character",store::Value::Int(f.pilot)),("type_id",store::Value::Int(kind)),
            ("slot",store::Value::Int(slot)),("started",store::Value::Int(now)),("expires",store::Value::Int(now+18_000_000_000))])).unwrap();
        let after=f.snapshot();
        for id in [f.pilot,f.ship,gun,missile] {assert_eq!(before[&id],after[&id],"unsupported original type {kind} receives no fabricated later modifiers");}
        f.expire_now().await;expire(&f.db.store,&f.db.world).await.unwrap();
    }
}

#[tokio::test]
async fn consumption_checks_skills_slots_custody_and_rolls_back_failed_split() {
    let f=Fixture::new().await;let station=f.session.station_id.unwrap();
    let stack=f.item(28682,station,4,2,0).await;
    assert!(refusal(f.consume(stack).await,"You need skill"));assert_eq!(f.count(stack).await,2);
    f.train(1).await;
    f.db.store.write(|tx|tx.insert("pendingShipLosses",&[("shipID",store::Value::Int(f.ship)),("characterID",store::Value::Int(f.pilot)),
        ("solarSystemID",store::Value::Int(f.session.solar_system_id2.unwrap())),("shipTypeID",store::Value::Int(587)),("occupied",store::Value::Bool(true)),
        ("createdAt",store::Value::Int(0)),("loss",store::Value::Text("{}".into()))])).unwrap();
    assert!(refusal(f.consume(stack).await,"ship loss"),"direct service call observes the durable ordinary-loss quarantine");
    assert_eq!(f.count(stack).await,2);
    f.delete("pendingShipLosses",f.ship);
    for kind in [9947,9950,28670,28674,28676] {let item=f.item(kind,station,4,1,0).await;assert!(refusal(f.consume(item).await,"not implemented"));assert_eq!(f.count(item).await,1);}
    let remote=f.item(28682,station+1,4,1,0).await;assert!(refusal(f.consume(remote).await,"accessible"));
    let foreign=f.item(28682,station,4,1,0).await;
    f.set("entity",foreign,&[("ownerID",store::Value::Int(1))]);
    assert!(refusal(f.consume(foreign).await,"not your"));
    f.delete("entity",foreign);
    // The item the split would make is refused (its ID is already taken), so
    // the whole consumption fails after the source stack was reduced.
    let next=f.db.store.read(|d|d.sequence("dyn_id_seq")).unwrap()+1;
    f.db.store.write(|tx|tx.insert("entity",&entity(next,34,f.pilot,station,4,1,0))).unwrap();
    assert!(!success(&f.consume(stack).await));
    f.delete("entity",next);
    assert_eq!(f.count(stack).await,2,"failed active-item insert restores split source");
    let ctx=f.db.ctx(&f.session);let args=[Value::Int(stack)];
    let (first,second)=tokio::join!(consume(&ctx,&args),consume(&ctx,&args));
    assert_ne!(success(&first),success(&second));
    let rejected=if success(&first){second}else{first};
    assert!(matches!(rejected,Outcome::UserError{msg,..} if msg=="OnlyOneBoosterActive"));
    assert_eq!(f.count(stack).await,1);
    assert_eq!(f.db.store.read(|d|d.table("character_boosters").find(&["character"],&[store::Value::Int(f.pilot)]).count()),1);
}

/// Items moved before consumption or expiry (once while those waited for
/// the item's row lock) are judged where they now are.
#[tokio::test]
async fn consumption_and_expiry_revalidate_items_that_moved() {
    let f=Fixture::new().await;f.train(1).await;
    let station=f.session.station_id.unwrap();
    let item=f.item(28682,station,4,1,0).await;
    f.set("entity",item,&[("locationID",store::Value::Int(station+1))]);
    assert!(refusal(f.consume(item).await,"accessible"));
    f.set("entity",item,&[("locationID",store::Value::Int(station))]);
    assert!(success(&f.consume(item).await));f.expire_now().await;
    f.set("entity",item,&[("locationID",store::Value::Int(station)),("flag",store::Value::Int(4))]);
    expire(&f.db.store,&f.db.world).await.unwrap();
    assert_eq!(f.count(item).await,1,"stale expiry cannot destroy moved inventory");
    assert!(f.db.store.read(|d|d.table("character_boosters").get_id(item).is_none()));
}

#[tokio::test]
async fn live_refresh_waits_for_identity_reservation_preserves_damage_and_recovers_expiry() {
    let mut f=Fixture::new().await;f.train(1).await;
    let system=f.session.solar_system_id2.unwrap();
    f.set("character_",f.pilot,&[("stationID",store::Value::Int(0))]);
    f.set("entity",f.ship,&[("locationID",store::Value::Int(system)),("flag",store::Value::Int(0))]);
    f.session.station_id=None;f.session.solar_system_id=Some(system);
    let mut ship=super::super::inflight::ship_for_space(&f.db.ctx(&f.session),f.pilot,f.ship).await.unwrap();
    let speed=ship.stats.max_velocity;ship.identity_reserved=true;ship.fight.as_mut().unwrap().vitals.hull_damage=7.0;
    let mut park=crate::ballpark::Park::new(system,vec![],crate::destiny::tick_now());park.ships.insert(f.ship,ship);f.db.world.lock().parks.insert(system,park);
    let booster=f.item(28682,f.ship,5,1,0).await;assert!(success(&f.consume(booster).await));
    assert!(f.refresh_pending());
    {let mut w=f.db.world.lock();let ship=w.parks.get_mut(&system).unwrap().ships.get_mut(&f.ship).unwrap();assert_eq!(ship.stats.max_velocity,speed);ship.identity_reserved=false;}
    refresh::character(&f.db.store,&f.db.dogma,&f.db.world,f.pilot).await.unwrap();
    {let w=f.db.world.lock();let ship=&w.parks[&system].ships[&f.ship];assert!((ship.stats.max_velocity-speed*1.03).abs()<1e-9);assert_eq!(ship.fight.as_ref().unwrap().vitals.hull_damage,7.0);}
    let restored=super::super::inflight::ship_for_space(&f.db.ctx(&f.session),f.pilot,f.ship).await.unwrap();assert!((restored.stats.max_velocity-speed*1.03).abs()<1e-9);
    f.expire_now().await;expire(&f.db.store,&f.db.world).await.unwrap();
    refresh::character(&f.db.store,&f.db.dogma,&f.db.world,f.pilot).await.unwrap();
    {let w=f.db.world.lock();let ship=&w.parks[&system].ships[&f.ship];assert_eq!(ship.stats.max_velocity,speed);assert_eq!(ship.fight.as_ref().unwrap().vitals.hull_damage,7.0);}
    assert!(!f.refresh_pending());
}

#[tokio::test]
async fn expiry_refresh_survives_committed_location_reserved_source_and_detached_travel() {
    let mut f=Fixture::new().await;f.train(1).await;
    let from=f.session.solar_system_id2.unwrap();
    f.set("character_",f.pilot,&[("stationID",store::Value::Int(0))]);
    f.set("entity",f.ship,&[("locationID",store::Value::Int(from)),("flag",store::Value::Int(0))]);
    f.session.station_id=None;f.session.solar_system_id=Some(from);
    let mut ship=super::super::inflight::ship_for_space(&f.db.ctx(&f.session),f.pilot,f.ship).await.unwrap();
    let speed=ship.stats.max_velocity;ship.fight.as_mut().unwrap().vitals.hull_damage=9.0;
    let mut park=crate::ballpark::Park::new(from,vec![],crate::destiny::tick_now());park.ships.insert(f.ship,ship);f.db.world.lock().parks.insert(from,park);
    let booster=f.item(28682,f.ship,5,1,0).await;assert!(success(&f.consume(booster).await));
    {let mut w=f.db.world.lock();let ship=w.parks.get_mut(&from).unwrap().ships.get_mut(&f.ship).unwrap();assert!((ship.stats.max_velocity-speed*1.03).abs()<1e-9);ship.identity_reserved=true;}
    let (system,constellation,region)=f.db.store.read(|d|d.table("mapSolarSystems").rows()
        .filter_map(|r|Some((r.int("solarSystemID")?,r.int("constellationID")?,r.int("regionID")?))).filter(|r|r.0!=from).min()).unwrap();
    let destination=crate::travel::Place{system,station:None,constellation,region,at:crate::ballpark::Vec3::ZERO};
    let source=crate::travel::Place{system:from,..destination};
    // The first commit moves stored custody, while the old source hull
    // remains reserved and damageable. Its destination park does not exist yet.
    f.set("character_",f.pilot,&[("solarSystemID",store::Value::Int(destination.system))]);
    f.set("entity",f.ship,&[("locationID",store::Value::Int(destination.system))]);
    f.expire_now().await;expire(&f.db.store,&f.db.world).await.unwrap();
    refresh::character(&f.db.store,&f.db.dogma,&f.db.world,f.pilot).await.unwrap();
    assert!(f.refresh_pending(),"reserved source identity retains expiry work despite committed destination custody");
    let mut captured=f.db.world.lock().parks.get_mut(&from).unwrap().ships.remove(&f.ship).unwrap();
    let marker=f.db.store.write(|tx|crate::travel::begin(tx,f.pilot,f.ship,source,destination)).unwrap();
    crate::travel::track_uncertain(&f.db.world,&marker).unwrap();
    f.set("travelReceipts",marker.generation,&[("state",store::Value::Int(1))]);
    refresh::character(&f.db.store,&f.db.dogma,&f.db.world,f.pilot).await.unwrap();
    assert!(f.refresh_pending(),"in-memory unresolved completion retains expiry work with no published hull");
    f.set("travelReceipts",marker.generation,&[("state",store::Value::Int(0))]);
    crate::travel::clear_uncertain(&f.db.world,&marker);
    refresh::character(&f.db.store,&f.db.dogma,&f.db.world,f.pilot).await.unwrap();
    assert!(f.refresh_pending(),"durable pending travel retains expiry work even without its in-memory reservation");
    f.set("travelReceipts",marker.generation,&[("state",store::Value::Int(1))]);
    f.db.store.write(|tx|tx.insert("pendingShipLogouts",&[("shipID",store::Value::Int(f.ship)),("characterID",store::Value::Int(f.pilot)),
        ("solarSystemID",store::Value::Int(destination.system)),("createdAt",store::Value::Int(crate::combat::now_ms()))])).unwrap();
    refresh::character(&f.db.store,&f.db.dogma,&f.db.world,f.pilot).await.unwrap();
    assert!(f.refresh_pending(),"a detached final logout snapshot must not discard deferred expiry work");
    f.delete("pendingShipLogouts",f.ship);
    captured.identity_reserved=false;captured.arrive(destination.at);
    let mut park=crate::ballpark::Park::new(destination.system,vec![],crate::destiny::tick_now());park.ships.insert(f.ship,captured);f.db.world.lock().parks.insert(destination.system,park);
    refresh::character(&f.db.store,&f.db.dogma,&f.db.world,f.pilot).await.unwrap();
    assert!(!f.refresh_pending(),"published destination consumes the repair only after applying the current profile");
    {let w=f.db.world.lock();let ship=&w.parks[&destination.system].ships[&f.ship];assert_eq!(ship.stats.max_velocity,speed);assert_eq!(ship.fight.as_ref().unwrap().vitals.hull_damage,9.0);}
}
