//! A station arrival is provisional until its last live combat snapshot is
//! durably saved or its loss is settled. The marker commits with the move.
//! A restart with only that marker cannot infer damage during COMMIT; it stays
//! quarantined instead of inventing either survival or destruction.
use super::{ArrivalDisposition,Attacker,Loss};
use crate::{ballpark::{Ship,Vec3},combat,conn::Server,svc::inflight};
use tokio_postgres::{Client,GenericClient,Transaction};

#[cfg(test)]
mod tests;

#[derive(Debug,Clone,Copy)]
pub struct Marker {pub ship:i64,pub pilot:i64,pub station:i64,pub source:i64,pub generation:i64,pub ship_type:i64}

#[derive(Debug,Clone)]
pub struct Pending {marker:Marker,ship:Ship,busy:bool,claim:u64}

fn next_claim()->u64 {
    static CLAIM:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(1);
    CLAIM.fetch_add(1,std::sync::atomic::Ordering::Relaxed)
}
struct Busy {world:crate::world::World,marker:Marker,claim:u64}
impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(p)=self.world.lock().pending_station_arrivals.get_mut(&self.marker.ship)
            .filter(|p|p.marker.generation==self.marker.generation && p.claim==self.claim) {p.busy=false;}
    }
}

pub async fn ensure(db:&Client)->Result<(),tokio_postgres::Error> {
    db.batch_execute(r#"
        CREATE SEQUENCE IF NOT EXISTS station_arrival_generation;
        CREATE TABLE IF NOT EXISTS "pendingStationArrivals" (
            "shipID" integer PRIMARY KEY REFERENCES entity("itemID") ON DELETE CASCADE,
            "characterID" integer NOT NULL, "stationID" integer NOT NULL, "sourceSystemID" integer NOT NULL,
            "shipTypeID" integer NOT NULL, generation bigint NOT NULL DEFAULT nextval('station_arrival_generation'),
            "createdAt" bigint NOT NULL, loss text);
        CREATE INDEX IF NOT EXISTS "pendingStationArrivals_character" ON "pendingStationArrivals" ("characterID");
    "#).await
}

pub async fn blocked(db:&impl GenericClient,ship:i64)->Result<bool,tokio_postgres::Error> {
    Ok(blocked_except_travel(db,ship).await? || crate::travel::blocked(db,ship).await?)
}

pub(crate) async fn blocked_except_travel(db:&impl GenericClient,ship:i64)->Result<bool,tokio_postgres::Error> {
    Ok(blocked_arrival_or_loss(db,ship).await? || crate::svc::jumps::portal::blocked(db,ship).await?)
}

pub(crate) async fn blocked_except_portal(db:&impl GenericClient,ship:i64)->Result<bool,tokio_postgres::Error> {
    Ok(blocked_arrival_or_loss(db,ship).await? || crate::travel::blocked(db,ship).await?)
}

async fn blocked_arrival_or_loss(db:&impl GenericClient,ship:i64)->Result<bool,tokio_postgres::Error> {
    Ok(blocked_arrival_or_loss_only(db,ship).await? || crate::conn::logout::blocked(db,ship).await?)
}

pub(crate) async fn blocked_except_logout(db:&impl GenericClient,ship:i64)->Result<bool,tokio_postgres::Error> {
    Ok(blocked_arrival_or_loss_only(db,ship).await? || crate::travel::blocked(db,ship).await?
        || crate::svc::jumps::portal::blocked(db,ship).await?)
}

async fn blocked_arrival_or_loss_only(db:&impl GenericClient,ship:i64)->Result<bool,tokio_postgres::Error> {
    Ok(db.query_one(r#"SELECT EXISTS(SELECT 1 FROM "pendingStationArrivals" WHERE "shipID"=$1)
        OR EXISTS(SELECT 1 FROM "pendingShipLosses" WHERE "shipID"=$1)"#,&[&(ship as i32)]).await?.get(0))
}

pub async fn blocked_character(db:&impl GenericClient,pilot:i64)->Result<bool,tokio_postgres::Error> {
    Ok(db.query_one(r#"SELECT EXISTS(SELECT 1 FROM "pendingStationArrivals" WHERE "characterID"=$1)
        OR EXISTS(SELECT 1 FROM "pendingShipLosses" WHERE "characterID"=$1 AND occupied)"#,&[&(pilot as i32)]).await?.get::<_,bool>(0)
        || crate::travel::blocked_character(db,pilot).await?
        || crate::conn::logout::blocked_character(db,pilot).await?
        || crate::svc::jumps::portal::blocked_character(db,pilot).await?)
}

/// Call in the arrival transaction after locking and moving the current hull.
/// A duplicate unresolved arrival is a constraint failure, not a replacement.
pub async fn begin(tx:&Transaction<'_>,pilot:i64,ship:i64,station:i64,source:i64)->Result<Marker,tokio_postgres::Error> {
    let row=tx.query_one(r#"INSERT INTO "pendingStationArrivals" ("shipID","characterID","stationID","sourceSystemID","shipTypeID","createdAt")
        SELECT e."itemID",$2,$3,$4,e."typeID",$5 FROM entity e
        WHERE e."itemID"=$1 AND e."ownerID"=$2 AND e."locationID"=$3 AND e.flag=4 AND e.quantity=1 AND e.singleton<>0
        RETURNING generation,"shipTypeID""#,&[&(ship as i32),&(pilot as i32),&(station as i32),&(source as i32),&combat::now_ms()]).await?;
    Ok(Marker {ship,pilot,station,source,generation:row.get(0),ship_type:i64::from(row.get::<_,i32>(1))})
}

fn failed(error:tokio_postgres::Error)->String {
    tracing::warn!("final station snapshot is pending: {}",crate::logging::chain(&error));
    "The station arrival is awaiting its final combat state. Please try again shortly.".into()
}

/// Retains the detached final snapshot before any fallible database operation.
/// The owned transition task awaits this; a failed caller must close its stale
/// session while the marker and heartbeat finish reconciliation.
pub async fn finish(server:&Server,db:&mut Client,marker:Marker,ship:Ship)->Result<ArrivalDisposition,String> {
    if ship.id!=marker.ship || ship.pilot.char_id!=marker.pilot || ship.pilot.type_id!=marker.ship_type {
        return Err("The final station snapshot does not match its arrival.".into());
    }
    let pending={
        let mut world=server.world.lock();
        if let Some(existing)=world.pending_station_arrivals.get_mut(&marker.ship) {
            if existing.marker.generation!=marker.generation || existing.busy {return Err("This station arrival is already being finalized.".into());}
            existing.busy=true;existing.claim=next_claim();existing.clone()
        }else{
            let pending=Pending {marker,ship,busy:true,claim:next_claim()};world.pending_station_arrivals.insert(marker.ship,pending.clone());pending
        }
    };
    let _busy=Busy {world:server.world.clone(),marker,claim:pending.claim};
    let result=attempt(server,db,&pending).await;
    complete_memory(server,&pending,result.is_ok());
    result
}

fn complete_memory(server:&Server,pending:&Pending,success:bool) {
    if !success{return;} // Only the RAII owner releases a failed attempt.
    let mut world=server.world.lock();
    if world.pending_station_arrivals.get(&pending.marker.ship).is_some_and(|p|p.marker.generation==pending.marker.generation && p.claim==pending.claim) {
        world.pending_station_arrivals.remove(&pending.marker.ship);
    }
}

async fn locked(tx:&Transaction<'_>,m:Marker)->Result<bool,String> {
    if !crate::svc::login::lock_character_lifecycle(tx,m.pilot as i32).await.map_err(failed)? {return Err("The arriving character is unavailable.".into());}
    if tx.query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2 AND "stationID"=$3 FOR UPDATE"#,
        &[&(m.pilot as i32),&(m.ship as i32),&(m.station as i32)]).await.map_err(failed)?.is_none() {return Ok(false);}
    if tx.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1 AND "ownerID"=$2 AND "locationID"=$3 AND "typeID"=$4
        AND flag=4 AND quantity=1 AND singleton<>0 FOR UPDATE"#,&[&(m.ship as i32),&(m.pilot as i32),&(m.station as i32),&(m.ship_type as i32)]).await.map_err(failed)?.is_none(){return Ok(false);}
    Ok(tx.query_opt(r#"SELECT 1 FROM "pendingStationArrivals" WHERE "shipID"=$1 AND "characterID"=$2 AND "stationID"=$3 AND generation=$4 FOR UPDATE"#,
        &[&(m.ship as i32),&(m.pilot as i32),&(m.station as i32),&m.generation]).await.map_err(failed)?.is_some())
}

async fn disposition(db:&impl GenericClient,pilot:i64)->Result<ArrivalDisposition,String> {
    let row=db.query_one(r#"SELECT "shipID",COALESCE("stationID",0),"solarSystemID","constellationID","regionID"
        FROM "character_" WHERE "characterID"=$1"#,&[&(pilot as i32)]).await.map_err(failed)?;
    Ok(ArrivalDisposition {ship_id:i64::from(row.get::<_,i32>(0)),station_id:i64::from(row.get::<_,i32>(1)),
        solar_system_id:i64::from(row.get::<_,i32>(2)),constellation_id:i64::from(row.get::<_,i32>(3)),region_id:i64::from(row.get::<_,i32>(4))})
}

async fn attempt(server:&Server,db:&mut Client,p:&Pending)->Result<ArrivalDisposition,String> {
    let m=p.marker;
    let fighter=p.ship.fight.as_ref().ok_or("The final station snapshot has no combat state.")?;
    if !fighter.vitals.hull_max.is_finite() || fighter.vitals.hull_max<=0.0 || !fighter.vitals.hull_damage.is_finite() {
        return Err("The final station snapshot has invalid hull damage.".into());
    }
    let lethal=fighter.vitals.hull_damage>=fighter.vitals.hull_max-1e-6;
    if lethal {
        let mut loss=Loss::of(&p.ship,m.source,p.ship.pilot.type_id==670);
        let raw=serde_json::to_string(&StoredLoss::from(&loss)).map_err(|_|"The final loss could not be recorded.".to_string())?;
        if decode(&raw,m).is_none(){return Err("The final loss snapshot is invalid and remains quarantined.".into());}
        let tx=db.transaction().await.map_err(failed)?;
        if locked(&tx,m).await? {
            let saved:String=tx.query_one(r#"UPDATE "pendingStationArrivals" SET loss=COALESCE(loss,$3) WHERE "shipID"=$1 AND generation=$2 RETURNING loss"#,
                &[&(m.ship as i32),&m.generation,&raw]).await.map_err(failed)?.get(0);
            loss=decode(&saved,m).ok_or("The recorded loss is invalid and remains quarantined.")?;
            loss.client_id=server.world.lock().clients_of(m.pilot).first().copied().unwrap_or(0);
            tx.commit().await.map_err(failed)?;
        }else{
            let done=tx.query_opt(r#"SELECT 1 FROM "killLog" WHERE "victimItemID"=$1 AND "victimCharID"=$2 LIMIT 1"#,&[&(m.ship as i32),&(m.pilot as i32)]).await.map_err(failed)?.is_some();
            if !done{return Err("The pending arrival's custody changed before loss settlement.".into());}
            drop(tx);
        }
        return super::docked::settle_loss(server,db,m.station,&loss).await;
    }
    let tx=db.transaction().await.map_err(failed)?;
    if !locked(&tx,m).await? {
        if blocked(&tx,m.ship).await.map_err(failed)? {return Err("The pending arrival changed before its final snapshot was saved.".into());}
        // A successful but unacknowledged final COMMIT is already complete.
        return disposition(&tx,m.pilot).await;
    }
    let recorded:Option<String>=tx.query_one(r#"SELECT loss FROM "pendingStationArrivals" WHERE "shipID"=$1 AND generation=$2"#,
        &[&(m.ship as i32),&m.generation]).await.map_err(failed)?.get(0);
    if let Some(raw)=recorded {
        let mut loss=decode(&raw,m).ok_or("The recorded loss is invalid and remains quarantined.")?;
        loss.client_id=server.world.lock().clients_of(m.pilot).first().copied().unwrap_or(0);
        drop(tx);
        return super::docked::settle_loss(server,db,m.station,&loss).await;
    }
    inflight::write_vitals(&tx,m.ship,fighter,true).await.map_err(failed)?;
    crate::cloak::durability::save_history(&tx,&p.ship,combat::now_ms()).await.map_err(failed)?;
    let result=disposition(&tx,m.pilot).await?;
    tx.execute(r#"DELETE FROM "pendingStationArrivals" WHERE "shipID"=$1 AND generation=$2"#,&[&(m.ship as i32),&m.generation]).await.map_err(failed)?;
    tx.commit().await.map_err(failed)?;
    Ok(result)
}

pub async fn retry(server:&Server,db:&mut Client) {
    // The failed transition closes its old connection. Its successful caller,
    // or a later login, publishes the authoritative session. A retry may find
    // that the pilot has already boarded or undocked after an acknowledged
    // settlement, so it must never push a captured relocation to that session.
    let pending:Vec<_>=server.world.lock().pending_station_arrivals.keys().copied().collect();
    for id in pending {
        let Some(p)=({let mut world=server.world.lock();world.pending_station_arrivals.get_mut(&id).filter(|p|!p.busy).map(|p|{p.busy=true;p.claim=next_claim();p.clone()})}) else {continue;};
        let _busy=Busy {world:server.world.clone(),marker:p.marker,claim:p.claim};
        let result=attempt(server,db,&p).await;complete_memory(server,&p,result.is_ok());
        if let Err(error)=result {tracing::warn!(ship=p.marker.ship,"station arrival remains pending: {error}");}
    }
    let cursor=server.world.lock().pending_station_arrival_cursor;
    let rows=match db.query(r#"SELECT "shipID","characterID","stationID","sourceSystemID","shipTypeID",generation,loss
        FROM "pendingStationArrivals" WHERE loss IS NOT NULL AND "shipID">$1 ORDER BY "shipID" LIMIT 64"#,&[&cursor]).await {
        Ok(rows)=>rows,Err(error)=>{tracing::warn!("reading pending station losses: {}",crate::logging::chain(&error));return;}
    };
    server.world.lock().pending_station_arrival_cursor=rows.last().map(|row|row.get::<_,i32>(0)).unwrap_or(0);
    for row in rows {
        let m=Marker {ship:i64::from(row.get::<_,i32>(0)),pilot:i64::from(row.get::<_,i32>(1)),station:i64::from(row.get::<_,i32>(2)),
            source:i64::from(row.get::<_,i32>(3)),ship_type:i64::from(row.get::<_,i32>(4)),generation:row.get(5)};
        if server.world.lock().pending_station_arrivals.contains_key(&m.ship){continue;}
        let raw:String=row.get(6);
        let Some(mut loss)=decode(&raw,m) else {tracing::warn!(ship=m.ship,"invalid pending loss remains quarantined");continue;};
        loss.client_id=server.world.lock().clients_of(m.pilot).first().copied().unwrap_or(0);
        if let Err(error)=super::docked::settle_loss(server,db,m.station,&loss).await {
            tracing::warn!(ship=m.ship,"durable station loss remains pending: {error}");
        }
    }
}

#[derive(serde::Serialize,serde::Deserialize)]
struct StoredAttacker {who:i64,ship_type:i64,weapon_type:i64,damage:f64}
#[derive(serde::Serialize,serde::Deserialize)]
struct StoredLoss {char_id:i64,ship_id:i64,ship_type:i64,system:i64,pos:Vec3,capsule:bool,unpiloted:bool,
    final_blow:Option<i64>,attackers:Vec<StoredAttacker>,items:Vec<(i64,i64)>}
impl From<&Loss> for StoredLoss {
    fn from(l:&Loss)->Self {Self {char_id:l.char_id,ship_id:l.ship_id,ship_type:l.ship_type,system:l.system,pos:l.pos,capsule:l.capsule,
        unpiloted:l.unpiloted,final_blow:l.final_blow,attackers:l.attackers.iter().map(|a|StoredAttacker {who:a.who,ship_type:a.ship_type,weapon_type:a.weapon_type,damage:a.damage}).collect(),items:l.items.clone()}}
}
fn decode(raw:&str,m:Marker)->Option<Loss> {
    if raw.len()>1_048_576{return None;}
    let l:StoredLoss=serde_json::from_str(raw).ok()?;
    if l.char_id!=m.pilot || l.ship_id!=m.ship || l.ship_type!=m.ship_type || l.system!=m.source || l.unpiloted
        || l.capsule!=(l.ship_type==670) || !l.pos.is_finite() || l.attackers.iter().any(|a|!a.damage.is_finite() || a.damage<0.0) {return None;}
    Some(Loss {client_id:0,char_id:l.char_id,ship_id:l.ship_id,ship_type:l.ship_type,system:l.system,pos:l.pos,arrival_custody:None,capsule:l.capsule,
        unpiloted:l.unpiloted,final_blow:l.final_blow,attackers:l.attackers.into_iter().map(|a|Attacker {who:a.who,ship_type:a.ship_type,weapon_type:a.weapon_type,damage:a.damage}).collect(),items:l.items})
}
