use super::*;
use crate::ballpark::{Command,Park,Vec3};
use crate::combat::{self,Charged};
use crate::services::Call;
use crate::session::Session;

struct Fixture {
    db: crate::test_support::Database,
    session: Session,
    system: i64,
    ship: i64,
    shield: i64,
    ab: i64,
    cyno: i64,
    doom: i64,
    fuel: i64,
}

/// An item row for a test.
fn put_item(tx: &mut crate::store::Tx<'_>, id: i64, type_id: i64, owner: i64, location: i64, flag: i64, quantity: i64, singleton: i64) -> Result<(), StoreError> {
    tx.insert("entity", &[("itemID",Value::Int(id)),("typeID",Value::Int(type_id)),("ownerID",Value::Int(owner)),("locationID",Value::Int(location)),
        ("flag",Value::Int(flag)),("quantity",Value::Int(quantity)),("singleton",Value::Int(singleton))])?;
    Ok(())
}

/// An item's quantity, as stored.
fn quantity(db: &crate::test_support::Database, id: i64) -> i64 {
    db.store.read(|db| db.table("entity").get_id(id).unwrap().int("quantity").unwrap())
}

impl Fixture {
    async fn new() -> Self {
        let db=crate::test_support::Database::load().await;
        let pilot=i64::from(db.id().await); let ship=i64::from(db.id().await);
        let shield=i64::from(db.id().await);let ab=i64::from(db.id().await);let cyno=i64::from(db.id().await);let doom=i64::from(db.id().await);let fuel=i64::from(db.id().await);
        let system=db.store.read(|db| db.table("mapSolarSystems").rows().filter(|s| s.float("security").is_some_and(|s| s<0.45)).filter_map(|s| s.int("solarSystemID")).min()).unwrap();
        db.store.write(|tx| -> Result<(), StoreError> {
            tx.insert("character_",&[("characterID",Value::Int(pilot)),("characterName",Value::Text(format!("Warp effects {pilot}").into())),
                ("shipID",Value::Int(ship)),("solarSystemID",Value::Int(system)),("stationID",Value::Int(0))])?;
            put_item(tx,ship,587,pilot,system,0,1,1)?;
            put_item(tx,shield,399,pilot,ship,19,1,1)?;
            put_item(tx,ab,439,pilot,ship,20,1,1)?;
            put_item(tx,cyno,21096,pilot,ship,11,1,1)?;
            put_item(tx,doom,24550,pilot,ship,12,1,1)?;
            put_item(tx,fuel,16274,pilot,ship,5,150000,0)?;
            for module in [shield,ab,cyno,doom] {
                tx.insert("entity_attributes",&[("itemID",Value::Int(module)),("attributeID",Value::Int(2)),("valueInt",Value::Int(1))])?;
            }
            Ok(())
        }).unwrap();
        let session=Session {char_id:Some(pilot),ship_id:Some(ship),solar_system_id:Some(system),solar_system_id2:Some(system),..Default::default()};
        let mut hull=super::super::ship_for_space(&db.ctx(&session),pilot,ship).await.unwrap();
        hull.stats.warp_capacitor_need=0.0;
        hull.fight.as_mut().unwrap().vitals.capacitor=Charged::full(100000.0,0.0,combat::now_ms());
        let mut park=Park::new(system,vec![],0);park.ships.insert(ship,hull);
        db.world.lock().parks.insert(system,park);
        Self {db,session,system,ship,shield,ab,cyno,doom,fuel}
    }

    fn align(&self) {
        let mut world=self.db.world.lock();let park=world.parks.get_mut(&self.system).unwrap();let hull=park.ships.get_mut(&self.ship).unwrap();
        hull.arrive(Vec3::ZERO);hull.vel=Vec3::new(hull.stats.max_velocity,0.0,0.0);
        park.command(self.ship,Command::Warp {target:Vec3::new(5e12,0.0,0.0),range:0.0});
    }

    fn advance(&self) {
        let mut world=self.db.world.lock();let park=world.parks.get_mut(&self.system).unwrap();park.advance(park.tick+1);
    }

    fn call(&self,item:i64,name:&str)->Call {
        Call {service:"dogmaIM".into(),method:"Activate".into(),args:vec![eve_marshal::Value::Int(item),eve_marshal::Value::bytes(name),eve_marshal::Value::None,eve_marshal::Value::Int(0)],kwargs:vec![]}
    }

    fn fuel(&self)->i64 {
        quantity(&self.db,self.fuel)
    }

    fn set_fuel(&self,quantity:i64) {
        self.db.store.write(|tx| {
            let row=tx.table("entity").get_id(self.fuel).unwrap().id;
            tx.update("entity",row,&[("quantity",Value::Int(quantity))])
        }).unwrap();
    }

    fn capacitor(&self)->f64 {
        self.db.world.lock().parks[&self.system].ships[&self.ship].fight.as_ref().unwrap().vitals.capacitor.at(combat::now_ms())
    }
}

#[tokio::test]
async fn original_warp_safe_flags_allow_repairs_and_refuse_the_rest_in_warp() {
    let f=Fixture::new().await;
    let ctx=f.db.ctx(&f.session);
    for (name,safe) in [("shieldBoosting",true),("armorRepair",true),("powerBooster",true),("cloakingWarpSafe",true),("cloaking",false),("speedBoostMassAddition",false)] {
        assert_eq!(super::super::effect_by_name(&ctx,name).await.unwrap().warp_safe,safe);
    }
    f.align();f.advance();assert!(f.db.world.lock().parks[&f.system].ships[&f.ship].in_warp());
    assert!(matches!(super::super::activate(&ctx,&f.call(f.shield,"shieldBoosting")).await,Outcome::Value(_)));
    assert!(f.db.world.lock().parks[&f.system].ships[&f.ship].fight.as_ref().unwrap().module(f.shield).unwrap().warp_safe);
    assert!(matches!(super::super::activate(&ctx,&f.call(f.ab,"speedBoostMassAddition")).await,Outcome::UserError{..}));
    assert!(matches!(super::super::activate(&ctx,&f.call(f.cyno,"cynosuralGeneration")).await,Outcome::UserError{..}));
    assert_eq!(f.fuel(),150000);
    assert!(f.db.world.lock().parks[&f.system].ships[&f.ship].fight.as_ref().unwrap().module(f.ab).is_none());
}

#[tokio::test]
async fn original_cyno_effect_burns_fuel_once_creates_field_and_refuses_alignment() {
    let f=Fixture::new().await;let ctx=f.db.ctx(&f.session);
    let fuel=i64::from(f.db.id().await);
    f.db.store.write(|tx| put_item(tx,fuel,16273,f.session.char_id.unwrap(),f.ship,5,1000,0)).unwrap();
    let call=f.call(f.cyno,"cynosuralGeneration");
    assert_eq!(super::super::effect_by_name(&ctx,"cynosuralGeneration").await.unwrap().id,2857);
    f.align();
    assert!(matches!(super::super::activate(&ctx,&call).await,Outcome::UserError{..}));
    assert_eq!(quantity(&f.db,fuel),1000);
    f.db.world.lock().parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap().arrive(Vec3::ZERO);
    assert!(matches!(super::super::activate(&ctx,&call).await,Outcome::Value(_)));
    assert_eq!(quantity(&f.db,fuel),500);
    {
        let world=f.db.world.lock();let park=&world.parks[&f.system];
        let active=park.ships[&f.ship].fight.as_ref().unwrap().module(f.cyno).unwrap();
        assert_eq!(active.action,crate::combat::Action::Cyno);assert!(active.disallow_warp_alignment);assert_eq!(active.cycles,1);
        assert_eq!(park.beacons.values().filter(|b|matches!(b.kind,crate::beacons::Kind::Cyno {ship_id,..} if ship_id==f.ship)).count(),1);
    }
    assert!(matches!(super::super::activate(&ctx,&call).await,Outcome::Value(_)));
    assert_eq!(quantity(&f.db,fuel),500);
    f.align();f.advance();
    assert!(!f.db.world.lock().parks[&f.system].ships[&f.ship].warping());
}

#[tokio::test]
async fn funded_activation_reserves_capacitor_rolls_back_failures_and_fires_once() {
    let f=Fixture::new().await;let ctx=f.db.ctx(&f.session);let call=f.call(f.doom,"empWaveGrid");
    {
        let mut world=f.db.world.lock();let ship=world.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap();
        ship.fight.as_mut().unwrap().vitals.capacitor.set(combat::now_ms(),1000.0);
    }
    assert!(matches!(super::super::activate(&ctx,&call).await,Outcome::UserError{..}));
    assert_eq!(f.capacitor(),1000.0);assert_eq!(f.fuel(),150000);
    f.align();
    // A refusal after the capacitor is reserved (here, too little fuel for
    // the debit) rolls the payment back. With no intervening action, the
    // entire original recharge curve is restored.
    let recharge_curve = {
        let mut curve=Charged::full(100000.0,1000.0,combat::now_ms());
        curve.set(combat::now_ms(),59000.0);
        f.db.world.lock().parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.capacitor=curve;
        curve
    };
    f.set_fuel(1000);
    assert!(matches!(super::super::activate(&ctx,&call).await,Outcome::UserError{..}));
    assert_eq!(f.db.world.lock().parks[&f.system].ships[&f.ship].fight.as_ref().unwrap().vitals.capacitor,recharge_curve);
    assert_eq!(f.fuel(),1000);
    assert!(!f.db.world.lock().parks[&f.system].ships[&f.ship].identity_reserved);
    f.set_fuel(150000);
    let victim=f.ship+1000000000;
    let mut notifications=f.db.world.lock().connect(1);
    {
        let mut world=f.db.world.lock();let park=world.parks.get_mut(&f.system).unwrap();
        std::sync::Arc::make_mut(&mut park.wildlife).security=0.3;
        let mut curve=Charged::full(100000.0,0.0,combat::now_ms());curve.set(combat::now_ms(),59000.0);
        park.ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.capacitor=curve;
        let mut target=park.ships[&f.ship].clone();target.id=victim;target.client_id=2;target.pilot.char_id=victim;target.pilot.corp_id=2;target.arrive(Vec3::new(1000.0,0.0,0.0));
        let fight=target.fight.as_mut().unwrap();fight.vitals.shield=Charged::full(100000.0,0.0,combat::now_ms());fight.vitals.resonance=[[1.0;4];3];
        park.ships.insert(victim,target);
    }
    assert!(matches!(super::super::activate(&ctx,&call).await,Outcome::Value(_)));
    assert_eq!(f.capacitor(),31500.0);assert_eq!(f.fuel(),75000);
    let started;
    {
        let world=f.db.world.lock();let park=&world.parks[&f.system];let ship=&park.ships[&f.ship];
        assert!(!ship.identity_reserved);let module=ship.fight.as_ref().unwrap().module(f.doom).unwrap();
        assert_eq!(module.cycles,1);assert!(!module.first_cycle_prepaid);started=module.clone();
        assert_eq!(park.ships[&victim].fight.as_ref().unwrap().vitals.shield.at(combat::now_ms()),53125.0);
        assert!(world.pending_jobs.iter().any(|job| matches!(job,crate::heartbeat::Job::Security {char_id,delta} if *char_id==f.session.char_id.unwrap() && *delta == -0.1)));
    }
    let mut damage_notice=false;let mut capacitor_notice=false;
    while let Ok(push)=notifications.try_recv() {
        if let crate::world::Push::Notify(note)=push {
            damage_notice |= note.event=="OnDamageMessage";
            capacitor_notice |= note.event=="OnModuleAttributeChanges";
        }
    }
    assert!(damage_notice);assert!(capacitor_notice);
    f.advance(); // The aligned warp proceeds only after the paid volley fired.
    {
        let mut world=f.db.world.lock();let park=world.parks.get_mut(&f.system).unwrap();let mut out=crate::heartbeat::Out::default();
        crate::fight::funded_first_cycle(park,f.ship,f.doom,started.next_ms,&mut out);
        assert!(park.ships[&f.ship].fight.as_ref().unwrap().module(f.doom).is_none());
        assert_eq!(park.ships[&victim].fight.as_ref().unwrap().vitals.shield.at(combat::now_ms()),53125.0);
    }
    assert_eq!(f.capacitor(),31500.0);
}
