use super::*;

async fn arrive(f:&Fixture,db:&mut Client)->Loss {
    let row=db.query_one(r#"SELECT "solarSystemID","constellationID","regionID" FROM "mapSolarSystems"
        WHERE "solarSystemID"<>$1 ORDER BY "solarSystemID" LIMIT 1"#,&[&(f.system as i32)]).await.unwrap();
    let destination=SpaceArrivalCustody {system:i64::from(row.get::<_,i32>(0)),at:Vec3::new(701.0,802.0,903.0)};
    let mut loss=f.loss();loss.arrival_custody=Some(destination);
    // A source-space hull cannot be claimed with a fabricated destination.
    let tx=db.transaction().await.unwrap();
    assert!(!stage(&tx,&loss).await.unwrap());tx.rollback().await.unwrap();
    db.execute(r#"UPDATE "character_" SET "solarSystemID"=$2,"constellationID"=$3,"regionID"=$4 WHERE "characterID"=$1"#,
        &[&(loss.char_id as i32),&(destination.system as i32),&row.get::<_,i32>(1),&row.get::<_,i32>(2)]).await.unwrap();
    db.execute(r#"UPDATE entity SET "locationID"=$2,x=$3,y=$4,z=$5 WHERE "itemID"=ANY($1)"#,
        &[&vec![loss.char_id as i32,loss.ship_id as i32],&(destination.system as i32),&destination.at.x,&destination.at.y,&destination.at.z]).await.unwrap();
    loss
}

#[test]
fn arrival_custody_is_optional_for_old_journals_and_rejects_invalid_destinations() {
    let mut loss=Loss {client_id:0,char_id:100,ship_id:101,ship_type:587,system:30_000_001,pos:Vec3::new(1.0,2.0,3.0),
        arrival_custody:None,capsule:false,unpiloted:false,final_blow:None,attackers:vec![],items:vec![]};
    let mut old=serde_json::to_value(&loss).unwrap();old.as_object_mut().unwrap().remove("arrival_custody");
    let restored:Loss=serde_json::from_value(old).unwrap();
    assert_eq!(restored.custody_system(),loss.system);assert_eq!(restored.custody_position(),loss.pos);assert!(valid(&restored));
    for system in [0,-1,i64::from(i32::MAX)+1] {
        loss.arrival_custody=Some(SpaceArrivalCustody {system,at:Vec3::ZERO});assert!(!valid(&loss));
    }
    loss.arrival_custody=Some(SpaceArrivalCustody {system:30_000_002,at:Vec3::new(f64::NAN,0.0,0.0)});assert!(!valid(&loss));
    loss.arrival_custody=Some(SpaceArrivalCustody {system:30_000_002,at:Vec3::ZERO});assert!(valid(&loss));
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn arrival_loss_restarts_in_destination_with_immutable_source_attribution() {
    let mut f=Fixture::new(false).await;f.insured().await;
    let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    let mut loss=arrive(&f,&mut db).await;
    loss.final_blow=Some(-587);loss.attackers=vec![Attacker {who:-587,ship_type:587,weapon_type:34,damage:42.0}];
    let arrival=loss.arrival_custody.unwrap();
    let captured=capture(&mut db,&loss).await.unwrap().unwrap();assert_eq!(captured.arrival_custody,Some(arrival));
    let mut conflict=loss.clone();conflict.final_blow=Some(-601);conflict.arrival_custody.as_mut().unwrap().at=Vec3::ZERO;
    let recorded=capture(&mut db,&conflict).await.unwrap().unwrap();
    assert_eq!(recorded.final_blow,loss.final_blow);assert_eq!(recorded.arrival_custody,loss.arrival_custody);
    let stored=db.query_one(r#"SELECT "solarSystemID",occupied FROM "pendingShipLosses" WHERE "shipID"=$1"#,&[&(loss.ship_id as i32)]).await.unwrap();
    assert_eq!(stored.get::<_,i32>(0),loss.system as i32);assert!(stored.get::<_,bool>(1));
    let source_name:String=db.query_one(r#"SELECT "solarSystemName" FROM "mapSolarSystems" WHERE "solarSystemID"=$1"#,&[&(loss.system as i32)]).await.unwrap().get(0);
    assert!(report_text(&db,&loss).await.contains(&source_name));
    f.server.world=Default::default();
    for system in [loss.system,arrival.system] {
        f.server.world.lock().parks.insert(system,crate::ballpark::Park::new(system,vec![],crate::destiny::tick_now()));
    }
    retry(&f.server,&mut db).await;
    let row=db.query_one(r#"SELECT c."shipID",c.balance,e."locationID",e.x,e.y,e.z FROM "character_" c
        JOIN entity e ON e."itemID"=c."shipID" WHERE c."characterID"=$1"#,&[&(loss.char_id as i32)]).await.unwrap();
    let capsule=i64::from(row.get::<_,i32>(0));assert_ne!(capsule,loss.ship_id);assert_eq!(row.get::<_,f64>(1),1500.0);
    assert_eq!(i64::from(row.get::<_,i32>(2)),arrival.system);assert_eq!(Vec3::new(row.get(3),row.get(4),row.get(5)),arrival.at);
    let kill=db.query_one(r#"SELECT "solarSystemID","locationID","finalBlowID","totalDamage" FROM "killLog" WHERE "victimItemID"=$1"#,&[&(loss.ship_id as i32)]).await.unwrap();
    assert_eq!(kill.get::<_,i32>(0),loss.system as i32);assert_eq!(kill.get::<_,i32>(1),loss.system as i32);
    assert_eq!(kill.get::<_,i32>(2),-587);assert_eq!(kill.get::<_,f64>(3),42.0);
    {
        let w=f.server.world.lock();assert!(w.parks[&loss.system].ships.is_empty());
        assert_eq!(w.parks[&arrival.system].ships[&capsule].pos,arrival.at);
    }
    // Cargo either dies or survives in a can at the same destination; the
    // probabilistic drop decision does not change its custody requirements.
    let cans=db.query(r#"SELECT "locationID",x,y,z FROM entity WHERE "ownerID"=$1 AND "typeID"=23"#,&[&(loss.char_id as i32)]).await.unwrap();
    for can in cans {assert_eq!(i64::from(can.get::<_,i32>(0)),arrival.system);assert_eq!(Vec3::new(can.get(1),can.get(2),can.get(3)),arrival.at);}
    handle(&f.server,&mut db,loss.clone()).await;
    assert_eq!(db.query_one(r#"SELECT count(*) FROM "killLog" WHERE "victimItemID"=$1"#,&[&(loss.ship_id as i32)]).await.unwrap().get::<_,i64>(0),1);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn arrival_loss_does_not_redirect_a_newer_boarded_hull() {
    let f=Fixture::new(false).await;let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    let loss=arrive(&f,&mut db).await;capture(&mut db,&loss).await.unwrap().unwrap();
    let newer=db.query_one("SELECT nextval('dyn_id_seq')::integer",&[]).await.unwrap().get::<_,i32>(0);
    db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton)
        VALUES($1,587,$2,$3,0,1,1)"#,&[&newer,&(loss.char_id as i32),&(loss.custody_system() as i32)]).await.unwrap();
    db.execute(r#"UPDATE "character_" SET "shipID"=$2 WHERE "characterID"=$1"#,&[&(loss.char_id as i32),&newer]).await.unwrap();
    handle(&f.server,&mut db,loss.clone()).await;
    assert!(completed(&db,&loss).await.unwrap());
    assert_eq!(db.query_one(r#"SELECT "shipID" FROM "character_" WHERE "characterID"=$1"#,&[&(loss.char_id as i32)]).await.unwrap().get::<_,i32>(0),newer);
    assert!(db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&newer]).await.unwrap().is_some());
    assert_eq!(db.query_one(r#"SELECT count(*) FROM entity WHERE "ownerID"=$1 AND "typeID"=670"#,&[&(loss.char_id as i32)]).await.unwrap().get::<_,i64>(0),0);
    assert!(!blocked_in_memory(&f.server.world,loss.ship_id));f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn arrival_capsule_loss_uses_destination_custody_and_source_kill_location() {
    let mut f=Fixture::new(true).await;let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    crate::svc::medical::replace_clone(&db,f.ship.pilot.char_id,TYPE_CLONE_ALPHA,Some(f.station)).await.unwrap();
    let loss=arrive(&f,&mut db).await;capture(&mut db,&loss).await.unwrap().unwrap();
    f.server.world=Default::default();retry(&f.server,&mut db).await;
    assert!(completed(&db,&loss).await.unwrap());
    let row=db.query_one(r#"SELECT c."stationID",c."shipID",e."locationID" FROM "character_" c JOIN entity e ON e."itemID"=c."shipID"
        WHERE c."characterID"=$1"#,&[&(loss.char_id as i32)]).await.unwrap();
    assert_eq!(row.get::<_,i32>(0),f.station as i32);assert_ne!(row.get::<_,i32>(1),loss.ship_id as i32);assert_eq!(row.get::<_,i32>(2),f.station as i32);
    let kill=db.query_one(r#"SELECT "solarSystemID","isPodKill" FROM "killLog" WHERE "victimItemID"=$1"#,&[&(loss.ship_id as i32)]).await.unwrap();
    assert_eq!(kill.get::<_,i32>(0),loss.system as i32);assert_eq!(kill.get::<_,i16>(1),1);
    assert!(db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(f.cargo as i32)]).await.unwrap().is_none());f.cleanup().await;
}
