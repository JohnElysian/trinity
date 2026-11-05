use super::*;
use crate::{ballpark::Park,combat,items::ItemTxn,services::{Call,Outcome},session::Session,store::Sets,svc::{inflight,inventory::{Container,CONTAINER_HANGAR},moves},test_support::Database};

struct Fixture {db:Database,session:Session,ship:Ship,module:i64,station:i64,other:i64}

/// Set columns of the row whose single-column key is `id` (nothing if there
/// is none, as an UPDATE matching no row).
fn set(tx:&mut Tx<'_>,table:&str,id:i64,sets:Sets)->Result<(),StoreError> {
    match tx.table(table).get_id(id).map(|r|r.id) {
        Some(row)=>tx.update(table,row,sets),
        None=>Ok(()),
    }
}

fn entity(id:i64,ty:i64,owner:i64,location:i64,flag:i64)->Vec<(&'static str,Value)> {
    vec![("itemID",Value::Int(id)),("typeID",Value::Int(ty)),("ownerID",Value::Int(owner)),("locationID",Value::Int(location)),
        ("flag",Value::Int(flag)),("quantity",Value::Int(1)),("singleton",Value::Int(1))]
}

impl Fixture {
    async fn new()->Self {
        let db=Database::load().await;
        let pilot=i64::from(db.id().await);let hull=i64::from(db.id().await);let module=i64::from(db.id().await);let other=i64::from(db.id().await);
        let (station,system)=db.store.read(|d|d.table("staStations").rows().filter_map(|r|Some((r.int("stationID")?,r.int("solarSystemID")?))).min()).unwrap();
        db.store.write(|tx|->Result<(),StoreError> {
            tx.insert("character_",&[("characterID",Value::Int(pilot)),("characterName",Value::Text(format!("Cooldown {pilot}").into())),
                ("shipID",Value::Int(hull)),("solarSystemID",Value::Int(system)),("stationID",Value::Int(0))])?;
            tx.insert("entity",&entity(hull,587,pilot,system,0))?;
            tx.insert("entity",&entity(module,11370,pilot,hull,11))?;
            tx.insert("entity",&entity(other,587,pilot,system,0))?;
            tx.insert("entity_attributes",&[("itemID",Value::Int(module)),("attributeID",Value::Int(2)),("valueInt",Value::Int(1))])?;
            Ok(())
        }).unwrap();
        let session=Session {char_id:Some(pilot),ship_id:Some(hull),solar_system_id:Some(system),solar_system_id2:Some(system),..Default::default()};
        let ship=inflight::ship_for_space(&db.ctx(&session),pilot,hull).await.unwrap();
        let mut park=Park::new(system,vec![],crate::destiny::tick_now());park.ships.insert(ship.id,ship);
        db.world.lock().parks.insert(system,park);
        for method in ["Activate","Deactivate"] {
            let call=Call {service:"dogmaIM".into(),method:method.into(),args:vec![eve_marshal::Value::Int(module),eve_marshal::Value::bytes("cloaking"),eve_marshal::Value::None,eve_marshal::Value::Int(0)],kwargs:vec![]};
            assert!(matches!(inflight::dogma_im(&call,&db.ctx(&session)).await,Some(Outcome::Value(_))),"{method}");
        }
        let ship=db.world.lock().parks[&system].ships[&hull].clone();
        assert!(ship.fight.as_ref().unwrap().cloak_history.module_ready[&module]>combat::now_ms());
        Self {db,session,ship,module,station,other}
    }
    fn history(&self)->History {self.ship.fight.as_ref().unwrap().cloak_history.clone()}
    fn docked(&self)->Session {Session {station_id:Some(self.station),solar_system_id:None,..self.session.clone()}}
    fn load(&self,owner:i64,ship:i64,now:i64)->History {
        self.db.store.read(|d|load_history(d,owner,ship,now)).unwrap()
    }
    fn save(&self) {
        self.db.store.write(|tx|save_history(tx,&self.ship,combat::now_ms())).unwrap();
    }
    fn set(&self,table:&str,id:i64,sets:Sets) {
        self.db.store.write(|tx|set(tx,table,id,sets)).unwrap();
    }
    async fn move_module(&self,docked:&Session)->Outcome {
        let call=Call {service:"invcontainer".into(),method:"Add".into(),args:vec![eve_marshal::Value::Int(self.module),eve_marshal::Value::None,eve_marshal::Value::Int(4)],kwargs:vec![]};
        moves::container_write(&self.db.ctx(docked),Container::Special {container_id:CONTAINER_HANGAR,owner_id:None},&call).await.unwrap()
    }
}

#[tokio::test]
async fn observed_cloak_deadlines_survive_dock_unfit_checks_and_undock_without_restarting() {
    let f=Fixture::new().await;let expected=f.history();let now=combat::now_ms();
    let (pilot,hull,system)=(f.ship.pilot.char_id,f.ship.id,f.session.solar_system_id.unwrap());
    assert!(crate::space_state::save_detached(&f.db.store,system,&f.ship,crate::destiny::tick_now()).await.unwrap());
    f.db.store.write(|tx|->Result<(),StoreError> {
        set(tx,"character_",pilot,&[("stationID",Value::Int(f.station))])?;
        set(tx,"entity",hull,&[("locationID",Value::Int(f.station)),("flag",Value::Int(4))])?;
        save_history(tx,&f.ship,now)
    }).unwrap();
    f.db.world.lock().parks.get_mut(&system).unwrap().ships.remove(&hull);
    assert!(f.db.store.read(|d|d.table("shipSpaceMotion").find(&["itemID"],&[Value::Int(hull)]).next().is_none()));
    assert_eq!(f.load(pilot,hull,now),expected);
    let docked=f.docked();
    assert!(matches!(f.move_module(&docked).await,Outcome::UserError {msg,..} if msg=="CannotRemoveModuleReactivationDelayed"));
    let restored=inflight::ship_for_space(&f.db.ctx(&docked),pilot,hull).await.unwrap();
    assert!(!restored.cloaked);assert!(restored.fight.as_ref().unwrap().modules.is_empty());
    assert_eq!(restored.fight.as_ref().unwrap().cloak_history,expected,"docked reconnect reads the original deadlines only");
    f.db.store.write(|tx|->Result<(),StoreError> {
        set(tx,"character_",pilot,&[("stationID",Value::Int(0))])?;
        set(tx,"entity",hull,&[("locationID",Value::Int(system)),("flag",Value::Int(0))])?;
        save_history(tx,&restored,now+1)
    }).unwrap();
    let loaded=inflight::ship_for_space(&f.db.ctx(&f.session),pilot,hull).await.unwrap();
    assert_eq!(loaded.fight.as_ref().unwrap().cloak_history,expected);
    f.db.world.lock().parks.get_mut(&system).unwrap().ships.insert(hull,loaded);
    let activate=Call {service:"dogmaIM".into(),method:"Activate".into(),args:vec![eve_marshal::Value::Int(f.module),eve_marshal::Value::bytes("cloaking"),eve_marshal::Value::None,eve_marshal::Value::Int(0)],kwargs:vec![]};
    assert!(matches!(inflight::dogma_im(&activate,&f.db.ctx(&f.session)).await,Some(Outcome::UserError {msg,..}) if msg=="ModuleReactivationDelayed"));
    let end=expected.module_ready[&f.module].max(expected.targeting_at);
    assert_eq!(f.load(pilot,hull,end+1),History::default());
    let remaining=f.db.store.write(|tx| {
        let mut items=ItemTxn::new(tx);let item=items.item(f.module)?.unwrap();
        module_reactivation_remaining(items.db(),&item,end+1)
    }).unwrap();
    assert_eq!(remaining,None);
    assert_eq!(f.load(pilot,hull,now),expected,"querying expiry does not create a later deadline");
}

#[tokio::test]
async fn cooldown_transaction_rollback_and_custody_changes_cannot_leak_between_hulls_or_owners() {
    let f=Fixture::new().await;let expected=f.history();let now=combat::now_ms();f.save();
    let (pilot,hull)=(f.ship.pilot.char_id,f.ship.id);
    let mut newer=f.ship.clone();newer.fight.as_mut().unwrap().cloak_history.module_ready.insert(f.module,expected.module_ready[&f.module]+1000);
    let rolled_back=f.db.store.write(|tx|->Result<(),StoreError> {
        set(tx,"entity",hull,&[("locationID",Value::Int(f.station)),("flag",Value::Int(4))])?;
        save_history(tx,&newer,now)?;
        Err(StoreError {code:crate::store::DATA_EXCEPTION,message:"rolled back".into()})
    });
    assert!(rolled_back.is_err());
    assert_eq!(f.load(pilot,hull,now),expected);
    assert_eq!(f.db.store.read(|d|d.table("entity").get_id(hull).unwrap().int("locationID")),f.session.solar_system_id);
    assert_eq!(f.load(pilot,f.other,now),History::default());
    assert_eq!(f.load(pilot+1,hull,now),History::default());
    f.set("entity",f.module,&[("locationID",Value::Int(f.other))]);
    assert!(!f.load(pilot,hull,now).module_ready.contains_key(&f.module));
    assert_eq!(f.load(pilot,f.other,now),History::default());
    f.set("entity",f.module,&[("locationID",Value::Int(hull))]);
    assert!(!f.load(pilot,hull,now).module_ready.contains_key(&f.module),"returning an item cannot revive invalidated durable custody");
    f.save();
    f.set("cloakCooldowns",f.module,&[("readyAt",Value::Int(i64::MAX))]);
    assert!(!f.load(pilot,hull,now).module_ready.contains_key(&f.module));
    f.save();
    assert_eq!(f.load(pilot,hull,now),expected,"an invalid stored value cannot override a valid observed deadline");
    f.set("cloakCooldowns",f.module,&[("typeID",Value::Int(11578)),("readyAt",Value::Int(expected.module_ready[&f.module]))]);
    assert!(!f.load(pilot,hull,now).module_ready.contains_key(&f.module));
    f.set("entity",hull,&[("ownerID",Value::Int(pilot+1))]);
    f.set("entity",hull,&[("ownerID",Value::Int(pilot))]);
    assert_eq!(f.load(pilot,hull,now),History::default());
}

/// A module moved off its hull before the save (once a concurrent move that
/// held its row lock) is not saved as fitted.
#[tokio::test]
async fn cooldown_save_rechecks_module_custody_after_it_moves() {
    let f=Fixture::new().await;f.save();
    f.set("entity",f.module,&[("locationID",Value::Int(f.other))]);
    f.save();
    assert!(!f.load(f.ship.pilot.char_id,f.ship.id,combat::now_ms()).module_ready.contains_key(&f.module));
}
