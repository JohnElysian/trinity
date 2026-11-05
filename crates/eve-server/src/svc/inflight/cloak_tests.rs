use super::*;
use crate::ballpark::{Command,Park,Vec3};
use crate::session::Session;
use crate::world::{Presence,Push};

struct Fixture {
    db:crate::test_support::Database,session:Session,system:i64,ship:i64,module:i64,observer:i64,
    owner_rx:tokio::sync::mpsc::UnboundedReceiver<Push>,observer_rx:tokio::sync::mpsc::UnboundedReceiver<Push>,
}
impl Fixture {
    async fn new(covert:bool)->Self {
        let db=crate::test_support::Database::load().await;
        let pilot=i64::from(db.id().await);let ship=i64::from(db.id().await);let module=i64::from(db.id().await);let skill=i64::from(db.id().await);
        let system=30000142i64;
        let (hull_type,module_type)=if covert {(11188,11578)} else {(587,11370)};
        db.store.write(|tx| -> Result<(),StoreError> {
            tx.insert("character_",&[("characterID",store::Value::Int(pilot)),("characterName",store::Value::Text(format!("Cloak {pilot}").into())),
                ("shipID",store::Value::Int(ship)),("solarSystemID",store::Value::Int(system)),("stationID",store::Value::Int(0))])?;
            for (id,type_id,location,flag) in [(ship,hull_type,system,0),(module,module_type,ship,11),(skill,11579,pilot,7)] {
                tx.insert("entity",&[("itemID",store::Value::Int(id)),("typeID",store::Value::Int(type_id)),("ownerID",store::Value::Int(pilot)),
                    ("locationID",store::Value::Int(location)),("flag",store::Value::Int(flag)),("quantity",store::Value::Int(1)),("singleton",store::Value::Int(1))])?;
            }
            for (id,attribute,value) in [(module,2,1),(skill,280,5)] {
                tx.insert("entity_attributes",&[("itemID",store::Value::Int(id)),("attributeID",store::Value::Int(attribute)),("valueInt",store::Value::Int(value))])?;
            }
            Ok(())
        }).unwrap();
        let session=Session {char_id:Some(pilot),ship_id:Some(ship),solar_system_id:Some(system),solar_system_id2:Some(system),..Default::default()};
        let mut hull=ship_for_space(&db.ctx(&session),pilot,ship).await.unwrap();
        hull.stats.warp_capacitor_need=0.0;
        let observer=ship+1000000000;
        let mut other=hull.clone();other.id=observer;other.client_id=2;other.pilot.char_id=observer;other.pos=Vec3::new(20000.0,0.0,0.0);
        // Leave one elapsed simulation tick for the warp-entry step below.
        // Advancing a park created at wall-clock now by one would checkpoint
        // a future trajectory, correctly rejected by reconnect validation.
        let mut park=Park::new(system,vec![],crate::destiny::tick_now()-1);park.ships.insert(ship,hull);park.ships.insert(observer,other);
        let (owner_rx,observer_rx)={let mut world=db.world.lock();world.parks.insert(system,park);
            let rx1=world.connect(1);let rx2=world.connect(2);
            for (client,char_id) in [(1,pilot),(2,observer)] {
                world.set_presence(client,Some(Presence {char_id,corp_id:1,solar_system_id:system,docked:false,station_id:None,role:0}));world.sees(client,system);
            }
            (rx1,rx2)
        };
        Self {db,session,system,ship,module,observer,owner_rx,observer_rx}
    }
    fn call(&self,method:&str,covert:bool)->Call {
        Call {service:"dogmaIM".into(),method:method.into(),args:vec![Value::Int(self.module),Value::bytes(if covert {"cloakingWarpSafe"}else{"cloaking"}),Value::None,Value::Int(0)],kwargs:vec![]}
    }
    fn active(&self)->Option<Active> {self.db.world.lock().parks[&self.system].ships[&self.ship].fight.as_ref().unwrap().module(self.module).cloned()}
    fn forget_delays(&self) {self.db.world.lock().parks.get_mut(&self.system).unwrap().ships.get_mut(&self.ship).unwrap().fight.as_mut().unwrap().cloak_history=Default::default();}
}
fn contains(value:&Value,name:&str)->bool {
    value.as_string().is_some_and(|s|s==name) || value.seq().is_some_and(|values|values.iter().any(|v|contains(v,name)))
}
fn drain(rx:&mut tokio::sync::mpsc::UnboundedReceiver<Push>)->Vec<crate::world::Notification> {
    let mut out=vec![];while let Ok(push)=rx.try_recv() {if let Push::Notify(n)=push {out.push(n);}}out
}
fn user_error_is(outcome:Outcome,key:&str)->bool {matches!(outcome,Outcome::UserError {msg,..} if msg==key)}

#[tokio::test]
async fn continuous_cloak_native_visibility_immediate_stop_and_original_delays() {
    let mut f=Fixture::new(false).await;
    let ctx=f.db.ctx(&f.session);
    // Established outgoing and incoming target locks prevent activation.
    f.db.world.lock().parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().locks.insert(f.observer,Lock::Locked);
    assert!(matches!(activate(&ctx,&f.call("Activate",false)).await,Outcome::UserError {..}));
    f.db.world.lock().parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().locks.clear();
    f.db.world.lock().parks.get_mut(&f.system).unwrap().ships.get_mut(&f.observer).unwrap().fight.as_mut().unwrap().locks.insert(f.ship,Lock::Locked);
    assert!(matches!(activate(&ctx,&f.call("Activate",false)).await,Outcome::UserError {..}));
    f.db.world.lock().parks.get_mut(&f.system).unwrap().ships.get_mut(&f.observer).unwrap().fight.as_mut().unwrap().locks.insert(f.ship,Lock::Pending(combat::now_ms()+10000));
    f.db.world.lock().parks.get_mut(&f.system).unwrap().command(f.ship,Command::Warp {target:Vec3::new(5e12,0.0,0.0),range:0.0});
    let speed=f.db.world.lock().parks[&f.system].ships[&f.ship].stats.max_velocity;
    assert!(matches!(activate(&ctx,&f.call("Activate",false)).await,Outcome::Value(_)));
    let active=f.active().unwrap();assert!(active.continuous());assert_eq!(active.duration_ms,-1);assert_eq!(active.repeat,0);assert_eq!(active.cycles,0);
    assert!(matches!(active.action,Action::Cloak {targeting_delay_ms:15000,reactivation_delay_ms:30000,..}),"original Cloaking5 reduces30000ms by50%: {:?}",active.action);
    {
        let mut world=f.db.world.lock();let park=world.parks.get_mut(&f.system).unwrap();let ship=&park.ships[&f.ship];
        assert!(ship.cloaked);assert!(!ship.massive());assert!((ship.stats.max_velocity-speed*0.1).abs()<1e-6);
        assert!(!ship.warping(),"ordinary cloak cancels an already queued warp");
        assert!(!park.ships[&f.observer].fight.as_ref().unwrap().locks.contains_key(&f.ship));
        let before=ship.fight.as_ref().unwrap().vitals.capacitor;
        let mut out=crate::heartbeat::Out::default();crate::fight::funded_first_cycle(park,f.ship,f.module,combat::now_ms()+1000000,&mut out);
        assert_eq!(park.ships[&f.ship].fight.as_ref().unwrap().vitals.capacitor,before);
    }
    // Check counters outside the mutex: continuous state never cycles.
    assert_eq!(f.active().unwrap().cycles,0);
    let owner=drain(&mut f.owner_rx);let observer=drain(&mut f.observer_rx);
    assert!(owner.iter().any(|n|contains(&n.args,"CloakBall")));
    let started=owner.iter().find(|n|n.event=="OnGodmaShipEffect").unwrap();assert_eq!(started.args.seq().unwrap()[7].as_i64(),Some(-1));
    assert!(observer.iter().any(|n|contains(&n.args,"RemoveBalls")));
    assert_eq!(f.db.world.lock().with_hidden(2,|hidden|hidden.contains(&f.ship)),Some(true));
    {let world=f.db.world.lock();space::broadcast(&world,f.system,None,vec![entry(1,"SetMaxSpeed",vec![Value::Int(f.ship),Value::Double(1.0)])]);}
    assert!(drain(&mut f.observer_rx).is_empty(),"hidden ships do not leak later native updates");
    assert!(user_error_is(add_target(&ctx,f.observer),"DeniedTargetingCloaked"));
    assert!(matches!(deactivate(&ctx,&f.call("Deactivate",false)).await,Outcome::Value(_)));
    assert!(f.active().is_none());assert!(!f.db.world.lock().parks[&f.system].ships[&f.ship].cloaked);
    assert!(drain(&mut f.owner_rx).iter().any(|n|contains(&n.args,"UncloakBall")));
    assert!(drain(&mut f.observer_rx).iter().any(|n|contains(&n.args,"AddBalls")));
    assert!(user_error_is(activate(&ctx,&f.call("Activate",false)).await,"ModuleReactivationDelayed"));
    assert!(user_error_is(add_target(&ctx,f.observer),"DeniedTargetAfterCloak"));
    let stopped=f.db.world.lock().parks[&f.system].ships[&f.ship].clone();
    assert_eq!(stopped.stats.max_velocity,speed);
    let tick=f.db.world.lock().parks[&f.system].tick;
    assert!(crate::space_state::save_detached(&f.db.store,f.system,&stopped,tick).await.unwrap());
    let mut restored=ship_for_space(&ctx,f.session.char_id.unwrap(),f.ship).await.unwrap();
    assert!(crate::space_state::restore(&ctx,f.system,&mut restored).await.unwrap());
    assert_eq!(restored.fight.as_ref().unwrap().cloak_history,stopped.fight.as_ref().unwrap().cloak_history);
    assert!(!restored.cloaked);
    f.forget_delays();assert!(matches!(activate(&ctx,&f.call("Activate",false)).await,Outcome::Value(_)));
    crate::cloak::uncloaked(&mut f.db.world.lock(),f.system,f.ship);assert!(f.db.world.lock().parks[&f.system].ships[&f.ship].cloaked,"stale Uncloak job cannot reveal a newer cloak");
    {
        let mut world=f.db.world.lock();let park=world.parks.get_mut(&f.system).unwrap();park.ships.get_mut(&f.observer).unwrap().pos=Vec3::new(1000.0,0.0,0.0);
        let mut out=crate::heartbeat::Out::default();crate::cloak::proximity(park,combat::now_ms(),&mut out);
        let jobs=out.deliver(&world);world.pending_jobs.extend(jobs);crate::cloak::uncloaked(&mut world,f.system,f.ship);
    }
    assert!(f.active().is_none());assert!(!f.db.world.lock().parks[&f.system].ships[&f.ship].cloaked);
}

#[tokio::test]
async fn covert_cloak_stays_continuous_in_warp_and_reconnect_validates_fitted_item() {
    let f=Fixture::new(true).await;let ctx=f.db.ctx(&f.session);
    f.db.world.lock().parks.get_mut(&f.system).unwrap().command(f.ship,Command::Warp {target:Vec3::new(5e12,0.0,0.0),range:0.0});
    assert!(matches!(activate(&ctx,&f.call("Activate",true)).await,Outcome::Value(_)));
    {
        let mut world=f.db.world.lock();let park=world.parks.get_mut(&f.system).unwrap();let ship=park.ships.get_mut(&f.ship).unwrap();
        assert!(ship.warping(),"covert cloak preserves alignment");
        ship.stats.warp_capacitor_need=0.0;ship.vel=Vec3::new(ship.stats.max_velocity,0.0,0.0);
        park.command(f.ship,Command::Warp {target:Vec3::new(5e12,0.0,0.0),range:0.0});park.advance(park.tick+1);
        assert!(park.ships[&f.ship].in_warp());assert_eq!(crate::cloak::warp_safe(park,f.ship),Some(true));
        let mut out=crate::heartbeat::Out::default();crate::fight::funded_first_cycle(park,f.ship,f.module,combat::now_ms()+1000000,&mut out);
        assert_eq!(park.ships[&f.ship].fight.as_ref().unwrap().module(f.module).unwrap().cycles,0);
    }
    let snapshot=f.db.world.lock().parks[&f.system].ships[&f.ship].clone();let tick=f.db.world.lock().parks[&f.system].tick;
    assert!(tick<=crate::destiny::tick_now(),"the saved warp is an elapsed, not future, simulation state");
    assert!(crate::space_state::save_detached(&f.db.store,f.system,&snapshot,tick).await.unwrap());
    let mut restored=ship_for_space(&ctx,f.session.char_id.unwrap(),f.ship).await.unwrap();
    assert!(crate::space_state::restore(&ctx,f.system,&mut restored).await.unwrap());
    assert!(restored.cloaked);assert!(restored.in_warp());assert_eq!(restored.fight.as_ref().unwrap().module(f.module).unwrap().started_ms,f.active().unwrap().started_ms);
    assert_eq!(restored.stats.max_velocity,snapshot.stats.max_velocity);
    assert!(matches!(deactivate(&ctx,&f.call("Deactivate",true)).await,Outcome::Value(_)));
    f.forget_delays();assert!(matches!(activate(&ctx,&f.call("Activate",true)).await,Outcome::Value(_)),"a warp-safe cloak may activate during the actual warp");
    f.db.store.write(|tx| {
        let Some(row)=tx.table("entity_attributes").get(&[store::Value::Int(f.module),store::Value::Int(2)]).map(|r|r.id) else {return Ok(())};
        tx.update("entity_attributes",row,&[("valueInt",store::Value::Int(0))])
    }).unwrap();
    let mut offline=ship_for_space(&ctx,f.session.char_id.unwrap(),f.ship).await.unwrap();
    assert!(crate::space_state::restore(&ctx,f.system,&mut offline).await.unwrap());
    assert!(!offline.cloaked);assert!(offline.fight.as_ref().unwrap().modules.is_empty());
}
