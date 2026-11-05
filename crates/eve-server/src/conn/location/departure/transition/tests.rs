use super::*;
use crate::{ballpark::{Celestial,Park},combat::{Action,Active,Charged,Lock},test_support::Database};
use std::sync::Arc;

struct Fixture {server:Arc<Server>,session:Session,ship:i64,station:i64,from:i64,from_gate:i64,to_gate:i64,to:Place,enemy:i64}
impl Fixture {
    async fn new()->Self {
        let db=Database::load().await;
        let pilot=i64::from(db.id().await);let ship=i64::from(db.id().await);let corp=i64::from(db.id().await);
        let station=i64::from(db.id().await);let enemy=i64::from(db.id().await);
        // The first gate (by ID) of a mapJumps pair joining two systems.
        let (from_gate,from,constellation,region,to_gate,to)=db.store.read(|d| {
            let map=d.table("mapDenormalize");
            crate::sde::table("mapJumps").rows().filter_map(|j| {
                let g=map.get_id(j.int("stargateID")?)?;let e=map.get_id(j.int("celestialID")?)?;
                if g.int("groupID")!=Some(10) || e.int("groupID")!=Some(10) || g.int("solarSystemID")==e.int("solarSystemID") {return None;}
                let at=|c:&str|e.float(c).unwrap_or(0.0);
                Some((g.int("itemID")?,g.int("solarSystemID")?,g.int("constellationID")?,g.int("regionID")?,e.int("itemID")?,
                    Place {station:None,system:e.int("solarSystemID")?,constellation:e.int("constellationID")?,region:e.int("regionID")?,
                        at:space::arrival_point(Vec3::new(at("x"),at("y"),at("z")),at("radius"))}))
            }).min_by_key(|gate|gate.0).unwrap()
        });
        db.store.write(|tx| {
            tx.insert("corporation",&[("corporationID",StoreValue::Int(corp)),("corporationName",StoreValue::Text(format!("Travel corp {corp}").into()))])?;
            tx.insert("character_",&[("characterID",StoreValue::Int(pilot)),("characterName",StoreValue::Text(format!("Transition pilot {pilot}").into())),
                ("corporationID",StoreValue::Int(corp)),("shipID",StoreValue::Int(ship)),("solarSystemID",StoreValue::Int(from)),("stationID",StoreValue::Int(0))])?;
            for (id,ty,flag) in [(ship,587,0),(pilot,1373,57)] {
                tx.insert("entity",&[("itemID",StoreValue::Int(id)),("typeID",StoreValue::Int(ty)),("ownerID",StoreValue::Int(pilot)),
                    ("locationID",StoreValue::Int(from)),("flag",StoreValue::Int(flag)),("quantity",StoreValue::Int(1)),("singleton",StoreValue::Int(1))])?;
            }
            tx.insert("staStations",&[("stationID",StoreValue::Int(station)),("stationTypeID",StoreValue::Int(1529)),("corporationID",StoreValue::Int(corp)),
                ("solarSystemID",StoreValue::Int(from)),("constellationID",StoreValue::Int(constellation)),("regionID",StoreValue::Int(region)),
                ("stationName",StoreValue::Text(format!("Transition station {station}").into())),
                ("x",StoreValue::Float(0.0)),("y",StoreValue::Float(0.0)),("z",StoreValue::Float(0.0))])
        }).unwrap();
        let session=Session {char_id:Some(pilot),ship_id:Some(ship),corp_id:Some(corp),solar_system_id:Some(from),solar_system_id2:Some(from),role:8,..Default::default()};
        let mut hull=inflight::ship_for_space(&db.ctx(&session),pilot,ship).await.unwrap();
        hull.pos=Vec3::new(2000.0,0.0,0.0);hull.fight.as_mut().unwrap().vitals.capacitor=Charged::full(1000.0,0.0,combat::now_ms());
        let body=|id,group|Celestial {id,type_id:1529,group_id:group,name:"Transition endpoint".into(),pos:Vec3::ZERO,radius:1000.0,jump:None};
        let mut park=Park::new(from,vec![body(station,15),body(from_gate,10)],destiny::tick_now());
        let module=|id,target|Active {module_id:id,type_id:3651,effect_id:4,guid:"effects.Test".into(),offensive:true,warp_safe:false,disallow_warp_alignment:false,
            action:Action::Passive,target,ammo:None,duration_ms:1000,cap_need:0.0,first_cycle_prepaid:false,started_ms:combat::now_ms(),next_ms:i64::MAX,
            cycles:1,repeat:1000,stopping:false,name:"Travel test weapon".into(),reach:None,landed:true};
        let mut attacker=hull.clone();attacker.id=enemy;attacker.client_id=0;attacker.pilot.char_id=enemy;
        attacker.fight.as_mut().unwrap().locks.insert(ship,Lock::Locked);
        attacker.fight.as_mut().unwrap().modules.push(module(enemy+1,Some(ship)));
        hull.fight.as_mut().unwrap().locks.insert(enemy,Lock::Locked);
        hull.fight.as_mut().unwrap().modules.push(module(enemy+2,None));
        park.ships.insert(ship,hull);park.ships.insert(enemy,attacker);
        park.missiles.push(crate::npc::Missile {attacker:crate::npc::combat_ref::Attacker {ship_id:enemy,char_id:enemy,client_id:None,ship_type:587,weapon_type:3651},
            target:ship,impact_ms:i64::MAX,damage:[1.0,0.0,0.0,0.0],cloud:100.0,aoe_velocity:100.0,reaches:true,weapon:"Travel missile".into()});
        {let mut w=db.world.lock();w.parks.insert(from,park);w.parks.insert(to.system,Park::new(to.system,vec![],destiny::tick_now()));
            w.maintenance.insert(ship,crate::svc::maintenance::Configuration {allow_fleet:true,pilot,system:from});}
        let server=Arc::new(Server {table:Default::default(),store:db.store,node_id:1,bulk:db.bulk,dogma:db.dogma,world:db.world,
            services:crate::svc::registry(),dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0});
        Self {server,session,ship,station,from,from_gate,to_gate,to,enemy}
    }
    fn request(&self,kind:u8)->Request {match kind {0=>Request::Gate {from_gate:self.from_gate,to_gate:self.to_gate,to:self.to},1=>Request::Dock {station:self.station,towed:false},_=>Request::Admin {to:Place{system:self.from,at:Vec3::new(9e7,3e7,1e7),..self.to}}}}
    fn pilot(&self)->i64 {self.session.char_id.unwrap()}
    fn unchanged(&self) {
        let saved=self.server.store.read(|db| {
            let c=db.table("character_").get_id(self.pilot()).unwrap();let e=db.table("entity").get_id(c.int("shipID").unwrap()).unwrap();
            (c.int("stationID"),c.int("solarSystemID"),e.int("locationID"),e.int("flag"))
        });
        assert_eq!(saved,(Some(0),Some(self.from),Some(self.from),Some(0)));
        let w=self.server.world.lock();let p=&w.parks[&self.from];let ship=&p.ships[&self.ship];let fight=ship.fight.as_ref().unwrap();
        assert!(!ship.identity_reserved);assert_eq!(fight.modules.len(),1);assert_eq!(fight.locks.get(&self.enemy),Some(&Lock::Locked));
        assert_eq!(p.ships[&self.enemy].fight.as_ref().unwrap().locks.get(&self.ship),Some(&Lock::Locked));assert_eq!(p.missiles.len(),1);
        assert!(w.maintenance[&self.ship].allow_fleet);
    }
}

#[tokio::test]
async fn refused_gate_dock_and_admin_transitions_keep_live_combat_and_damage() {
    for kind in 0..3 {
        let f=Fixture::new().await;
        {let mut w=f.server.world.lock();let fight=w.parks.get_mut(&f.from).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap();
            fight.vitals.hull_damage=17.0;assert!(fight.vitals.capacitor.spend(combat::now_ms(),50.0));}
        // Each is refused inside the write, after the live hull was reserved.
        let request=match kind {
            0=>Request::Gate {from_gate:f.from_gate,to_gate:f.to_gate,to:Place {system:f.from,..f.to}},
            1=>Request::Dock {station:f.station+1_000_000,towed:false},
            _=>Request::Admin {to:Place {system:1,at:Vec3::new(9e7,3e7,1e7),..f.to}},
        };
        assert!(matches!(run(&f.server,&f.session,1,request).await,Err(Failure::Refused(_))));
        f.unchanged();
        assert!(!crate::travel::blocked_character_in_memory(&f.server.world,f.pilot()));
        assert!(!f.server.store.read(|db|crate::losses::pending::blocked_character(db,f.pilot())).unwrap());
        let w=f.server.world.lock();let fight=w.parks[&f.from].ships[&f.ship].fight.as_ref().unwrap();
        assert_eq!(fight.vitals.hull_damage,17.0);assert_eq!(fight.vitals.capacitor.at(combat::now_ms()),950.0);
    }
}

#[tokio::test]
async fn reserved_gate_and_dock_reject_out_of_range_and_destroyed_hulls() {
    for (kind,change) in [(0,0),(1,0),(0,1),(1,1)] {
        let f=Fixture::new().await;
        {let mut w=f.server.world.lock();let ship=w.parks.get_mut(&f.from).unwrap().ships.get_mut(&f.ship).unwrap();
            if change==0 {ship.pos=Vec3::new(1e8,0.0,0.0);}else{let v=&mut ship.fight.as_mut().unwrap().vitals;v.hull_damage=v.hull_max;}}
        assert!(run(&f.server,&f.session,1,f.request(kind)).await.is_err());
        f.unchanged();
    }
}

#[tokio::test]
async fn accepted_gate_dock_and_admin_publish_latest_damage_and_expected_location() {
    for kind in 0..3 {
        let f=Fixture::new().await;
        f.server.world.lock().parks.get_mut(&f.from).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.hull_damage=19.0;
        let arrived=run(&f.server,&f.session,1,f.request(kind)).await.unwrap();
        let saved=f.server.store.read(|db| {
            let c=db.table("character_").get_id(f.pilot()).unwrap();let ship=c.int("shipID").unwrap();
            let e=db.table("entity").get_id(ship).unwrap();
            let damage=db.table("entity_attributes").get(&[StoreValue::Int(ship),StoreValue::Int(3)]).and_then(|a|a.float("valueFloat"));
            (c.int("stationID"),c.int("solarSystemID"),e.int("locationID"),damage)
        });
        assert_eq!(saved,(Some(arrived.place.station.unwrap_or(0)),Some(arrived.place.system),
            Some(arrived.place.station.unwrap_or(arrived.place.system)),Some(19.0)));
        let w=f.server.world.lock();if kind==1 {assert!(!w.parks[&f.from].ships.contains_key(&f.ship));assert!(!w.maintenance.contains_key(&f.ship));}
        else {let hull=&w.parks[&arrived.place.system].ships[&f.ship];assert!(!hull.identity_reserved);assert_eq!(hull.pos,arrived.place.at);
            assert_eq!(hull.fight.as_ref().unwrap().vitals.hull_damage,19.0);assert_eq!(hull.arrived_through,if kind==0 {Some(f.to_gate)}else{None});
            if kind==2 {assert!(w.maintenance[&f.ship].allow_fleet);}else{assert!(!w.maintenance.contains_key(&f.ship));}}
    }
}

#[tokio::test]
async fn delayed_healthy_arrival_preserves_reconnected_motion_and_newer_boarding() {
    let f=Fixture::new().await;
    let captured=f.server.world.lock().parks[&f.from].ships[&f.ship].clone();
    run(&f.server,&f.session,1,f.request(0)).await.unwrap();
    let marker=f.server.store.read(|db| {
        let r=db.table("travelReceipts").rows().filter(|r|r.int("shipID")==Some(f.ship)).max_by_key(|r|r.int("generation")).unwrap();
        crate::travel::Marker {generation:r.int("generation").unwrap(),pilot:r.int("characterID").unwrap(),ship:r.int("shipID").unwrap(),
            ship_type:r.int("shipTypeID").unwrap(),source:serde_json::from_str(r.text("source").unwrap()).unwrap(),
            destination:serde_json::from_str(r.text("destination").unwrap()).unwrap()}
    });
    let profile=||f.server.store.read(|db|inflight::profile(db,&f.server.dogma,marker.pilot,marker.ship,&Default::default(),combat::now_ms())).unwrap().unwrap();
    let pos=Vec3::new(7000.0,8000.0,9000.0);let vel=Vec3::new(4.0,5.0,6.0);
    {let mut world=f.server.world.lock();let ship=world.parks.get_mut(&f.to.system).unwrap().ships.get_mut(&f.ship).unwrap();ship.pos=pos;ship.vel=vel;ship.client_id=55;}
    super::super::completion::finish_space(&f.server,marker,captured.clone(),profile(),Some(f.to_gate)).await.unwrap();
    {let world=f.server.world.lock();let ship=&world.parks[&f.to.system].ships[&f.ship];assert_eq!(ship.pos,pos);assert_eq!(ship.vel,vel);assert_eq!(ship.client_id,55);}
    let mut current=f.session.clone();current.solar_system_id=Some(f.to.system);current.solar_system_id2=Some(f.to.system);
    let newer=Place {at:Vec3::new(9e8,8e8,7e8),..f.to};
    run(&f.server,&current,55,Request::Admin {to:newer}).await.unwrap();
    f.server.world.lock().parks.get_mut(&f.to.system).unwrap().ships.remove(&f.ship);
    assert!(matches!(super::super::completion::finish_space(&f.server,marker,captured.clone(),profile(),Some(f.to_gate)).await,Err(Failure::Pending(_))),
        "same-hull same-system later relocation also supersedes the old generation");
    assert!(!f.server.world.lock().parks[&f.to.system].ships.contains_key(&f.ship));
    assert_eq!(f.server.store.read(|db|db.table("entity").get_id(f.ship).unwrap().float("x")),Some(newer.at.x));
    // A newer boarding supersedes the delayed arrival too.
    f.server.store.write(|tx| {
        let replacement=tx.nextval("dyn_id_seq")?;
        tx.insert("entity",&[("itemID",StoreValue::Int(replacement)),("typeID",StoreValue::Int(587)),("ownerID",StoreValue::Int(marker.pilot)),
            ("locationID",StoreValue::Int(f.to.system)),("flag",StoreValue::Int(0)),("quantity",StoreValue::Int(1)),("singleton",StoreValue::Int(1))])?;
        let pilot=tx.table("character_").get_id(marker.pilot).unwrap().id;
        tx.update("character_",pilot,&[("shipID",StoreValue::Int(replacement))])
    }).unwrap();
    assert!(matches!(super::super::completion::finish_space(&f.server,marker,captured,profile(),Some(f.to_gate)).await,Err(Failure::Pending(_))));
    assert!(!f.server.world.lock().parks[&f.to.system].ships.contains_key(&f.ship));
}

#[tokio::test]
async fn docking_stops_active_cloak_at_publication_and_persists_original_deadlines() {
    let f=Fixture::new().await;
    let module=f.server.store.write(|tx| {
        let module=tx.nextval("dyn_id_seq")?;
        tx.insert("entity",&[("itemID",StoreValue::Int(module)),("typeID",StoreValue::Int(11370)),("ownerID",StoreValue::Int(f.pilot())),
            ("locationID",StoreValue::Int(f.ship)),("flag",StoreValue::Int(11)),("quantity",StoreValue::Int(1)),("singleton",StoreValue::Int(1))])?;
        tx.insert("entity_attributes",&[("itemID",StoreValue::Int(module)),("attributeID",StoreValue::Int(2)),("valueInt",StoreValue::Int(1))])?;
        Ok::<_,crate::store::StoreError>(module)
    }).unwrap();
    {let mut w=f.server.world.lock();let p=w.parks.get_mut(&f.from).unwrap();p.ships.remove(&f.enemy);p.missiles.clear();
        let ship=p.ships.get_mut(&f.ship).unwrap();ship.arrive(Vec3::new(1e8,0.0,0.0));let fight=ship.fight.as_mut().unwrap();fight.locks.clear();fight.modules.clear();}
    let ctx=crate::services::Ctx {bulk:&f.server.bulk,dogma:&f.server.dogma,session:&f.session,store:&f.server.store,
        node_id:1,world:&f.server.world,client_id:1};
    let activate=crate::services::Call {service:"dogmaIM".into(),method:"Activate".into(),args:vec![Value::Int(module),Value::bytes("cloaking"),Value::None,Value::Int(0)],kwargs:vec![]};
    assert!(matches!(inflight::dogma_im(&activate,&ctx).await,Some(crate::services::Outcome::Value(_))));
    {let w=f.server.world.lock();assert!(w.parks[&f.from].ships[&f.ship].cloaked);}
    let before=combat::now_ms();
    let arrived=run(&f.server,&f.session,1,Request::Dock {station:f.station,towed:true}).await.unwrap();
    let history=f.server.store.read(|db|crate::cloak::durability::load_history(db,f.pilot(),f.ship,combat::now_ms())).unwrap();
    assert!(history.module_ready[&module]>before);assert!(history.targeting_at>before);
    let docked=Session {station_id:arrived.place.station,solar_system_id:None,..f.session.clone()};
    let ctx=crate::services::Ctx {session:&docked,..ctx};
    let loaded=inflight::ship_for_space(&ctx,f.pilot(),f.ship).await.unwrap();
    assert!(!loaded.cloaked);assert!(loaded.fight.as_ref().unwrap().modules.is_empty());assert_eq!(loaded.fight.as_ref().unwrap().cloak_history,history);
}

#[tokio::test]
async fn same_system_admin_teleport_refreshes_owner_ego_and_keeps_maintenance_preference() {
    let f=Fixture::new().await;
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (peer,accepted)=tokio::join!(tokio::net::TcpStream::connect(listener.local_addr().unwrap()),listener.accept());
    let _peer=peer.unwrap();let (socket,address)=accepted.unwrap();
    let mut conn=crate::conn::Conn::new(socket,address,f.server.clone(),0);assert_eq!(conn.client_id,1);conn.session=f.session.clone();
    let mut events={let mut w=f.server.world.lock();let events=w.connect(1);w.set_presence(1,conn.presence());events};
    let at=Vec3::new(9e7,3e7,1e7);
    conn.teleport(None,f.from,0,0,at).await.unwrap();
    fn contains_set_state(value:&Value)->bool {value.as_string().as_deref()==Some("SetState") || value.seq().is_some_and(|seq|seq.iter().any(contains_set_state))}
    let mut reset=false;
    while let Ok(push)=events.try_recv() {if let crate::world::Push::Notify(n)=push {if n.event.starts_with("DoDestiny") && contains_set_state(&n.args) {reset=true;}}}
    assert!(reset,"same-system teleport must reset the owner's original native ego state");
    let w=f.server.world.lock();assert_eq!(w.parks[&f.from].ships[&f.ship].pos,at);assert!(w.maintenance[&f.ship].allow_fleet);
    assert_eq!(conn.session.solar_system_id,Some(f.from));assert_eq!(conn.session.ship_id,Some(f.ship));
}
