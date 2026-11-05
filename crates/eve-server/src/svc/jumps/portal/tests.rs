use super::*;
use crate::ballpark::{Command, Park, Vec3};
use crate::session::Session;
use crate::store::{StoreError, Value as Cell};
use std::sync::Arc;

pub(super) const FROM:i64=30000142;
pub(super) const TO:i64=30000143;

pub(super) struct Fixture {
    pub(super) db:crate::test_support::Database,
    pub(super) session:Session,
    pub(super) ship:i64,
    pub(super) module:i64,
    pub(super) fuel:i64,
    pub(super) member:i64,
    pub(super) beacon:i64,
    pub(super) fixed:i64,
}

/// Test rows, inserted as they are given.
pub(super) fn insert(f:&Fixture,table:&str,sets:&[(&str,Cell)]) {
    f.db.store.write(|tx|tx.insert(table,sets)).unwrap();
}

/// Sets columns of the row whose single-column key is `id`.
pub(super) fn set(f:&Fixture,table:&str,id:i64,sets:&[(&str,Cell)]) {
    f.db.store.write(|tx|{let row=tx.table(table).get_id(id).unwrap().id;tx.update(table,row,sets)}).unwrap();
}

impl Fixture {
    pub(super) async fn new(covert:bool)->Self {
        let db=crate::test_support::Database::load().await;
        let id=|v:i32|i64::from(v);
        let pilot=id(db.id().await);let corp=id(db.id().await);let alliance=id(db.id().await);let ship=id(db.id().await);
        let module=id(db.id().await);let fuel=id(db.id().await);let member=id(db.id().await);let beacon=id(db.id().await);let fixed=id(db.id().await);
        let ship_type=if covert {22428}else{11567};let module_type=if covert {28652}else{23953};
        let fixed_type=i64::from(crate::sde::get().types_in_group(838)[0]);
        db.store.write(|tx|{
            tx.insert("corporation",&[("corporationID",Cell::Int(corp)),("corporationName",Cell::Text(format!("Portal corp {corp}").into())),("allianceID",Cell::Int(alliance))])?;
            tx.insert("character_",&[("characterID",Cell::Int(pilot)),("characterName",Cell::Text(format!("Portal pilot {pilot}").into())),
                ("corporationID",Cell::Int(corp)),("shipID",Cell::Int(ship)),("solarSystemID",Cell::Int(FROM)),("stationID",Cell::Int(0))])?;
            for (id,ty,owner,location,flag,quantity,singleton) in [
                (ship,ship_type,pilot,FROM,0,1,1),(module,module_type,pilot,ship,27,1,1),(fuel,16275,pilot,ship,5,1500,0),(fixed,fixed_type,corp,TO,0,1,1),
            ] {
                tx.insert("entity",&[("itemID",Cell::Int(id)),("typeID",Cell::Int(ty)),("ownerID",Cell::Int(owner)),("locationID",Cell::Int(location)),
                    ("flag",Cell::Int(flag)),("quantity",Cell::Int(quantity)),("singleton",Cell::Int(singleton))])?;
            }
            tx.insert("entity_attributes",&[("itemID",Cell::Int(module)),("attributeID",Cell::Int(2)),("valueInt",Cell::Int(1))])?;
            tx.insert("posStructures",&[("itemID",Cell::Int(fixed)),("state",Cell::Int(4))])?;
            Ok::<_,StoreError>(())
        }).unwrap();
        let session=Session {char_id:Some(pilot),corp_id:Some(corp),alliance_id:Some(alliance),
            ship_id:Some(ship),solar_system_id:Some(FROM),solar_system_id2:Some(FROM),..Default::default()};
        let mut hull=crate::svc::inflight::ship_for_space(&db.ctx(&session),pilot,ship).await.unwrap();
        hull.fight.as_mut().unwrap().vitals.capacitor=Charged::full(10000.0,0.0,combat::now_ms());
        let mut source=Park::new(FROM,vec![],0);source.ships.insert(ship,hull);
        let mut destination=Park::new(TO,vec![],0);
        destination.beacons.insert(beacon,Beacon::new(beacon,if covert {28650}else{21094},Some(member),"Portal destination",Vec3::ZERO,1000.0,
            Kind::Cyno {char_id:member,ship_id:member+1}));
        {
            let mut w=db.world.lock();w.parks.insert(FROM,source);w.parks.insert(TO,destination);
            w.gangs.test_members(vec![pilot,member]);
        }
        Self {db,session,ship,module,fuel,member,beacon,fixed}
    }
    pub(super) async fn open(&self)->Refused<()> {
        super::super::open_bridge(&self.db.ctx(&self.session),self.member,self.beacon,TO).await
    }
    pub(super) async fn fuel(&self)->i64 {
        self.db.store.read(|db|db.table("entity").get_id(self.fuel).unwrap().int("quantity").unwrap())
    }
    pub(super) fn cap(&self)->f64 {
        self.db.world.lock().parks[&FROM].ships[&self.ship].fight.as_ref().unwrap().vitals.capacitor.at(combat::now_ms())
    }
    pub(super) fn released(&self) {
        let w=self.db.world.lock();assert!(!w.pending_bridges.contains(&self.ship));assert!(!w.parks[&FROM].ships[&self.ship].identity_reserved);
    }
}

/// A portal request for the fixture's pilot, as connection 1 makes it.
pub(super) fn req(f:&Fixture)->Request {Request {world:f.db.world.clone(),pilot:f.session.char_id.unwrap(),client:1,ship:f.ship,from:FROM,
    member:Some(f.member),beacon:f.beacon,to:TO,alliance:f.session.alliance_id,bonuses:BTreeMap::new()}}

/// A receipt for a standard portal, written as an accepted start writes it.
pub(super) fn seed(f:&Fixture)->journal::Receipt {
    seed_for(f,60000)
}

pub(super) fn seed_for(f:&Fixture,duration:i64)->journal::Receipt {
    let quote=Quote {generator_id:f.module,generator_type:23953,fuel_type:16275,fuel:500,capacitor:1000.0,duration,mass_factor:1e-9,range:10.0,harmonics:0};
    f.db.store.write(|tx|journal::begin(tx,&req(f),&quote,11567).map_err(|why|StoreError {code:crate::store::DATA_EXCEPTION,message:why})).unwrap()
}

/// The world as an accepted start leaves it for `receipt`: the source's
/// identity reserved and its capacitor paid, the portal pending.
pub(super) fn reserve(f:&Fixture,receipt:&journal::Receipt) {
    let mut w=f.db.world.lock();
    let hull=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap();
    hull.identity_reserved=true;
    let capacitor=&mut hull.fight.as_mut().unwrap().vitals.capacitor;
    let before=*capacitor;assert!(capacitor.spend(combat::now_ms(),1000.0));let reserved=*capacitor;
    w.pending_bridges.insert(f.ship);
    w.pending_portals.insert(receipt.generation,Pending::owned(receipt.clone(),1,before,reserved));
}

/// The state of the ship's newest receipt.
pub(super) fn phase(f:&Fixture)->i64 {
    f.db.store.read(|db|db.table("portalReceipts").rows().filter(|r|r.int("shipID")==Some(f.ship))
        .max_by_key(|r|r.int("generation")).and_then(|r|r.int("state")).unwrap())
}

#[tokio::test]
async fn portal_invalidations_refuse_without_payment_or_reservation() {
    let empty=Fixture::new(false).await;
    empty.db.world.lock().parks.get_mut(&FROM).unwrap().ships.get_mut(&empty.ship).unwrap().fight.as_mut().unwrap().vitals.capacitor.set(combat::now_ms(),999.0);
    assert!(empty.open().await.is_err());assert_eq!(empty.fuel().await,1500);assert_eq!(empty.cap(),999.0);empty.released();
    for case in 0..3 {
        let f=Fixture::new(false).await;
        {
            let mut w=f.db.world.lock();
            match case {
                0=>{w.parks.get_mut(&TO).unwrap().beacons.remove(&f.beacon);},
                1=>{crate::svc::gang::leave(&mut w,f.member).ok().unwrap();},
                _=>{let fight=w.parks.get_mut(&FROM).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap();fight.vitals.hull_damage=fight.vitals.hull_max;},
            }
        }
        assert!(f.open().await.is_err(),"invalidation case {case}");assert_eq!(f.fuel().await,1500);assert_eq!(f.cap(),10000.0);
        assert!(!f.db.world.lock().bridges.contains_key(&f.ship));f.released();
    }
    let f=Fixture::new(false).await;
    {
        let mut w=f.db.world.lock();let p=w.parks.get_mut(&FROM).unwrap();
        p.command(f.ship,Command::Warp {target:Vec3::new(5e12,0.0,0.0),range:0.0});
        assert!(p.ships[&f.ship].warping());
    }
    assert!(f.open().await.is_err());assert_eq!(f.fuel().await,1500);assert_eq!(f.cap(),10000.0);f.released();
}

#[tokio::test]
async fn portal_duplicates_preserve_one_payment() {
    let f=Arc::new(Fixture::new(false).await);
    f.open().await.unwrap();
    assert!(f.db.world.lock().bridges.contains_key(&f.ship));
    assert_eq!(f.fuel().await,1000);assert_eq!(f.cap(),9000.0);f.released();
    assert!(f.open().await.is_err());assert_eq!(f.fuel().await,1000);
    f.db.world.lock().bridges.remove(&f.ship);
    f.db.world.lock().portal_cycles.remove(&f.ship);
    let (a,b)=tokio::join!(f.open(),f.open());
    assert_ne!(a.is_ok(),b.is_ok());assert_eq!(f.fuel().await,500);assert_eq!(f.cap(),8000.0);f.released();
}

#[tokio::test]
async fn portal_revocation_after_acceptance_closes_it_and_fixed_beacons_recheck_state() {
    let f=Fixture::new(false).await;
    // An accepted start whose destination goes before it is finalized is
    // closed: its payment stands, but no bridge is published.
    let receipt=seed(&f);reserve(&f,&receipt);
    f.db.world.lock().parks.get_mut(&TO).unwrap().beacons.remove(&f.beacon);
    journal::complete_owned(&req(&f),&f.db.store,&receipt,Vec::new()).await.unwrap();
    assert_eq!(f.cap(),9000.0);assert_eq!(phase(&f),2);
    assert!(!f.db.world.lock().bridges.contains_key(&f.ship));f.released();
    // An offline fixed beacon is read as it stands when the portal opens.
    set(&f,"posStructures",f.fixed,&[("state",Cell::Int(1))]);
    let ctx=f.db.ctx(&f.session);
    assert!(super::super::open_bridge_structure(&ctx,f.fixed,TO).await.is_err());
    assert_eq!(f.fuel().await,1500);assert_eq!(f.cap(),9000.0);f.released();
}

#[tokio::test]
async fn covert_portals_use_original_cost_duration_and_traveler_harmonics() {
    let normal=Fixture::new(false).await;
    normal.db.world.lock().parks.get_mut(&TO).unwrap().beacons.get_mut(&normal.beacon).unwrap().type_id=28650;
    assert!(normal.open().await.is_err());assert_eq!(normal.fuel().await,1500);assert_eq!(normal.cap(),10000.0);
    let f=Fixture::new(true).await;let before=combat::now_ms();
    f.open().await.unwrap();assert_eq!(f.cap(),9900.0);assert_eq!(f.fuel().await,1500,"original covert module has no activation-fuel attributes");
    {
        let w=f.db.world.lock();let bridge=&w.bridges[&f.ship];assert_eq!(bridge.harmonics,2);
        assert!(bridge.until_ms>=before+20000 && bridge.until_ms<=combat::now_ms()+20000);
    }
    let traveler=i64::from(f.db.id().await);let ship=i64::from(f.db.id().await);
    insert(&f,"character_",&[("characterID",Cell::Int(traveler)),("characterName",Cell::Text(format!("Traveler {traveler}").into())),
        ("corporationID",Cell::Int(f.session.corp_id.unwrap())),("shipID",Cell::Int(ship)),("solarSystemID",Cell::Int(FROM)),("stationID",Cell::Int(0))]);
    insert(&f,"entity",&[("itemID",Cell::Int(ship)),("typeID",Cell::Int(587)),("ownerID",Cell::Int(traveler)),("locationID",Cell::Int(FROM)),
        ("flag",Cell::Int(0)),("quantity",Cell::Int(1)),("singleton",Cell::Int(1))]);
    let session=Session {char_id:Some(traveler),ship_id:Some(ship),..f.session.clone()};
    let hull=crate::svc::inflight::ship_for_space(&f.db.ctx(&session),traveler,ship).await.unwrap();
    {
        let mut w=f.db.world.lock();w.parks.get_mut(&FROM).unwrap().ships.insert(ship,hull);
        let gang=w.gangs.of(f.session.char_id.unwrap()).unwrap();w.gangs.gangs.get_mut(&gang).unwrap().members.push(traveler);
    }
    assert!(super::super::prepare_jump_through(&f.db.ctx(&session),traveler,ship,FROM,f.session.char_id.unwrap(),f.ship,false).await.is_err());
    // Original covertops hull 11182 has jumpHarmonics2; the ordinary frigate
    // must not inherit the portal's own permission.
    set(&f,"entity",ship,&[("typeID",Cell::Int(11182))]);
    let hull=crate::svc::inflight::ship_for_space(&f.db.ctx(&session),traveler,ship).await.unwrap();
    f.db.world.lock().parks.get_mut(&FROM).unwrap().ships.insert(ship,hull);
    // Transit requires source isotope fuel, independently of activation.
    let drive=super::super::drive(&f.db.ctx(&f.session),f.session.char_id.unwrap(),f.ship).await.unwrap();
    let id=i64::from(f.db.id().await);
    insert(&f,"entity",&[("itemID",Cell::Int(id)),("typeID",Cell::Int(drive.fuel)),("ownerID",Cell::Int(f.session.char_id.unwrap())),("locationID",Cell::Int(f.ship)),
        ("flag",Cell::Int(5)),("quantity",Cell::Int(100000)),("singleton",Cell::Int(0))]);
    super::super::prepare_jump_through(&f.db.ctx(&session),traveler,ship,FROM,f.session.char_id.unwrap(),f.ship,false).await.unwrap();
}
