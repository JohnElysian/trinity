use super::*;
use crate::svc::inventory::Container;

async fn dock_host(f:&mut Fixture)->i64{
    let station:i32=f.db.db.query_one(r#"SELECT "stationID" FROM "staStations" ORDER BY "stationID" LIMIT 1"#,&[]).await.unwrap().get(0);
    f.db.db.execute(r#"UPDATE "character_" SET "stationID"=$2 WHERE "characterID"=$1"#,&[&(f.host.char_id.unwrap() as i32),&station]).await.unwrap();
    f.db.db.execute(r#"UPDATE entity SET "locationID"=$2,flag=4 WHERE "itemID"=$1"#,&[&(f.host_id as i32),&station]).await.unwrap();
    f.host.station_id=Some(station as i64);f.host.solar_system_id=None;
    f.db.world.lock().parks.get_mut(&f.system).unwrap().ships.remove(&f.host_id);station as i64
}
async fn qty(f:&Fixture,id:i64)->Option<i32>{f.db.db.query_opt(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#,&[&(id as i32)]).await.unwrap().map(|r|r.get(0))}
async fn array(f:&Fixture)->i64{
    let id=f.item(12237,f.host.corp_id.unwrap(),f.system,0,1,1).await;
    f.db.db.execute(r#"INSERT INTO "posStructures"("itemID",state,"useViewPut","useViewPutTake") VALUES($1,1,1,1)"#,&[&(id as i32)]).await.unwrap();
    let s=crate::pos::load(&f.db.db,f.system).await.unwrap().into_iter().find(|s|s.id==id).unwrap();
    f.db.world.lock().parks.get_mut(&f.system).unwrap().structures.insert(id,s);id
}
async fn take_to_hangar(f:&Fixture,id:i64)->Outcome{
    crate::svc::moves::container_write(&f.host_ctx(),Container::Special{container_id:crate::svc::inventory::CONTAINER_HANGAR,owner_id:None},
        &Call{service:"invbroker".into(),method:"Add".into(),args:vec![Value::Int(id)],kwargs:vec![]}).await.unwrap()
}
fn merges(source:i64,target:i64,n:i64)->Vec<Value>{vec![Value::List(vec![Value::Tuple(vec![Value::Int(source),Value::Int(target),Value::Int(n)])])]}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_original_stack_merge_split_calls_keep_quantities_and_owner(){
    let mut f=Fixture::new().await;let station=dock_host(&mut f).await;let owner=f.host.char_id.unwrap();
    let a=f.item(588,owner,station,4,4,0).await;let b=f.item(588,owner,station,4,3,0).await;
    assert!(moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(a),Value::Int(2),Value::Int(90)]).await));
    let split:i32=f.db.db.query_one(r#"SELECT "itemID" FROM entity WHERE "locationID"=$1 AND flag=90"#,&[&(f.host_id as i32)]).await.unwrap().get(0);
    assert_eq!(qty(&f,a).await,Some(2));
    assert!(moved(inventory_call(&f.host_ctx(),f.host_id,"MultiAdd",vec![Value::List(vec![Value::Int(a),Value::Int(b)]),Value::None,Value::Int(90)]).await));
    assert!(moved(inventory_call(&f.host_ctx(),f.host_id,"StackAll",vec![Value::Int(90)]).await));
    assert_eq!(qty(&f,a).await,Some(7));assert_eq!(qty(&f,b).await,None);assert_eq!(qty(&f,split as i64).await,None);
    assert!(moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(a),Value::Int(2),Value::Int(90)]).await),"same-bay Add splits a stack");
    let split:i32=f.db.db.query_one(r#"SELECT "itemID" FROM entity WHERE "locationID"=$1 AND flag=90 AND "itemID"<>$2"#,&[&(f.host_id as i32),&(a as i32)]).await.unwrap().get(0);
    assert!(moved(inventory_call(&f.host_ctx(),f.host_id,"MergeItems",vec![Value::Int(split as i64),Value::Int(a),Value::Int(1)]).await));
    assert!(moved(inventory_call(&f.host_ctx(),f.host_id,"MultiMerge",merges(split as i64,a,1)).await));
    assert_eq!(qty(&f,a).await,Some(7));assert_eq!(qty(&f,split as i64).await,None);
    for n in [-1,0,8]{assert!(!moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(a),Value::Int(n),Value::Int(90)]).await));}
    let other=f.item(588,f.actor.char_id.unwrap(),f.host_id,90,2,0).await;
    assert!(!moved(inventory_call(&f.host_ctx(),f.host_id,"MergeItems",vec![Value::Int(other),Value::Int(a),Value::Int(1)]).await));
    let assembled=f.item(588,owner,f.host_id,90,1,1).await;
    assert!(!moved(inventory_call(&f.host_ctx(),f.host_id,"MergeItems",vec![Value::Int(assembled),Value::Int(a)]).await));
    assert_eq!(qty(&f,a).await,Some(7));assert_eq!(qty(&f,other).await,Some(2));assert_eq!(f.location(a).await,(f.host_id,90,owner));
    assert!(!moved(inventory_call(&f.host_ctx(),f.host_id,"MultiAdd",vec![Value::List(vec![Value::Int(a),Value::bytes("invalid")]),Value::None,Value::Int(90)]).await));
    assert!(!moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(a+(1i64<<32)),Value::None,Value::Int(90)]).await),"wide IDs cannot wrap to an existing item");
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_pos_original_omitted_flags_preserve_property_and_reject_noncanonical_or_wrong_items(){
    let f=Fixture::new().await;let bay=array(&f).await;let owner=f.host.char_id.unwrap();
    let a=f.item(588,owner,f.host_id,90,2,0).await;let b=f.item(588,owner,f.host_id,90,3,0).await;
    let assembled=f.item(588,owner,f.host_id,90,1,1).await;
    assert!(!moved(inventory_call(&f.host_ctx(),bay,"Add",vec![Value::Int(a),Value::None,Value::Int(90)]).await));
    assert_eq!(f.location(a).await,(f.host_id,90,owner));
    assert!(moved(inventory_call(&f.host_ctx(),bay,"Add",vec![Value::Int(a)]).await));
    assert!(moved(inventory_call(&f.host_ctx(),bay,"MultiAdd",vec![Value::List(vec![Value::Int(b),Value::Int(assembled)])]).await));
    assert_eq!(f.location(assembled).await,(bay,0,owner));
    assert!(moved(inventory_call(&f.host_ctx(),bay,"StackAll",vec![]).await));
    assert_eq!(qty(&f,a).await,Some(5));assert_eq!(qty(&f,b).await,None);
    let mineral=f.item(34,owner,f.host_id,5,1,0).await;
    assert!(!moved(inventory_call(&f.host_ctx(),bay,"Add",vec![Value::Int(mineral)]).await));
    assert!(!moved(inventory_call(&f.host_ctx(),bay,"Add",vec![Value::Int(f.host_id)]).await));
    let corp=f.host.corp_id.unwrap();let c=f.item(588,corp,bay,0,2,0).await;let d=f.item(588,corp,bay,0,3,0).await;
    assert!(moved(inventory_call(&f.host_ctx(),bay,"StackAll",vec![]).await));
    assert_eq!(qty(&f,c).await,Some(5));assert_eq!(qty(&f,d).await,None);assert_eq!(qty(&f,a).await,Some(5));
    assert!(!moved(inventory_call(&f.host_ctx(),bay,"MultiMerge",merges(c,a,1)).await));
    f.db.world.lock().parks.get_mut(&f.system).unwrap().structures.get_mut(&bay).unwrap().state=crate::pos::state::UNANCHORED;
    assert!(!moved(inventory_call(&f.host_ctx(),bay,"StackAll",vec![]).await));
    assert!(listed(crate::svc::inventory::list(&f.host_ctx(),Container::Item(bay)).await).is_empty());
    assert_eq!(qty(&f,c).await,Some(5));
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_pending_repair_guards_generic_withdrawal_and_nested_mutation_until_saved(){
    let mut f=Fixture::new().await;let station=dock_host(&mut f).await;let owner=f.host.char_id.unwrap();
    let hull=f.item(588,owner,f.host_id,90,1,1).await;let module=f.item(34,owner,hull,5,2,0).await;
    let mut fighter=f.db.world.lock().parks[&f.system].ships[&f.ship].fight.clone().unwrap();fighter.vitals.hull_damage=13.0;
    let generation=success(repair::begin(&f.db.db,hull,f.host_id,owner,90).await);
    f.db.world.lock().maintenance_repairs.insert(hull,Repair{id:hull,host:f.host_id,owner,flag:90,generation,at_ms:crate::combat::now_ms(),fighter});
    assert!(!moved(take_to_hangar(&f,hull).await));
    assert!(!moved(take_to_hangar(&f,module).await));
    assert!(!moved(inventory_call(&f.host_ctx(),hull,"StackAll",vec![]).await));
    let target=f.item(34,owner,hull,5,1,0).await;
    assert!(!moved(inventory_call(&f.host_ctx(),hull,"MergeItems",vec![Value::Int(module),Value::Int(target)]).await));
    assert_eq!(f.location(hull).await,(f.host_id,90,owner));assert_eq!(qty(&f,module).await,Some(2));
    retry_repairs(&f.db.world,&f.db.url).await;
    assert!(f.db.world.lock().maintenance_repairs.is_empty());
    assert!(moved(take_to_hangar(&f,hull).await));assert_eq!(f.location(hull).await,(station,4,owner));
    let damage:f64=f.db.db.query_one(r#"SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(hull as i32)]).await.unwrap().get(0);assert_eq!(damage,13.0);
    // The independent station-arrival journal guards the same generic paths,
    // including nested cargo, even after the ship is already in the hangar.
    f.db.db.execute(r#"INSERT INTO "pendingStationArrivals"("shipID","characterID","stationID","sourceSystemID","shipTypeID","createdAt") VALUES($1,$2,$3,$4,588,0)"#,
        &[&(hull as i32),&(owner as i32),&(station as i32),&(f.system as i32)]).await.unwrap();
    assert!(!moved(take_to_hangar(&f,module).await));
    assert!(!moved(inventory_call(&f.host_ctx(),hull,"StackAll",vec![]).await));
    f.db.db.execute(r#"DELETE FROM "pendingStationArrivals" WHERE "shipID"=$1"#,&[&(hull as i32)]).await.unwrap();
    assert!(moved(take_to_hangar(&f,module).await));
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_unflagged_stack_all_cannot_bypass_admission_or_change_other_owner(){
    let f=Fixture::new().await;let owner=f.host.char_id.unwrap();
    let a=f.item(588,owner,f.host_id,90,2,0).await;let b=f.item(588,owner,f.host_id,90,3,0).await;
    let foreign=f.item(588,f.actor.char_id.unwrap(),f.host_id,90,5,0).await;
    let ammo=f.item(178,owner,f.host_id,5,2,0).await;let ammo2=f.item(178,owner,f.host_id,5,3,0).await;
    let crowd:Vec<_>=(0..9).map(|n|9_810_000_000+n).collect();
    {let mut w=f.db.world.lock();let p=w.parks.get_mut(&f.system).unwrap();let s=p.ships[&f.ship].clone();for id in &crowd{let mut s=s.clone();s.id=*id;s.pilot.char_id=*id;p.ships.insert(*id,s);}}
    assert!(!moved(inventory_call(&f.host_ctx(),f.host_id,"StackAll",vec![]).await));
    assert_eq!(qty(&f,a).await,Some(2));assert_eq!(qty(&f,ammo).await,Some(2));assert_eq!(qty(&f,ammo2).await,Some(3));
    {let mut w=f.db.world.lock();let p=w.parks.get_mut(&f.system).unwrap();for id in crowd{p.ships.remove(&id);}}
    assert!(moved(inventory_call(&f.host_ctx(),f.host_id,"StackAll",vec![]).await));
    assert_eq!(qty(&f,a).await,Some(5));assert_eq!(qty(&f,b).await,None);assert_eq!(qty(&f,foreign).await,Some(5));
    assert_eq!(qty(&f,ammo).await,Some(5));assert_eq!(qty(&f,ammo2).await,None);
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_inventory_revalidates_host_after_wait_and_concurrent_deposits_respect_capacity(){
    let mut f=Fixture::new().await;let owner=f.host.char_id.unwrap();
    let packed=f.item(588,f.actor.char_id.unwrap(),f.ship,5,1,0).await;
    let blocker=crate::db::connect(&f.db.url).await.unwrap();let pid:i32=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    blocker.batch_execute("BEGIN").await.unwrap();blocker.query_one(r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(packed as i32)]).await.unwrap();
    let ctx=f.actor_ctx();
    let (out,())=tokio::join!(inventory_call(&ctx,f.host_id,"Add",vec![Value::Int(packed),Value::None,Value::Int(90)]),async{
        wait_blocked(&blocker,pid).await;
        {let mut w=f.db.world.lock();let p=w.parks.get_mut(&f.system).unwrap();assert!(p.ships[&f.host_id].identity_reserved);p.ships.get_mut(&f.host_id).unwrap().pos=Vec3::new(1e9,0.0,0.0);}
        blocker.batch_execute("ROLLBACK").await.unwrap();
    });
    assert!(!moved(out));assert_eq!(f.location(packed).await.0,f.ship);assert!(!f.db.world.lock().parks[&f.system].ships[&f.host_id].identity_reserved);
    let station=dock_host(&mut f).await;
    let a=f.item(588,owner,station,4,201,0).await;let b=f.item(588,owner,station,4,201,0).await;
    let ctx=f.host_ctx();let (one,two)=tokio::join!(inventory_call(&ctx,f.host_id,"Add",vec![Value::Int(a),Value::None,Value::Int(90)]),inventory_call(&ctx,f.host_id,"Add",vec![Value::Int(b),Value::None,Value::Int(90)]));
    assert_eq!(usize::from(moved(one))+usize::from(moved(two)),1);
    assert_eq!(success(used(&f.db.db,f.host_id,90,None).await),502500.0);
    assert_eq!(qty(&f,a).await,Some(201));assert_eq!(qty(&f,b).await,Some(201));
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_cargo_extraction_uses_packaged_volume_and_final_fleet_authority(){
    let mut f=Fixture::new().await;let owner=f.host.char_id.unwrap();
    let a=f.item(588,owner,f.host_id,90,5,0).await;
    // Thanatos cargo is smaller than a Reaper's assembled volume but holds its
    // 2500m3 packaged hull. Subsequent withdrawals must count existing packages.
    assert!(moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(a),Value::Int(1),Value::Int(5)]).await));
    let cap=crate::svc::inventory::get_capacity(&f.host_ctx(),Container::Item(f.host_id),Some(5)).await;
    assert_eq!(cells(&cap).get(1),Some(&Value::Double(2500.0)));
    let capacity=cells(&cap)[0].clone();let Value::Double(capacity)=capacity else{panic!("capacity")};
    let remaining=((capacity-2500.0)/2500.0).floor() as i64;
    if remaining>0 && remaining<4{assert!(moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(a),Value::Int(remaining),Value::Int(5)]).await));}
    assert!(!moved(inventory_call(&f.host_ctx(),f.host_id,"Add",vec![Value::Int(a),Value::Int(1),Value::Int(5)]).await));

    f.actor.corp_id=None;
    f.db.db.execute(r#"UPDATE "character_" SET "corporationID"=0 WHERE "characterID"=$1"#,&[&(f.actor.char_id.unwrap() as i32)]).await.unwrap();
    success(configuration(&f.host_ctx(),Some(true)).await);
    f.db.world.lock().gangs.test_members(vec![owner,f.actor.char_id.unwrap()]);
    let packed=f.item(588,f.actor.char_id.unwrap(),f.ship,5,1,0).await;
    let blocker=crate::db::connect(&f.db.url).await.unwrap();let pid:i32=blocker.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
    blocker.batch_execute("BEGIN").await.unwrap();blocker.query_one(r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(packed as i32)]).await.unwrap();
    let ctx=f.actor_ctx();let (out,())=tokio::join!(inventory_call(&ctx,f.host_id,"Add",vec![Value::Int(packed),Value::None,Value::Int(90)]),async{
        wait_blocked(&blocker,pid).await;f.db.world.lock().gangs=Default::default();blocker.batch_execute("ROLLBACK").await.unwrap();
    });
    assert!(!moved(out));assert_eq!(f.location(packed).await.0,f.ship);
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn maintenance_pending_descendant_blocks_parent_corporation_handover_until_final_save(){
    let mut f=Fixture::new().await;let station=dock_host(&mut f).await;
    let owner=f.host.char_id.unwrap();let corp=f.host.corp_id.unwrap();
    let carrier=f.item(23911,owner,station,4,1,1).await;
    let hull=f.item(588,owner,carrier,90,1,1).await;
    let office=f.item(27,corp,station,0,1,1).await;
    f.db.db.execute(r#"INSERT INTO "crpOffices"("corporationID","stationID","officeFolderID","itemID") VALUES($1,$2,$3,$3)"#,&[&(corp as i32),&(station as i32),&(office as i32)]).await.unwrap();
    let mut fighter=f.db.world.lock().parks[&f.system].ships[&f.ship].fight.clone().unwrap();fighter.vitals.hull_damage=17.0;
    let generation=success(repair::begin(&f.db.db,hull,carrier,owner,90).await);
    f.db.world.lock().maintenance_repairs.insert(hull,Repair{id:hull,host:carrier,owner,flag:90,generation,at_ms:crate::combat::now_ms(),fighter});
    assert!(!moved(inventory_call(&f.host_ctx(),office,"Add",vec![Value::Int(carrier),Value::None,Value::Int(4)]).await));
    assert_eq!(f.location(carrier).await,(station,4,owner));assert_eq!(f.location(hull).await,(carrier,90,owner));
    assert_eq!(f.db.db.query_one("SELECT generation FROM maintenance_custody WHERE item_id=$1",&[&(hull as i32)]).await.unwrap().get::<_,i64>(0),generation);
    retry_repairs(&f.db.world,&f.db.url).await;
    assert!(moved(inventory_call(&f.host_ctx(),office,"Add",vec![Value::Int(carrier),Value::None,Value::Int(4)]).await));
    assert_eq!(f.location(carrier).await,(office,4,corp));assert_eq!(f.location(hull).await,(carrier,90,corp));
    let damage:f64=f.db.db.query_one(r#"SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(hull as i32)]).await.unwrap().get(0);assert_eq!(damage,17.0);
}
