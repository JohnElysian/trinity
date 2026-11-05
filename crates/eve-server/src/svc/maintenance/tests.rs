use super::*;
use crate::{test_support::Database,session::Session,ballpark::Park,store::{Sets,StoreError}};
#[path="inventory_tests.rs"] mod inventory_cases;

struct Fixture {db:Database,actor:Session,host:Session,host_id:i64,ship:i64,system:i64}

fn entity(id:i64,ty:i64,owner:i64,loc:i64,flag:i64,quantity:i64,singleton:i64)->Vec<(&'static str,store::Value)> {
    vec![("itemID",store::Value::Int(id)),("typeID",store::Value::Int(ty)),("ownerID",store::Value::Int(owner)),("locationID",store::Value::Int(loc)),
        ("flag",store::Value::Int(flag)),("quantity",store::Value::Int(quantity)),("singleton",store::Value::Int(singleton))]
}

impl Fixture {
    async fn new()->Self {
        let db=Database::load().await;
        let mut ids=Vec::new();for _ in 0..5 {ids.push(i64::from(db.id().await));}
        let [a,b,ship,host,corp]=ids[..] else {unreachable!()};
        let system=30000141i64;
        let skills=db.store.read(|d|super::super::fitting::required_skill_levels(d,588)).unwrap();
        let mut skill_items=Vec::new();for _ in 0..skills.len() {skill_items.push(i64::from(db.id().await));}
        db.store.write(|tx|->Result<(),StoreError> {
            tx.insert("corporation",&[("corporationID",store::Value::Int(corp)),("corporationName",store::Value::Text(format!("Maintenance {corp}").into()))])?;
            for (who,hull,ty) in [(a,ship,588),(b,host,23911)] {
                tx.insert("character_",&[("characterID",store::Value::Int(who)),("characterName",store::Value::Text(format!("Bay pilot {who}").into())),("shipID",store::Value::Int(hull)),
                    ("corporationID",store::Value::Int(corp)),("solarSystemID",store::Value::Int(system)),("stationID",store::Value::Int(0))])?;
                tx.insert("entity",&entity(hull,ty,who,system,0,1,1))?;
            }
            for ((skill,level),id) in skills.iter().zip(&skill_items) {
                tx.insert("entity",&entity(*id,i64::from(*skill),a,a,7,1,1))?;
                tx.insert("entity_attributes",&[("itemID",store::Value::Int(*id)),("attributeID",store::Value::Int(280)),("valueInt",store::Value::Int(*level))])?;
            }
            Ok(())
        }).unwrap();
        let session=|who:i64,hull:i64|Session{char_id:Some(who),ship_id:Some(hull),corp_id:Some(corp),solar_system_id:Some(system),solar_system_id2:Some(system),..Default::default()};
        let actor=session(a,ship);let host_session=session(b,host);
        let mut actor_ship=inflight::ship_for_space(&db.ctx(&actor),a,ship).await.unwrap();
        let mut ctx=db.ctx(&host_session);ctx.client_id=2;
        let host_ship=inflight::ship_for_space(&ctx,b,host).await.unwrap();
        actor_ship.arrive(Vec3::new(host_ship.stats.radius+actor_ship.stats.radius+100.0,0.0,0.0));
        let mut park=Park::new(system,vec![],crate::destiny::tick_now());park.ships.insert(ship,actor_ship);park.ships.insert(host,host_ship);
        db.world.lock().parks.insert(system,park);
        Self{db,actor,host:host_session,host_id:host,ship,system}
    }
    fn actor_ctx(&self)->Ctx<'_>{self.db.ctx(&self.actor)}
    fn host_ctx(&self)->Ctx<'_>{let mut c=self.db.ctx(&self.host);c.client_id=2;c}
    async fn item(&self,ty:i64,owner:i64,loc:i64,flag:i64,quantity:i64,singleton:i64)->i64 {
        let id=i64::from(self.db.id().await);
        self.db.store.write(|tx|tx.insert("entity",&entity(id,ty,owner,loc,flag,quantity,singleton))).unwrap();id
    }
    async fn location(&self,id:i64)->(i64,i64,i64){
        self.db.store.read(|d|{let r=d.table("entity").get_id(id).unwrap();(r.int("locationID").unwrap(),r.int("flag").unwrap(),r.int("ownerID").unwrap())})
    }
    /// Set columns of the row whose single-column key is `id` (nothing if
    /// there is none, as an UPDATE matching no row).
    fn set(&self,table:&str,id:i64,sets:Sets){
        self.db.store.write(|tx|match tx.table(table).get_id(id).map(|r|r.id){Some(row)=>tx.update(table,row,sets),None=>Ok(())}).unwrap();
    }
    /// The station with the lowest ID.
    fn station(&self)->i64{self.db.store.read(|d|d.table("staStations").rows().filter_map(|r|r.int("stationID")).min()).unwrap()}
    /// The ship's stored structure damage.
    fn damage(&self,id:i64)->Option<f64>{
        self.db.store.read(|d|d.table("entity_attributes").get(&[store::Value::Int(id),store::Value::Int(3)]).and_then(|r|r.float("valueFloat").or(r.float("valueInt"))))
    }
}

fn listed(v:Value)->Vec<i64>{match v{Value::Instance{state,..}=>state.get_str("lines").and_then(Value::seq).unwrap_or(&[]).iter().filter_map(|r|r.seq()?.first()?.as_i64()).collect(),_=>vec![]}}
fn cells(v:&Value)->&[Value]{match v{Value::Instance{state,..}=>state.get_str("line").and_then(Value::seq).unwrap_or(&[]),_=>&[]}}
fn success<T>(r:Result<T,MoveError>)->T{match r{Ok(v)=>v,Err(e)=>panic!("{}",embark::error_text(e))}}
async fn inventory_call(ctx:&Ctx<'_>,host:i64,method:&str,args:Vec<Value>)->Outcome{
    super::super::moves::container_write(ctx,super::super::inventory::Container::Item(host),&Call{service:"invbroker".into(),method:method.into(),args,kwargs:vec![]}).await.unwrap()
}
fn moved(out:Outcome)->bool{matches!(out,Outcome::Value(Value::None))}

#[test]
fn packaged_volume_matches_original_client_not_assembled_hull_volume(){
    assert_eq!(unit_volume(588,237,false,15800.0),2500.0);
    assert_eq!(unit_volume(588,237,true,15800.0),15800.0);
    assert_eq!(unit_volume(11019,28,false,1e8),1e8);
}

#[tokio::test]
async fn maintenance_store_capsule_board_preserves_custody_damage_and_private_inventory(){
    let mut f=Fixture::new().await;let who=f.actor.char_id.unwrap();
    let secret=f.item(34,f.host.char_id.unwrap(),f.host_id,5,10,0).await;
    let cargo=f.item(34,who,f.ship,5,1,0).await;
    assert!(store(&f.actor_ctx(),f.host_id).await.is_err(),"non-charge cargo prevents storage");
    assert_eq!(f.location(f.ship).await.0,f.system);
    f.db.store.write(|tx|{let row=tx.table("entity").get_id(cargo).unwrap().id;tx.delete("entity",row)}).unwrap();
    {let mut w=f.db.world.lock();let fight=w.parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap();fight.vitals.armor_damage=7.0;fight.vitals.hull_damage=3.0;}
    let capsule=success(store(&f.actor_ctx(),f.host_id).await);
    assert_eq!(f.location(f.ship).await,(f.host_id,90,who));
    assert!(!f.db.world.lock().parks[&f.system].ships.contains_key(&f.ship));
    f.actor.ship_id=Some(capsule);
    let ids=listed(super::super::inventory::list(&f.actor_ctx(),super::super::inventory::Container::Item(f.host_id)).await);
    assert_eq!(ids,vec![f.ship]);assert!(!ids.contains(&secret),"bay access never exposes host cargo");
    let next=success(board_stored(&f.actor_ctx(),f.host_id,f.ship).await);
    assert_eq!(next,f.ship);assert_eq!(f.location(next).await,(f.system,0,who));
    assert!(f.db.store.read(|d|d.table("entity").get_id(capsule).is_none()));
    let w=f.db.world.lock();let s=&w.parks[&f.system].ships[&next];assert!(s.is_pilot());assert_eq!(s.fight.as_ref().unwrap().vitals.armor_damage,7.0);assert_eq!(s.fight.as_ref().unwrap().vitals.hull_damage,3.0);
    assert!(w.parks[&f.system].ships.values().all(|s|!s.identity_reserved));
}

#[tokio::test]
async fn maintenance_launch_stack_scoop_and_transient_configuration(){
    let mut f=Fixture::new().await;let owner=f.host.char_id.unwrap();
    assert_eq!(success(configuration(&f.host_ctx(),None).await).get_str("allowGangSMBUsage"),Some(&Value::Bool(false)));
    success(configuration(&f.host_ctx(),Some(true)).await);
    assert_eq!(success(configuration(&f.host_ctx(),None).await).get_str("allowGangSMBUsage"),Some(&Value::Bool(true)));
    f.actor.corp_id=None;
    f.set("character_",f.actor.char_id.unwrap(),&[("corporationID",store::Value::Int(0))]);
    assert!(view(&f.actor_ctx(),f.host_id).await.is_none(),"fleet opt-in alone is not membership");
    f.db.world.lock().gangs.test_members(vec![owner,f.actor.char_id.unwrap()]);
    assert!(view(&f.actor_ctx(),f.host_id).await.is_some());
    f.db.world.lock().gangs=Default::default();
    assert!(view(&f.actor_ctx(),f.host_id).await.is_none(),"leaving fleet revokes access immediately");
    let packed=f.item(588,owner,f.host_id,90,2,0).await;
    success(operate(&f.host_ctx(),f.host_id,Operation::Launch(vec![packed,packed])).await);
    let launched:Vec<_>=f.db.world.lock().parks[&f.system].ships.values().filter(|s|s.unpiloted&&s.pilot.char_id==owner).map(|s|s.id).collect();
    assert_eq!(launched.len(),2);
    for id in &launched{assert_eq!(f.location(*id).await,(f.system,0,owner));}
    success(operate(&f.host_ctx(),f.host_id,Operation::Scoop(packed)).await);
    assert_eq!(f.location(packed).await,(f.host_id,90,owner));
    assert!(!f.db.world.lock().parks[&f.system].ships.contains_key(&packed));
    let no_access=f.item(588,f.actor.char_id.unwrap(),f.system,0,1,1).await;
    assert!(operate(&f.host_ctx(),f.host_id,Operation::Scoop(no_access)).await.is_err());
    let mut w=f.db.world.lock();let hull=space::remove_ship(&mut w,f.system,f.host_id).unwrap();assert!(!w.maintenance.contains_key(&f.host_id));space::add_ship(&mut w,f.system,hull);
}

#[tokio::test]
async fn maintenance_station_deposits_use_original_volume_and_reject_full_or_wrong_items(){
    let mut f=Fixture::new().await;let owner=f.host.char_id.unwrap();
    let station=f.station();
    f.set("character_",owner,&[("stationID",store::Value::Int(station))]);
    f.set("entity",f.host_id,&[("locationID",store::Value::Int(station)),("flag",store::Value::Int(4))]);
    f.host.station_id=Some(station);f.host.solar_system_id=None;
    let packed=f.item(588,owner,station,4,3,0).await;
    assert!(moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(packed),Value::None,Value::Int(90)]).await));
    assert_eq!(f.location(packed).await,(f.host_id,90,owner));
    assert_eq!(success(f.db.store.read(|d|used(d,f.host_id,90,None))),7500.0);
    let capacity=super::super::inventory::get_capacity(&f.host_ctx(),super::super::inventory::Container::Item(f.host_id),Some(90)).await;
    assert_eq!(cells(&capacity).get(1),Some(&Value::Double(7500.0)));
    let notship=f.item(34,owner,station,4,1,0).await;
    assert!(!moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(notship),Value::None,Value::Int(90)]).await));
    let huge=f.item(23911,owner,station,4,1,1).await;
    assert!(!moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(huge),Value::None,Value::Int(90)]).await));
    assert_eq!(f.location(huge).await.0,station);
}

#[tokio::test]
async fn maintenance_array_requires_live_state_permissions_and_physical_user_admission(){
    let mut f=Fixture::new().await;
    let array=f.item(12237,f.actor.corp_id.unwrap(),f.system,0,1,1).await;
    f.db.store.write(|tx|tx.insert("posStructures",&[("itemID",store::Value::Int(array)),("state",store::Value::Int(1)),("useViewPut",store::Value::Int(0)),("useViewPutTake",store::Value::Int(0)),("useUse",store::Value::Int(0))])).unwrap();
    let structure=f.db.store.read(|d|crate::pos::load(d,f.system)).unwrap().into_iter().find(|s|s.id==array).unwrap();
    f.db.world.lock().parks.get_mut(&f.system).unwrap().structures.insert(array,structure);
    assert!(store(&f.actor_ctx(),array).await.is_err(),"default POS permissions require a role");
    f.set("posStructures",array,&[("useViewPut",store::Value::Int(1)),("useViewPutTake",store::Value::Int(1))]);
    let crowd:Vec<_>=(0..9).map(|n|9_800_000_000+n).collect();
    {let mut w=f.db.world.lock();let park=w.parks.get_mut(&f.system).unwrap();let original=park.ships[&f.ship].clone();
     for id in &crowd{let mut s=original.clone();s.id=*id;s.pilot.char_id=*id;park.ships.insert(*id,s);}}
    assert!(store(&f.actor_ctx(),array).await.is_err(),"nearby piloted hulls count even without inventory windows");
    {let mut w=f.db.world.lock();let park=w.parks.get_mut(&f.system).unwrap();for id in crowd{park.ships.remove(&id);}park.structures.get_mut(&array).unwrap().state=crate::pos::state::UNANCHORED;}
    assert!(store(&f.actor_ctx(),array).await.is_err(),"unanchored arrays cannot operate");
    f.db.world.lock().parks.get_mut(&f.system).unwrap().structures.get_mut(&array).unwrap().state=crate::pos::state::ANCHORED;
    let capsule=success(store(&f.actor_ctx(),array).await);f.actor.ship_id=Some(capsule);
    assert_eq!(f.location(f.ship).await,(array,0,f.actor.char_id.unwrap()));
    assert_eq!(success(board_stored(&f.actor_ctx(),array,f.ship).await),f.ship);
    assert_eq!(f.location(f.ship).await.0,f.system);
}

/// A host that moved out of range before the store (once while the store
/// waited for a row lock) refuses it, leaving custody, live damage and
/// reservations as they were.
#[tokio::test]
async fn maintenance_rechecks_moving_host_without_losing_live_damage_or_custody(){
    let f=Fixture::new().await;
    let charge=i64::from(crate::sde::get().types_in_category(8)[0]);
    let cargo=f.item(charge,f.actor.char_id.unwrap(),f.ship,5,1,0).await;
    {let mut w=f.db.world.lock();let park=w.parks.get_mut(&f.system).unwrap();
     park.ships.get_mut(&f.host_id).unwrap().pos=Vec3::new(1e9,0.0,0.0);park.ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.hull_damage=7.0;}
    let result=store(&f.actor_ctx(),f.host_id).await;
    assert!(result.is_err());assert_eq!(f.location(f.ship).await.0,f.system);assert_eq!(f.location(cargo).await.0,f.ship);
    let w=f.db.world.lock();let ship=&w.parks[&f.system].ships[&f.ship];assert_eq!(ship.fight.as_ref().unwrap().vitals.hull_damage,7.0);assert!(!ship.identity_reserved);assert!(!w.parks[&f.system].ships[&f.host_id].identity_reserved);
}

/// The final flush after the custody commit writes the hull's live damage.
#[tokio::test]
async fn maintenance_final_flush_preserves_live_damage(){
    let f=Fixture::new().await;
    f.db.world.lock().parks.get_mut(&f.system).unwrap().ships.get_mut(&f.ship).unwrap().fight.as_mut().unwrap().vitals.hull_damage=11.0;
    success(store(&f.actor_ctx(),f.host_id).await);
    assert_eq!(f.damage(f.ship),Some(11.0));assert!(f.db.world.lock().maintenance_repairs.is_empty());
}

#[tokio::test]
async fn maintenance_pending_final_flush_blocks_withdrawal_and_stale_generation_cannot_overwrite(){
    let mut f=Fixture::new().await;
    let mut fighter=f.db.world.lock().parks[&f.system].ships[&f.ship].fight.clone().unwrap();fighter.vitals.hull_damage=13.0;
    let capsule=success(store(&f.actor_ctx(),f.host_id).await);f.actor.ship_id=Some(capsule);
    let generation=f.db.store.read(|d|d.table("maintenance_custody").get_id(f.ship).and_then(|r|r.int("generation"))).unwrap();
    let pending=Repair{id:f.ship,host:f.host_id,owner:f.actor.char_id.unwrap(),flag:90,generation,at_ms:crate::combat::now_ms(),fighter};
    // A final snapshot not yet saved holds the ship in the bay.
    f.db.world.lock().maintenance_repairs.insert(f.ship,pending.clone());
    assert!(board_stored(&f.actor_ctx(),f.host_id,f.ship).await.is_err());
    retry_repairs(&f.db.world,&f.db.store).await;
    assert!(f.db.world.lock().maintenance_repairs.is_empty());
    // A later transfer generation must make every delayed old repair inert,
    // even if the ship happens to return to the same owner, host and bay.
    f.set("entity",f.ship,&[("flag",store::Value::Int(5))]);
    f.set("entity",f.ship,&[("flag",store::Value::Int(90))]);
    let later=success(f.db.store.write(|tx|repair::begin(tx,f.ship,f.host_id,f.actor.char_id.unwrap(),90)));assert_ne!(later,generation);
    f.db.store.write(|tx|{let row=tx.table("entity_attributes").get(&[store::Value::Int(f.ship),store::Value::Int(3)]).unwrap().id;
        tx.update("entity_attributes",row,&[("valueInt",store::Value::Null),("valueFloat",store::Value::Float(2.0))])}).unwrap();
    repair::attempt(&f.db.world,&f.db.store,pending).await;
    assert_eq!(f.db.store.read(|d|d.table("entity_attributes").get(&[store::Value::Int(f.ship),store::Value::Int(3)]).and_then(|r|r.float("valueFloat"))),Some(2.0));
}
