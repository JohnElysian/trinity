use super::*;

struct Fixture {server:Server,ship:Ship,source:Place,destination:Place}
impl Fixture {
    async fn new()->Self {
        let f=crate::test_support::Database::load().await;
        let pilot=i64::from(f.id().await);let hull=i64::from(f.id().await);
        let mut systems:Vec<(i64,i64,i64)>=f.store.read(|db|db.table("mapSolarSystems").rows()
            .map(|r|(r.int("solarSystemID").unwrap(),r.int("constellationID").unwrap(),r.int("regionID").unwrap())).collect());
        systems.sort_unstable();
        let place=|n:usize|Place {system:systems[n].0,station:None,constellation:systems[n].1,region:systems[n].2,
            at:Vec3::new(1000.0+n as f64*1000.0,2000.0,3000.0)};
        let source=place(0);let destination=place(1);
        f.store.write(|tx|->Result<(),StoreError> {
            tx.insert("character_",&[("characterID",Value::Int(pilot)),("characterName",Value::Text(format!("Travel receipt {pilot}").into())),
                ("corporationID",Value::Int(1000167)),("shipID",Value::Int(hull)),("stationID",Value::Int(0)),("solarSystemID",Value::Int(source.system)),
                ("constellationID",Value::Int(source.constellation)),("regionID",Value::Int(source.region)),("balance",Value::Float(1000.0))])?;
            for (item,ty,flag) in [(pilot,1373,57),(hull,587,0)] {
                tx.insert("entity",&[("itemID",Value::Int(item)),("typeID",Value::Int(ty)),("ownerID",Value::Int(pilot)),("locationID",Value::Int(source.system)),
                    ("flag",Value::Int(flag)),("quantity",Value::Int(1)),("singleton",Value::Int(1)),
                    ("x",Value::Float(source.at.x)),("y",Value::Float(source.at.y)),("z",Value::Float(source.at.z))])?;
            }
            Ok(())
        }).unwrap();
        let profile=f.store.read(|db|crate::svc::inflight::profile(db,&f.dogma,pilot,hull,&Default::default(),crate::combat::now_ms())).unwrap().unwrap();
        let mut ship=Ship::new(hull,10,profile.stats,profile.pilot,source.at,Vec3::ZERO);ship.fight=Some(profile.fighter);
        let server=Server {table:Default::default(),store:f.store,node_id:1,
            bulk:f.bulk,dogma:f.dogma,world:f.world,services:crate::svc::registry(),dump_dir:Default::default(),
            alliance_join_delay_secs:0,sov_claim_delay_secs:0};
        Self {server,ship,source,destination}
    }
    fn store(&self)->&Store {&self.server.store}
    fn stage(&self,tx:&mut Tx<'_>,source:Place,destination:Place)->Marker {
        let pilot=self.ship.pilot.char_id;
        let character=tx.table("character_").get_id(pilot).unwrap().id;
        tx.update("character_",character,&[("solarSystemID",Value::Int(destination.system)),("constellationID",Value::Int(destination.constellation)),
            ("regionID",Value::Int(destination.region))]).unwrap();
        for item in [self.ship.id,pilot] {
            let row=tx.table("entity").get_id(item).unwrap().id;
            tx.update("entity",row,&[("locationID",Value::Int(destination.system)),("x",Value::Float(destination.at.x)),
                ("y",Value::Float(destination.at.y)),("z",Value::Float(destination.at.z))]).unwrap();
        }
        begin(tx,pilot,self.ship.id,source,destination).unwrap()
    }
    fn accepted(&self)->Marker {self.store().write(|tx|->Result<_,StoreError> {Ok(self.stage(tx,self.source,self.destination))}).unwrap()}
    fn healthy(&self,at:Vec3,damage:f64)->Final {
        let mut ship=self.ship.clone();ship.arrive(at);ship.fight.as_mut().unwrap().vitals.hull_damage=damage;
        Final::Healthy {ship,tick:crate::destiny::tick_now()}
    }
    fn loss(&self)->Loss {
        let mut loss=Loss::of(&self.ship,self.source.system,false);
        loss.arrival_custody=Some(crate::losses::SpaceArrivalCustody {system:self.destination.system,at:self.destination.at});loss
    }
    fn set_owner(&self,ship:i64,owner:i64) {
        self.store().write(|tx|{let row=tx.table("entity").get_id(ship).unwrap().id;tx.update("entity",row,&[("ownerID",Value::Int(owner))])}).unwrap();
    }
    fn receipt<T>(&self,generation:i64,f:impl FnOnce(crate::store::Row)->T)->T {
        self.store().read(|db|f(db.table("travelReceipts").get_id(generation).unwrap()))
    }
    fn hull_damage(&self,ship:i64)->Option<f64> {
        self.store().read(|db|db.table("entity_attributes").get(&[Value::Int(ship),Value::Int(3)]).and_then(|r|r.float("valueFloat")))
    }
}

#[tokio::test]
async fn rollback_is_proven_but_marker_only_restart_and_changed_custody_remain_quarantined() {
    let mut f=Fixture::new().await;
    let mut rolled=None;
    let undone=f.store().write(|tx|->Result<(),StoreError> {
        rolled=Some(f.stage(tx,f.source,f.destination));
        Err(StoreError {code:crate::store::DATA_EXCEPTION,message:"rolled back".into()})
    });
    assert!(undone.is_err());
    let rolled=rolled.unwrap();
    assert_eq!(resolve(f.store(),&rolled).await.unwrap(),Resolution::RolledBack);
    assert!(!f.store().read(|db|blocked(db,f.ship.id)).unwrap());
    let marker=f.accepted();
    assert!(finalize(&f.server,f.store(),marker,f.healthy(f.source.at,7.0)).await.is_err(),"a source snapshot cannot become a destination checkpoint");
    assert!(f.store().read(|db|blocked(db,marker.ship)).unwrap());
    track_uncertain(&f.server.world,&marker).unwrap();
    assert!(blocked_character_in_memory(&f.server.world,marker.pilot));
    f.server.world=World::default();retry(&f.server,f.store()).await;
    assert_eq!(resolve(f.store(),&marker).await.unwrap(),Resolution::Accepted);
    assert!(f.store().read(|db|blocked_character(db,marker.pilot)).unwrap());
    assert!(f.receipt(marker.generation,|r|r.text("loss").is_none()));
    f.set_owner(marker.ship,1);
    assert_eq!(resolve(f.store(),&marker).await.unwrap(),Resolution::Unknown);
    assert!(finalize(&f.server,f.store(),marker,f.healthy(f.destination.at,7.0)).await.is_err());
}

#[tokio::test]
async fn healthy_final_write_refused_retries_and_stale_receipts_never_overwrite_newer_travel() {
    let f=Fixture::new().await;let marker=f.accepted();
    // A pending station arrival for the hull refuses its space checkpoint.
    f.store().write(|tx|tx.insert("pendingStationArrivals",&[("shipID",Value::Int(marker.ship)),("characterID",Value::Int(marker.pilot)),
        ("stationID",Value::Int(60014689)),("sourceSystemID",Value::Int(f.source.system)),("shipTypeID",Value::Int(marker.ship_type)),
        ("createdAt",Value::Int(0))])).unwrap();
    assert!(finalize(&f.server,f.store(),marker,f.healthy(f.destination.at,7.0)).await.is_err());
    assert!(f.store().read(|db|blocked(db,marker.ship)).unwrap() && blocked_in_memory(&f.server.world,marker.ship));
    assert!(f.store().read(|db|db.table("shipSpaceMotion").get_id(marker.ship).is_none()));
    f.store().write(|tx|tx.delete_where("pendingStationArrivals",&["shipID"],&[Value::Int(marker.ship)])).unwrap();
    retry(&f.server,f.store()).await;
    assert_eq!(resolve(f.store(),&marker).await.unwrap(),Resolution::Finalized);
    assert!(!blocked_in_memory(&f.server.world,marker.ship));
    assert_eq!(f.hull_damage(marker.ship),Some(7.0));
    let newer=f.store().write(|tx|->Result<_,StoreError> {Ok(f.stage(tx,f.destination,f.source))}).unwrap();
    finalize(&f.server,f.store(),marker,f.healthy(f.destination.at,99.0)).await.unwrap();
    assert_eq!(resolve(f.store(),&newer).await.unwrap(),Resolution::Accepted);
    assert_eq!(f.hull_damage(marker.ship),Some(7.0));
    finalize(&f.server,f.store(),newer,f.healthy(f.source.at,2.0)).await.unwrap();
    f.store().write(|tx|tx.delete_where("entity",&["itemID"],&[Value::Int(marker.ship)])).unwrap();
    assert_eq!(resolve(f.store(),&marker).await.unwrap(),Resolution::Finalized,"receipt survives hull deletion");
}

#[tokio::test]
async fn known_loss_retained_on_its_receipt_restarts_with_source_attribution() {
    let mut f=Fixture::new().await;let marker=f.accepted();
    // The server stopped after the loss was retained on the receipt, before
    // it was staged: a restart recovers it from the receipt alone.
    let raw=serde_json::to_string(&f.loss()).unwrap();
    f.store().write(|tx|storage::retain_loss(tx,marker.generation,&raw)).unwrap();
    assert!(f.receipt(marker.generation,|r|r.int("state")==Some(0) && r.text("loss").is_some()));
    assert!(f.store().read(|db|db.table("pendingShipLosses").get_id(marker.ship).is_none()));
    f.server.world=World::default();
    retry(&f.server,f.store()).await;
    assert_eq!(resolve(f.store(),&marker).await.unwrap(),Resolution::Finalized);
    let raw=f.store().read(|db|db.table("pendingShipLosses").get_id(marker.ship).unwrap().text("loss").unwrap().to_string());
    let saved:Loss=serde_json::from_str(&raw).unwrap();
    assert_eq!(saved.system,f.source.system);assert_eq!(saved.pos,f.source.at);
    assert_eq!(saved.custody_system(),f.destination.system);assert_eq!(saved.custody_position(),f.destination.at);
}

#[tokio::test]
async fn corrupt_receipts_do_not_starve_later_known_loss_recovery() {
    let f=Fixture::new().await;
    let source=serde_json::to_string(&f.source).unwrap();let destination=serde_json::to_string(&f.destination).unwrap();
    f.store().write(|tx|->Result<(),StoreError> {
        for _ in 0..64 {
            let id=tx.nextval("dyn_id_seq")?;
            tx.insert("travelReceipts",&[("characterID",Value::Int(id)),("shipID",Value::Int(id)),("shipTypeID",Value::Int(587)),
                ("source",Value::Text(source.as_str().into())),("destination",Value::Text(destination.as_str().into())),
                ("createdAt",Value::Int(0)),("loss",Value::Text("{}".into()))])?;
        }
        Ok(())
    }).unwrap();
    let marker=f.accepted();let raw=serde_json::to_string(&f.loss()).unwrap();
    f.store().write(|tx|{let row=tx.table("travelReceipts").get_id(marker.generation).unwrap().id;tx.update("travelReceipts",row,&[("loss",Value::Text(raw.into()))])}).unwrap();
    retry(&f.server,f.store()).await;assert_eq!(resolve(f.store(),&marker).await.unwrap(),Resolution::Accepted);
    retry(&f.server,f.store()).await;assert_eq!(resolve(f.store(),&marker).await.unwrap(),Resolution::Finalized);
}
