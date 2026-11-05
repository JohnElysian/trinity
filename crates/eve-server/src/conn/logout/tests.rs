use super::*;
use crate::{ballpark::{Park,Vec3},session::Session,test_support::Database};

struct Fixture {db:Database,session:Session,id:i64,pilot:i64,system:i64}
impl Fixture {
    async fn new()->Self {
        let db=Database::load().await;let pilot=i64::from(db.id().await);let id=i64::from(db.id().await);let system=30000142_i64;
        db.store.write(|tx| {
            tx.insert("character_",&[("characterID",Value::Int(pilot)),("characterName",Value::Text(format!("Retained logout {pilot}").into())),
                ("shipID",Value::Int(id)),("solarSystemID",Value::Int(system)),("stationID",Value::Int(0))])?;
            tx.insert("entity",&[("itemID",Value::Int(id)),("typeID",Value::Int(587)),("ownerID",Value::Int(pilot)),("locationID",Value::Int(system)),
                ("flag",Value::Int(0)),("quantity",Value::Int(1)),("singleton",Value::Int(1))])
        }).unwrap();
        let session=Session {char_id:Some(pilot),ship_id:Some(id),solar_system_id:Some(system),..Default::default()};
        let ship=crate::svc::inflight::ship_for_space(&db.ctx(&session),pilot,id).await.unwrap();
        {let mut w=db.world.lock();let mut park=Park::new(system,vec![],crate::destiny::tick_now());park.ships.insert(id,ship);w.parks.insert(system,park);w.retained_logouts.insert(id,(pilot,1));}
        Self {db,session,id,pilot,system}
    }
    fn blocked(&self)->bool {
        self.db.store.read(|db|blocked(db,self.id)).unwrap()
    }
}

#[tokio::test]
async fn active_cycle_retains_source_then_saves_last_damage_and_position() {
    let f=Fixture::new().await;
    {let mut w=f.db.world.lock();w.portal_cycles.insert(f.id,crate::world::Bridge {generation:19,started_ms:combat::now_ms(),until_ms:combat::now_ms()+60000,
        titan_pilot:f.pilot,generator_id:123,system:f.system,to_system:30000141,beacon:99,member:None,mass_factor:1.0,harmonics:0});}
    retry_one(&f.db.world,&f.db.store,f.id).await.unwrap();
    assert!(!f.blocked());assert!(f.db.world.lock().parks[&f.system].ships.contains_key(&f.id));
    f.db.world.lock().portal_cycles.get_mut(&f.id).unwrap().until_ms=combat::now_ms()-1;
    {
        let mut w=f.db.world.lock();w.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.id).unwrap().fight.as_mut().unwrap().modules.push(combat::Active {
            module_id:124,type_id:21096,effect_id:2857,guid:String::new(),offensive:false,warp_safe:false,disallow_warp_alignment:true,
            action:combat::Action::Cyno,target:None,ammo:None,duration_ms:60000,cap_need:0.0,first_cycle_prepaid:false,
            started_ms:combat::now_ms(),next_ms:combat::now_ms()+60000,cycles:1,repeat:0,stopping:false,name:"Cyno".into(),reach:None,landed:false,
        });
    }
    retry_one(&f.db.world,&f.db.store,f.id).await.unwrap();
    assert!(f.db.world.lock().parks[&f.system].ships.contains_key(&f.id),"original906 cyno cycle also retains its live source");
    {let mut w=f.db.world.lock();let ship=w.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.id).unwrap();ship.fight.as_mut().unwrap().modules.clear();
        ship.pos=Vec3::new(123.0,456.0,789.0);ship.fight.as_mut().unwrap().vitals.hull_damage=27.0;}
    retry_one(&f.db.world,&f.db.store,f.id).await.unwrap();
    assert!(!f.blocked());assert!(!f.db.world.lock().parks[&f.system].ships.contains_key(&f.id));assert!(!blocked_character_in_memory(&f.db.world,f.pilot));
    let at=f.db.store.read(|db|{let e=db.table("entity").get_id(f.id).unwrap();(e.float("x"),e.float("y"),e.float("z"))});
    assert_eq!(at,(Some(123.0),Some(456.0),Some(789.0)));
    let loaded=crate::svc::inflight::ship_for_space(&f.db.ctx(&f.session),f.pilot,f.id).await.unwrap();assert_eq!(loaded.fight.unwrap().vitals.hull_damage,27.0);
}

#[tokio::test]
async fn reconnect_claims_current_live_hull_without_rewinding_or_late_removal() {
    let f=Fixture::new().await;let mut stale=f.db.world.lock().parks[&f.system].ships[&f.id].clone();stale.client_id=22;
    {let mut w=f.db.world.lock();let live=w.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.id).unwrap();live.pos=Vec3::new(70.0,80.0,90.0);live.fight.as_mut().unwrap().vitals.hull_damage=33.0;
        crate::conn::location::publish_reconnect(&mut w,f.system,stale).unwrap();let current=&w.parks[&f.system].ships[&f.id];
        assert_eq!(current.client_id,22);assert_eq!(current.pos,Vec3::new(70.0,80.0,90.0));assert_eq!(current.fight.as_ref().unwrap().vitals.hull_damage,33.0);assert!(!w.retained_logouts.contains_key(&f.id));}
    retry_one(&f.db.world,&f.db.store,f.id).await.unwrap();
    assert!(f.db.world.lock().parks[&f.system].ships.contains_key(&f.id));assert!(!f.blocked());
}

#[tokio::test]
async fn refused_final_snapshot_holds_the_ship_and_retries_the_same_snapshot() {
    let f=Fixture::new().await;
    // A hull without a usable maximum cannot be checkpointed.
    let hull_max={let mut w=f.db.world.lock();let vitals=&mut w.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.id).unwrap().fight.as_mut().unwrap().vitals;
        let max=vitals.hull_max;vitals.hull_max=0.0;vitals.hull_damage=41.0;max};
    retry_one(&f.db.world,&f.db.store,f.id).await.unwrap();
    assert!(blocked_character_in_memory(&f.db.world,f.pilot));assert!(!f.db.world.lock().parks[&f.system].ships.contains_key(&f.id));
    retry_one(&f.db.world,&f.db.store,f.id).await.unwrap();
    assert!(blocked_character_in_memory(&f.db.world,f.pilot),"the held snapshot is retried, not dropped");
    f.db.world.lock().pending_logouts.get_mut(&f.id).unwrap().ship.fight.as_mut().unwrap().vitals.hull_max=hull_max;
    retry_one(&f.db.world,&f.db.store,f.id).await.unwrap();
    assert!(!blocked_character_in_memory(&f.db.world,f.pilot));assert!(!f.db.world.lock().retained_logouts.contains_key(&f.id));
    let loaded=crate::svc::inflight::ship_for_space(&f.db.ctx(&f.session),f.pilot,f.id).await.unwrap();assert_eq!(loaded.fight.unwrap().vitals.hull_damage,41.0);
}

#[tokio::test]
async fn held_logout_clears_once_its_hull_is_settled_away() {
    let f=Fixture::new().await;
    f.db.world.lock().parks.get_mut(&f.system).unwrap().ships.get_mut(&f.id).unwrap().fight.as_mut().unwrap().vitals.hull_max=0.0;
    retry_one(&f.db.world,&f.db.store,f.id).await.unwrap();
    assert!(blocked_character_in_memory(&f.db.world,f.pilot));
    // Loss settlement replaces the old hull; nothing of it is left to save.
    f.db.store.write(|tx|tx.delete_where("entity",&["itemID"],&[Value::Int(f.id)])).unwrap();
    retry_one(&f.db.world,&f.db.store,f.id).await.unwrap();
    {let w=f.db.world.lock();assert!(!w.pending_logouts.contains_key(&f.id));assert!(!w.retained_logouts.contains_key(&f.id));}
    assert!(!blocked_character_in_memory(&f.db.world,f.pilot));
}

#[tokio::test]
async fn actual_connection_logout_keeps_portal_source_live_without_a_client() {
    let f=Fixture::new().await;
    let (id,pilot,system)=(f.id,f.pilot,f.system);
    f.db.world.lock().retained_logouts.clear();
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client,accepted)=tokio::join!(tokio::net::TcpStream::connect(listener.local_addr().unwrap()),listener.accept());
    let (socket,peer)=accepted.unwrap();let client=client.unwrap();
    let server=std::sync::Arc::new(Server {table:Default::default(),store:f.db.store,node_id:1,
        bulk:f.db.bulk,dogma:f.db.dogma,services:crate::svc::registry(),world:f.db.world,dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0});
    let mut conn=crate::conn::Conn::new(socket,peer,server,1);conn.session=f.session;
    let client_id=conn.client_id;
    let _receiver={let mut w=conn.server.world.lock();let rx=w.connect(client_id);
        w.set_presence(client_id,Some(crate::world::Presence{char_id:pilot,corp_id:1000002,solar_system_id:system,docked:false,station_id:None,role:0}));
        w.parks.get_mut(&system).unwrap().ships.get_mut(&id).unwrap().client_id=client_id;
        w.portal_cycles.insert(id,crate::world::Bridge {generation:20,started_ms:combat::now_ms(),until_ms:combat::now_ms()+60000,
            titan_pilot:pilot,generator_id:123,system,to_system:30000141,beacon:99,member:None,mass_factor:1.0,harmonics:0});rx};
    drop(client);conn.leave_world().await;
    {let w=conn.server.world.lock();assert!(w.clients_of(pilot).is_empty());assert!(w.parks[&system].ships.contains_key(&id));assert_eq!(w.retained_logouts.get(&id),Some(&(pilot,client_id)));}
    let store=conn.server.store.clone();
    retry_one(&conn.server.world,&store,id).await.unwrap();assert!(!store.read(|db|blocked(db,id)).unwrap());
    conn.server.world.lock().portal_cycles.get_mut(&id).unwrap().until_ms=combat::now_ms()-1;
    retry_one(&conn.server.world,&store,id).await.unwrap();
    assert!(!conn.server.world.lock().parks[&system].ships.contains_key(&id));assert!(!store.read(|db|blocked(db,id)).unwrap());
}
