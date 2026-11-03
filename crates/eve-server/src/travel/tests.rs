use super::*;

struct Fixture {server:Server,ship:Ship,source:Place,destination:Place}
impl Fixture {
    async fn new()->Self {
        let f=crate::test_support::Database::load().await;
        let pilot=f.id().await;let hull=f.id().await;
        let rows=f.db.query(r#"SELECT "solarSystemID","constellationID","regionID" FROM "mapSolarSystems" ORDER BY "solarSystemID" LIMIT 2"#,&[]).await.unwrap();
        let place=|n:usize|Place {system:i64::from(rows[n].get::<_,i32>(0)),station:None,
            constellation:i64::from(rows[n].get::<_,i32>(1)),region:i64::from(rows[n].get::<_,i32>(2)),
            at:Vec3::new(1000.0+n as f64*1000.0,2000.0,3000.0)};
        let source=place(0);let destination=place(1);
        f.db.execute(r#"INSERT INTO "character_"("characterID","characterName","corporationID","shipID","stationID","solarSystemID","constellationID","regionID",balance)
            VALUES($1,$2,1000167,$3,0,$4,$5,$6,1000)"#,&[&pilot,&format!("Travel receipt {pilot}"),&hull,&(source.system as i32),&(source.constellation as i32),&(source.region as i32)]).await.unwrap();
        for (item,ty,flag) in [(pilot,1373,57),(hull,587,0)] {
            f.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton,x,y,z)
                VALUES($1,$2,$3,$4,$5,1,1,$6,$7,$8)"#,&[&item,&ty,&pilot,&(source.system as i32),&flag,&source.at.x,&source.at.y,&source.at.z]).await.unwrap();
        }
        let profile=crate::svc::inflight::profile(&f.db,&f.dogma,pilot as i64,hull as i64,&Default::default(),crate::combat::now_ms()).await.unwrap().unwrap();
        let mut ship=Ship::new(hull as i64,10,profile.stats,profile.pilot,source.at,Vec3::ZERO);ship.fight=Some(profile.fighter);
        let server=Server {table:Default::default(),db:crate::db::Shared::new(f.db,f.url.clone()),db_url:f.url,node_id:1,
            bulk:f.bulk,dogma:f.dogma,world:f.world,services:crate::svc::registry(),dump_dir:Default::default(),
            alliance_join_delay_secs:0,sov_claim_delay_secs:0};
        Self {server,ship,source,destination}
    }
    async fn stage(&self,tx:&Transaction<'_>,source:Place,destination:Place)->Marker {
        assert!(crate::svc::login::lock_character_lifecycle(tx,self.ship.pilot.char_id as i32).await.unwrap());
        tx.execute(r#"UPDATE "character_" SET "solarSystemID"=$2,"constellationID"=$3,"regionID"=$4 WHERE "characterID"=$1"#,
            &[&(self.ship.pilot.char_id as i32),&(destination.system as i32),&(destination.constellation as i32),&(destination.region as i32)]).await.unwrap();
        tx.execute(r#"UPDATE entity SET "locationID"=$2,x=$3,y=$4,z=$5 WHERE "itemID"=ANY($1)"#,
            &[&vec![self.ship.id as i32,self.ship.pilot.char_id as i32],&(destination.system as i32),&destination.at.x,&destination.at.y,&destination.at.z]).await.unwrap();
        begin(tx,self.ship.pilot.char_id,self.ship.id,source,destination).await.unwrap()
    }
    async fn accepted(&self,db:&mut Client)->Marker {let tx=db.transaction().await.unwrap();let m=self.stage(&tx,self.source,self.destination).await;tx.commit().await.unwrap();m}
    fn healthy(&self,at:Vec3,damage:f64)->Final {
        let mut ship=self.ship.clone();ship.arrive(at);ship.fight.as_mut().unwrap().vitals.hull_damage=damage;
        Final::Healthy {ship,tick:crate::destiny::tick_now()}
    }
    fn loss(&self)->Loss {
        let mut loss=Loss::of(&self.ship,self.source.system,false);
        loss.arrival_custody=Some(crate::losses::SpaceArrivalCustody {system:self.destination.system,at:self.destination.at});loss
    }
    async fn cleanup(&self) {
        let db=self.server.db.snapshot();let pilot=self.ship.pilot.char_id as i32;
        db.execute(r#"DELETE FROM "travelReceipts" WHERE "characterID"=$1"#,&[&pilot]).await.unwrap();
        db.execute(r#"DELETE FROM entity_attributes WHERE "itemID" IN(SELECT "itemID" FROM entity WHERE "ownerID"=$1)"#,&[&pilot]).await.unwrap();
        db.execute(r#"DELETE FROM entity WHERE "ownerID"=$1"#,&[&pilot]).await.unwrap();
        db.execute(r#"DELETE FROM "character_" WHERE "characterID"=$1"#,&[&pilot]).await.unwrap();
    }
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn rollback_is_proven_but_marker_only_restart_and_changed_custody_remain_quarantined() {
    let mut f=Fixture::new().await;let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    let tx=db.transaction().await.unwrap();let rolled=f.stage(&tx,f.source,f.destination).await;tx.rollback().await.unwrap();
    assert_eq!(resolve(&mut db,&rolled).await.unwrap(),Resolution::RolledBack);
    assert!(!blocked(&db,f.ship.id).await.unwrap());
    let marker=f.accepted(&mut db).await;
    assert!(finalize(&f.server,&mut db,marker,f.healthy(f.source.at,7.0)).await.is_err(),"a source snapshot cannot become a destination checkpoint");
    assert!(blocked(&db,marker.ship).await.unwrap());
    track_uncertain(&f.server.world,&marker).unwrap();
    assert!(blocked_character_in_memory(&f.server.world,marker.pilot));
    f.server.world=World::default();retry(&f.server,&mut db).await;
    assert_eq!(resolve(&mut db,&marker).await.unwrap(),Resolution::Accepted);
    assert!(blocked_character(&db,marker.pilot).await.unwrap());
    assert!(db.query_one(r#"SELECT loss IS NULL FROM "travelReceipts" WHERE generation=$1"#,&[&marker.generation]).await.unwrap().get::<_,bool>(0));
    db.execute(r#"UPDATE entity SET "ownerID"=1 WHERE "itemID"=$1"#,&[&(marker.ship as i32)]).await.unwrap();
    assert_eq!(resolve(&mut db,&marker).await.unwrap(),Resolution::Unknown);
    assert!(finalize(&f.server,&mut db,marker,f.healthy(f.destination.at,7.0)).await.is_err());
    db.execute(r#"UPDATE entity SET "ownerID"=$2 WHERE "itemID"=$1"#,&[&(marker.ship as i32),&(marker.pilot as i32)]).await.unwrap();
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn healthy_final_write_rolls_back_retries_and_stale_receipts_never_overwrite_newer_travel() {
    let f=Fixture::new().await;let mut db=crate::db::connect(&f.server.db_url).await.unwrap();let marker=f.accepted(&mut db).await;
    let constraint=format!("travel_vitals_{}",marker.ship);
    db.batch_execute(&format!(r#"ALTER TABLE entity_attributes ADD CONSTRAINT {constraint} CHECK("itemID"<>{} OR "attributeID"<>3) NOT VALID"#,marker.ship)).await.unwrap();
    assert!(finalize(&f.server,&mut db,marker,f.healthy(f.destination.at,7.0)).await.is_err());
    assert!(blocked(&db,marker.ship).await.unwrap() && blocked_in_memory(&f.server.world,marker.ship));
    assert!(db.query_opt(r#"SELECT 1 FROM "shipSpaceMotion" WHERE "itemID"=$1"#,&[&(marker.ship as i32)]).await.unwrap().is_none());
    db.batch_execute(&format!("ALTER TABLE entity_attributes DROP CONSTRAINT {constraint}")).await.unwrap();
    retry(&f.server,&mut db).await;
    assert_eq!(resolve(&mut db,&marker).await.unwrap(),Resolution::Finalized);
    assert!(!blocked_in_memory(&f.server.world,marker.ship));
    assert_eq!(db.query_one(r#"SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(marker.ship as i32)]).await.unwrap().get::<_,f64>(0),7.0);
    let tx=db.transaction().await.unwrap();let newer=f.stage(&tx,f.destination,f.source).await;tx.commit().await.unwrap();
    finalize(&f.server,&mut db,marker,f.healthy(f.destination.at,99.0)).await.unwrap();
    assert_eq!(resolve(&mut db,&newer).await.unwrap(),Resolution::Accepted);
    assert_eq!(db.query_one(r#"SELECT "valueFloat" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&(marker.ship as i32)]).await.unwrap().get::<_,f64>(0),7.0);
    finalize(&f.server,&mut db,newer,f.healthy(f.source.at,2.0)).await.unwrap();
    db.execute(r#"DELETE FROM entity WHERE "itemID"=$1"#,&[&(marker.ship as i32)]).await.unwrap();
    assert_eq!(resolve(&mut db,&marker).await.unwrap(),Resolution::Finalized,"receipt survives hull deletion");
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn known_loss_survives_final_staging_failure_and_restarts_with_source_attribution() {
    let mut f=Fixture::new().await;let mut db=crate::db::connect(&f.server.db_url).await.unwrap();let marker=f.accepted(&mut db).await;
    let constraint=format!("travel_loss_{}",marker.ship);
    db.batch_execute(&format!(r#"ALTER TABLE "pendingShipLosses" ADD CONSTRAINT {constraint} CHECK("shipID"<>{}) NOT VALID"#,marker.ship)).await.unwrap();
    assert!(finalize(&f.server,&mut db,marker,Final::Lost(f.loss())).await.is_err());
    assert!(db.query_one(r#"SELECT state=0 AND loss IS NOT NULL FROM "travelReceipts" WHERE generation=$1"#,&[&marker.generation]).await.unwrap().get::<_,bool>(0));
    assert!(db.query_opt(r#"SELECT 1 FROM "pendingShipLosses" WHERE "shipID"=$1"#,&[&(marker.ship as i32)]).await.unwrap().is_none());
    f.server.world=World::default();db.batch_execute(&format!("ALTER TABLE \"pendingShipLosses\" DROP CONSTRAINT {constraint}")).await.unwrap();
    retry(&f.server,&mut db).await;
    assert_eq!(resolve(&mut db,&marker).await.unwrap(),Resolution::Finalized);
    let raw:String=db.query_one(r#"SELECT loss FROM "pendingShipLosses" WHERE "shipID"=$1"#,&[&(marker.ship as i32)]).await.unwrap().get(0);
    let saved:Loss=serde_json::from_str(&raw).unwrap();
    assert_eq!(saved.system,f.source.system);assert_eq!(saved.pos,f.source.at);
    assert_eq!(saved.custody_system(),f.destination.system);assert_eq!(saved.custody_position(),f.destination.at);
    f.cleanup().await;
}

#[tokio::test]
#[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
async fn corrupt_receipts_do_not_starve_later_known_loss_recovery() {
    let f=Fixture::new().await;let mut db=crate::db::connect(&f.server.db_url).await.unwrap();
    let source=serde_json::to_string(&f.source).unwrap();let destination=serde_json::to_string(&f.destination).unwrap();
    let ids=db.query("SELECT nextval('dyn_id_seq')::integer FROM generate_series(1,64)",&[]).await.unwrap().iter().map(|r|r.get::<_,i32>(0)).collect::<Vec<_>>();
    for id in &ids {
        db.execute(r#"INSERT INTO "travelReceipts"("characterID","shipID","shipTypeID",source,destination,"createdAt",loss)
            VALUES($1,$1,587,$2,$3,0,'{}')"#,&[id,&source,&destination]).await.unwrap();
    }
    let marker=f.accepted(&mut db).await;let raw=serde_json::to_string(&f.loss()).unwrap();
    db.execute(r#"UPDATE "travelReceipts" SET loss=$2 WHERE generation=$1"#,&[&marker.generation,&raw]).await.unwrap();
    retry(&f.server,&mut db).await;assert_eq!(resolve(&mut db,&marker).await.unwrap(),Resolution::Accepted);
    retry(&f.server,&mut db).await;assert_eq!(resolve(&mut db,&marker).await.unwrap(),Resolution::Finalized);
    db.execute(r#"DELETE FROM "travelReceipts" WHERE "characterID"=ANY($1)"#,&[&ids]).await.unwrap();f.cleanup().await;
}
