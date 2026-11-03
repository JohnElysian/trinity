//! A lethal hit during an accepted arrival still destroys the source hull.
//! Settle the loss before publishing the new station session. Normal ship
//! losses leave a capsule at the accepted station; pod losses use medical home.
use super::*;
use crate::svc::medical;

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub struct ArrivalDisposition {
    pub ship_id:i64,
    pub station_id:i64,
    pub solar_system_id:i64,
    pub constellation_id:i64,
    pub region_id:i64,
}

fn failed(error:tokio_postgres::Error)->String {
    error!("settling lethal station arrival: {}",crate::logging::chain(&error));
    "The ship's loss could not yet be settled.".into()
}

/// `system` and `ship.pos` describe the source fight, including an administrative
/// move to a station in a different system. Destination metadata is read afresh.
/// No Move event is emitted: the transition owner publishes this disposition.
#[cfg(test)]
pub async fn settle_docked_arrival(server:&Server,db:&mut Client,station:i64,system:i64,ship:&Ship)
    ->Result<ArrivalDisposition,String> {
    if ship.unpiloted || ship.fight.as_ref().is_none_or(|f| !f.vitals.hull_damage.is_finite()
        || !f.vitals.hull_max.is_finite() || f.vitals.hull_max<=0.0 || f.vitals.hull_damage<f.vitals.hull_max-1e-6) {
        return Err("The arriving ship has no confirmed occupied hull loss.".into());
    }
    let loss=Loss::of(ship,system,ship.pilot.type_id==TYPE_CAPSULE);
    settle_loss(server,db,station,&loss).await
}

pub(super) async fn settle_loss(server:&Server,db:&mut Client,station:i64,loss:&Loss)
    ->Result<ArrivalDisposition,String> {
    let pilot=loss.char_id;
    let mut txn=ItemTxn::begin(db).await.map_err(failed)?;
    if !crate::svc::login::lock_character_lifecycle(txn.tx(),pilot as i32).await.map_err(failed)? {
        return Err("The arriving character no longer exists.".into());
    }
    let row=txn.tx().query_opt(r#"SELECT "shipID",COALESCE("stationID",0),"solarSystemID","constellationID","regionID"
        FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#,&[&(pilot as i32)]).await.map_err(failed)?
        .ok_or("The arriving character no longer exists.")?;
    let current=ArrivalDisposition {ship_id:i64::from(row.get::<_,i32>(0)),station_id:i64::from(row.get::<_,i32>(1)),
        solar_system_id:i64::from(row.get::<_,i32>(2)),constellation_id:i64::from(row.get::<_,i32>(3)),region_id:i64::from(row.get::<_,i32>(4))};
    let hull=txn.item(loss.ship_id).await.map_err(failed)?;
    if hull.is_none() {
        // A retry after a successful (possibly unacknowledged) COMMIT must not
        // create another capsule, duplicate the kill record, or pay twice.
        let done=txn.tx().query_opt(r#"SELECT 1 FROM "killLog" WHERE "victimItemID"=$1 AND "victimCharID"=$2 LIMIT 1"#,
            &[&(loss.ship_id as i32),&(pilot as i32)]).await.map_err(failed)?.is_some();
        if done && current.ship_id!=loss.ship_id && current.ship_id>0 && current.station_id>=0 {
            if let Some(replacement)=txn.item(current.ship_id).await.map_err(failed)? {
                let location=if current.station_id>0 {current.station_id}else{current.solar_system_id};
                let flag=if current.station_id>0 {FLAG_HANGAR}else{FLAG_IN_SPACE};
                if replacement.category_id==6 && replacement.owner_id==pilot && replacement.location_id==location
                    && replacement.flag==flag && replacement.quantity==1 && replacement.singleton {
                    // Settlement may have committed before a lost reply. A
                    // subsequent boarding or undock is newer authoritative
                    // state; recovery must not require the original capsule.
                    let place=if current.station_id>0 {station_place(txn.tx(),current.station_id).await.map_err(failed)?}else{
                        txn.tx().query_opt(r#"SELECT "solarSystemID","constellationID","regionID" FROM "mapSolarSystems" WHERE "solarSystemID"=$1 FOR SHARE"#,
                            &[&(current.solar_system_id as i32)]).await.map_err(failed)?.map(|r|(i64::from(r.get::<_,i32>(0)),i64::from(r.get::<_,i32>(1)),i64::from(r.get::<_,i32>(2))))
                    };
                    if place==Some((current.solar_system_id,current.constellation_id,current.region_id)) {return Ok(current);}
                }
            }
        }
        return Err("The arriving hull disappeared without a settled loss.".into());
    }
    let hull=hull.unwrap();
    if current.ship_id!=loss.ship_id || current.station_id!=station || hull.owner_id!=pilot || hull.location_id!=station
        || hull.type_id!=loss.ship_type || hull.flag!=FLAG_HANGAR || !hull.singleton || hull.quantity!=1 || hull.category_id!=6 {
        return Err("The arriving ship's custody changed before loss settlement.".into());
    }
    let accepted=station_place(txn.tx(),station).await.map_err(failed)?.ok_or("The arrival station no longer exists.")?;
    if current.solar_system_id!=accepted.0 || current.constellation_id!=accepted.1 || current.region_id!=accepted.2 {
        return Err("The arriving station does not match the character's location.".into());
    }
    lock_loss_contents(txn.tx(),loss).await.map_err(failed)?;
    let before=item_snapshot(txn.tx(),loss).await.map_err(failed)?;
    let report=report_text(txn.tx(),loss).await;
    let mut insurance=None;
    let mut can=None;
    let mut bounty_balance=None;
    let mut skill_loss=None;
    let (destination,destroyed)=if loss.capsule && !loss.unpiloted {
        let chosen=medical::home_station(txn.tx(),pilot).await.map_err(failed)?;
        let destination=chosen.unwrap_or(station);
        if station_place(txn.tx(),destination).await.map_err(failed)?.is_none() {return Err("The medical clone has no valid home station.".into());}
        if let Some(killer)=loss.final_blow.filter(|id|*id>=100_000_000 && *id!=pilot) {
            if pay_player_bounty(txn.tx(),pilot,killer).await.map_err(failed)?>0.0 {
                let balance=txn.tx().query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,
                    &[&(killer as i32)]).await.map_err(failed)?.get::<_,f64>(0);
                bounty_balance=Some((killer,balance));
            }
        }
        // A capsule should have no cargo, but malformed descendants must not
        // become orphaned items when the hull is deleted.
        txn.destroy_tree(loss.ship_id,Some(station),&[pilot]).await.map_err(failed)?;
        let implants=lose_implants(&mut txn,pilot).await.map_err(failed)?;
        skill_loss=lose_skill_points(&txn,pilot).await.map_err(failed)?;
        medical::replace_clone(txn.tx(),pilot,TYPE_CLONE_ALPHA,chosen).await.map_err(failed)?;
        (destination,implants)
    }else{
        let contents=txn.tx().query(r#"SELECT e."itemID",COALESCE(t."typeName",'') FROM entity e
            JOIN "invTypes" t ON t."typeID"=e."typeID" WHERE e."locationID"=$1 AND e."itemID"<>$2 ORDER BY e."itemID""#,
            &[&(loss.ship_id as i32),&(pilot as i32)]).await.map_err(failed)?;
        let mut rng=combat::Rng::seeded(combat::now_ms() as u64^loss.ship_id as u64);
        let mut survivors=Vec::new();let mut destroyed=Vec::new();
        for row in contents {
            let id=i64::from(row.get::<_,i32>(0));
            if rng.unit()<SURVIVING_SHARE {survivors.push(Stash::Existing(id));}
            else {destroy_tree(&mut txn,id,loss.ship_id,pilot).await.map_err(failed)?;destroyed.push(row.get::<_,String>(1));}
        }
        can=loot::stage_can(&mut txn,loss.system,loss.pos,pilot,&survivors).await.map_err(failed)?;
        txn.destroy(loss.ship_id).await.map_err(failed)?;
        crate::svc::clones::ship_destroyed(txn.tx(),loss.ship_id).await.map_err(failed)?;
        insurance=claim_insurance(txn.tx(),loss.ship_id,crate::bulkdata::filetime_now()).await.map_err(failed)?;
        (station,destroyed)
    };
    let (system,constellation,region)=station_place(txn.tx(),destination).await.map_err(failed)?.ok_or("The destination station disappeared.")?;
    let capsule=txn.create(NewItem {type_id:TYPE_CAPSULE,owner_id:pilot,location_id:destination,flag:FLAG_HANGAR,quantity:1,singleton:true}).await.map_err(failed)?;
    txn.tx().execute(r#"UPDATE "character_" SET "shipID"=$2,"stationID"=$3,"solarSystemID"=$4,"constellationID"=$5,"regionID"=$6 WHERE "characterID"=$1"#,
        &[&(pilot as i32),&(capsule as i32),&(destination as i32),&(system as i32),&(constellation as i32),&(region as i32)]).await.map_err(failed)?;
    txn.tx().execute(r#"UPDATE entity SET "locationID"=$2,x=0,y=0,z=0 WHERE "itemID"=$1"#,
        &[&(pilot as i32),&(destination as i32)]).await.map_err(failed)?;
    let blob=item_blob(txn.tx(),&before).await.map_err(failed)?;
    txn.tx().execute(r#"DELETE FROM "pendingStationArrivals" WHERE "shipID"=$1 AND "characterID"=$2"#,
        &[&(loss.ship_id as i32),&(pilot as i32)]).await.map_err(failed)?;
    let record=stage_record(txn.tx(),loss,report,destroyed,blob).await.map_err(failed)?;
    let events=txn.commit().await.map_err(failed)?;
    items::notify(&server.world,loss.client_id,events);
    if let Some(payout)=insurance {notify_insurance(server,loss,payout);}
    if let Some(can)=can {loot::show_can(&server.world,loss.system,can);}
    {
        let w=server.world.lock();
        if let Some((killer,balance))=bounty_balance {
            w.notify(&w.clients_of(killer),"OnAccountChange",Value::Tuple(vec![Value::bytes("cash"),Value::Int(killer),Value::Double(balance)]));
        }
        if let Some((skill,lost))=skill_loss {
            let text=format!("Your clone could not hold everything you knew: you have lost {} skill points of {skill}. Upgrade your clone at a station's medical bay to keep more.",crate::heartbeat::fmt_isk(lost));
            let (key,args)=crate::fight::notify(&text);
            w.notify(&[loss.client_id],"OnRemoteMessage",Value::Tuple(vec![Value::bytes(key),args]));
        }
    }
    deliver_record(server,db,record).await;
    Ok(ArrivalDisposition {ship_id:capsule,station_id:destination,solar_system_id:system,constellation_id:constellation,region_id:region})
}

async fn station_place(db:&impl GenericClient,station:i64)->Result<Option<(i64,i64,i64)>,tokio_postgres::Error> {
    Ok(db.query_opt(r#"SELECT "solarSystemID","constellationID","regionID" FROM "staStations" WHERE "stationID"=$1 FOR SHARE"#,
        &[&(station as i32)]).await?.map(|r|(i64::from(r.get::<_,i32>(0)),i64::from(r.get::<_,i32>(1)),i64::from(r.get::<_,i32>(2)))))
}

#[cfg(test)] mod tests;

