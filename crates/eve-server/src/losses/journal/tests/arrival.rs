use super::*;

fn arrive(f:&Fixture)->Loss {
    let store=f.store();
    let (system,constellation,region)=store.read(|db| {
        let s=db.table("mapSolarSystems").rows().filter(|s| s.int("solarSystemID")!=Some(f.system)).min_by_key(|s| s.int("solarSystemID")).unwrap();
        (s.int("solarSystemID").unwrap(),s.int("constellationID").unwrap(),s.int("regionID").unwrap())
    });
    let destination=SpaceArrivalCustody {system,at:Vec3::new(701.0,802.0,903.0)};
    let mut loss=f.loss();loss.arrival_custody=Some(destination);
    // A source-space hull cannot be claimed with a fabricated destination.
    assert!(!store.write(|tx| stage(tx,&loss)).unwrap());
    store.write(|tx| {
        update_id(tx,"character_",loss.char_id,&[("solarSystemID",int(system)),("constellationID",int(constellation)),("regionID",int(region))])?;
        for id in [loss.char_id,loss.ship_id] {
            update_id(tx,"entity",id,&[("locationID",int(system)),("x",store::Value::Float(destination.at.x)),
                ("y",store::Value::Float(destination.at.y)),("z",store::Value::Float(destination.at.z))])?;
        }
        Ok::<_,StoreError>(())
    }).unwrap();
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
async fn arrival_loss_restarts_in_destination_with_immutable_source_attribution() {
    let mut f=Fixture::new(false).await;f.insured();
    let store=f.store();
    let mut loss=arrive(&f);
    loss.final_blow=Some(-587);loss.attackers=vec![Attacker {who:-587,ship_type:587,weapon_type:34,damage:42.0}];
    let arrival=loss.arrival_custody.unwrap();
    let captured=capture(&store,&loss).unwrap().unwrap();assert_eq!(captured.arrival_custody,Some(arrival));
    let mut conflict=loss.clone();conflict.final_blow=Some(-601);conflict.arrival_custody.as_mut().unwrap().at=Vec3::ZERO;
    let recorded=capture(&store,&conflict).unwrap().unwrap();
    assert_eq!(recorded.final_blow,loss.final_blow);assert_eq!(recorded.arrival_custody,loss.arrival_custody);
    let stored=store.read(|db| db.table("pendingShipLosses").get_id(loss.ship_id).map(|p| (p.int("solarSystemID"),p.bool("occupied"))));
    assert_eq!(stored,Some((Some(loss.system),Some(true))));
    let (source_name,report)=store.read(|db| {
        let name=db.table("mapSolarSystems").get_id(loss.system).and_then(|s| s.text("solarSystemName")).unwrap().to_string();
        (name,report_text(db,&loss))
    });
    assert!(report.contains(&source_name));
    f.server.world=Default::default();
    for system in [loss.system,arrival.system] {
        f.server.world.lock().parks.insert(system,crate::ballpark::Park::new(system,vec![],crate::destiny::tick_now()));
    }
    retry(&f.server,&store).await;
    let capsule=fixture::character_int(&store,loss.char_id,"shipID").unwrap();
    assert_ne!(capsule,loss.ship_id);assert_eq!(fixture::character_float(&store,loss.char_id,"balance"),Some(1500.0));
    let at=store.read(|db| db.table("entity").get_id(capsule).map(|e| (e.int("locationID").unwrap(),Vec3::new(e.float("x").unwrap(),e.float("y").unwrap(),e.float("z").unwrap())))).unwrap();
    assert_eq!(at,(arrival.system,arrival.at));
    let kill=store.read(|db| db.table("killLog").rows().find(|k| k.int("victimItemID")==Some(loss.ship_id))
        .map(|k| (k.int("solarSystemID"),k.int("locationID"),k.int("finalBlowID"),k.float("totalDamage")))).unwrap();
    assert_eq!(kill,(Some(loss.system),Some(loss.system),Some(-587),Some(42.0)));
    {
        let w=f.server.world.lock();assert!(w.parks[&loss.system].ships.is_empty());
        assert_eq!(w.parks[&arrival.system].ships[&capsule].pos,arrival.at);
    }
    // Cargo either dies or survives in a can at the same destination; the
    // probabilistic drop decision does not change its custody requirements.
    let cans:Vec<(i64,Vec3)>=store.read(|db| db.table("entity").find(&["ownerID"],&[int(loss.char_id)]).filter(|e| e.int("typeID")==Some(23))
        .map(|e| (e.int("locationID").unwrap(),Vec3::new(e.float("x").unwrap(),e.float("y").unwrap(),e.float("z").unwrap()))).collect());
    for can in cans {assert_eq!(can,(arrival.system,arrival.at));}
    handle(&f.server,&store,loss.clone()).await;
    assert_eq!(kills(&store,loss.ship_id),1);
}

#[tokio::test]
async fn arrival_loss_does_not_redirect_a_newer_boarded_hull() {
    let f=Fixture::new(false).await;let store=f.store();
    let loss=arrive(&f);capture(&store,&loss).unwrap().unwrap();
    let newer=fixture::id(&store);
    store.write(|tx| {
        fixture::item(tx,(newer,587,loss.char_id,loss.custody_system(),0,1),true,(0.0,0.0,0.0))?;
        update_id(tx,"character_",loss.char_id,&[("shipID",int(newer))])
    }).unwrap();
    handle(&f.server,&store,loss.clone()).await;
    assert!(is_completed(&store,&loss));
    assert_eq!(fixture::character_int(&store,loss.char_id,"shipID"),Some(newer));
    assert!(exists(&store,newer));
    let capsules=store.read(|db| db.table("entity").find(&["ownerID"],&[int(loss.char_id)]).filter(|e| e.int("typeID")==Some(670)).count());
    assert_eq!(capsules,0);
    assert!(!blocked_in_memory(&f.server.world,loss.ship_id));
}

#[tokio::test]
async fn arrival_capsule_loss_uses_destination_custody_and_source_kill_location() {
    let mut f=Fixture::new(true).await;let store=f.store();
    store.write(|tx| crate::svc::medical::replace_clone(tx,f.pilot(),TYPE_CLONE_ALPHA,Some(f.station))).unwrap();
    let loss=arrive(&f);capture(&store,&loss).unwrap().unwrap();
    f.server.world=Default::default();retry(&f.server,&store).await;
    assert!(is_completed(&store,&loss));
    let pod=fixture::character_int(&store,loss.char_id,"shipID").unwrap();
    assert_eq!(fixture::character_int(&store,loss.char_id,"stationID"),Some(f.station));assert_ne!(pod,loss.ship_id);
    assert_eq!(fixture::entity_int(&store,pod,"locationID"),Some(f.station));
    let kill=store.read(|db| db.table("killLog").rows().find(|k| k.int("victimItemID")==Some(loss.ship_id)).map(|k| (k.int("solarSystemID"),k.int("isPodKill"))));
    assert_eq!(kill,Some((Some(loss.system),Some(1))));
    assert!(!exists(&store,f.cargo));
}
