//! Space arrivals are provisional until their last source combat state is
//! durable. Receipts outlive hulls, so a delayed callback cannot replay a trip.
use crate::{ballpark::{Ship,Vec3},conn::Server,losses::Loss,world::World};
use tokio_postgres::{Client,GenericClient,Transaction};

mod storage;
#[cfg(test)] mod tests;
pub use storage::ensure;

#[derive(Debug,Clone,Copy,PartialEq,serde::Serialize,serde::Deserialize)]
pub struct Place {pub system:i64,pub station:Option<i64>,pub constellation:i64,pub region:i64,pub at:Vec3}
impl Place {
    fn valid(&self)->bool {id(self.system) && self.station.is_none_or(id) && id(self.constellation) && id(self.region) && self.at.is_finite()}
}
fn id(n:i64)->bool {n>0 && n<=i32::MAX as i64}

#[derive(Debug,Clone,Copy,PartialEq)]
pub struct Marker {pub generation:i64,pub pilot:i64,pub ship:i64,pub ship_type:i64,pub source:Place,pub destination:Place}
impl Marker {
    fn valid(&self)->bool {self.generation>0 && id(self.pilot) && id(self.ship) && id(self.ship_type)
        && self.source.valid() && self.destination.valid() && self.destination.station.is_none()}
}

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub enum Resolution {Accepted,Finalized,RolledBack,Unknown}

#[derive(Debug,Clone)]
pub enum Final {Healthy {ship:Ship,tick:i64},Lost(Loss)}

#[derive(Debug,Clone)]
pub struct Pending {marker:Marker,final_state:Option<Final>,busy:bool,claim:u64}

pub async fn begin(tx:&Transaction<'_>,pilot:i64,ship:i64,source:Place,destination:Place)->Result<Marker,tokio_postgres::Error> {
    storage::begin(tx,pilot,ship,source,destination).await
}

/// This read uses a fresh transaction in the standard custody lock order.
/// Unknown is deliberately not a refund or permission to load the old hull.
pub async fn resolve(db:&mut Client,marker:&Marker)->Result<Resolution,String> {
    if !marker.valid(){return Ok(Resolution::Unknown);}
    let tx=db.transaction().await.map_err(failed)?;
    Ok(storage::locked(&tx,*marker).await?.resolution)
}

pub async fn blocked(db:&impl GenericClient,ship:i64)->Result<bool,tokio_postgres::Error> {
    Ok(db.query_one(r#"SELECT EXISTS(SELECT 1 FROM "travelReceipts" WHERE "shipID"=$1 AND state=0)"#,&[&(ship as i32)]).await?.get(0))
}
pub async fn blocked_character(db:&impl GenericClient,pilot:i64)->Result<bool,tokio_postgres::Error> {
    Ok(db.query_one(r#"SELECT EXISTS(SELECT 1 FROM "travelReceipts" WHERE "characterID"=$1 AND state=0)"#,&[&(pilot as i32)]).await?.get(0))
}
pub fn blocked_in_memory(world:&World,ship:i64)->bool {
    world.lock().pending_travel.values().any(|p|p.marker.ship==ship)
}
pub fn blocked_character_in_memory(world:&World,pilot:i64)->bool {
    world.lock().pending_travel.values().any(|p|p.marker.pilot==pilot)
}

pub fn track_uncertain(world:&World,marker:&Marker)->Result<(),String> {
    if !marker.valid(){return Err("The travel receipt identity is invalid.".into());}
    let mut w=world.lock();
    if let Some(p)=w.pending_travel.get(&marker.generation) {
        return if p.marker==*marker {Ok(())}else{Err("This travel generation belongs to another trip.".into())};
    }
    if w.pending_travel.values().any(|p|p.marker.ship==marker.ship || p.marker.pilot==marker.pilot) {
        return Err("An earlier travel outcome is still unresolved.".into());
    }
    w.pending_travel.insert(marker.generation,Pending {marker:*marker,final_state:None,busy:false,claim:0});
    Ok(())
}

/// Called only after the owned departure runner proves rollback or takes over
/// accepted finalization. It cannot discard a recorded final snapshot.
pub fn clear_uncertain(world:&World,marker:&Marker) {
    let mut w=world.lock();
    if w.pending_travel.get(&marker.generation).is_some_and(|p|p.marker==*marker && p.final_state.is_none()) {
        w.pending_travel.remove(&marker.generation);
    }
}

fn valid_final(m:Marker,final_state:&Final)->bool {
    if !m.valid(){return false;}
    match final_state {
        Final::Healthy{ship,tick}=>ship.id==m.ship && ship.pilot.char_id==m.pilot && ship.pilot.type_id==m.ship_type
            && !ship.unpiloted && *tick>=0 && ship.pos==m.destination.at && !ship.warping() && ship.fight.as_ref().is_some_and(|f|
                f.vitals.hull_max.is_finite() && f.vitals.hull_max>0.0 && f.vitals.hull_damage.is_finite()
                && f.vitals.hull_damage>=0.0 && f.vitals.hull_damage<f.vitals.hull_max-1e-6),
        Final::Lost(loss)=>crate::losses::journal::valid(loss) && !loss.unpiloted && loss.char_id==m.pilot
            && loss.ship_id==m.ship && loss.ship_type==m.ship_type && loss.system==m.source.system
            && loss.arrival_custody.is_some_and(|c|c.system==m.destination.system && c.at==m.destination.at),
    }
}
fn decoded_loss(raw:&str,m:Marker)->Option<Loss> {
    if raw.len()>1_048_576{return None;}
    let loss:Loss=serde_json::from_str(raw).ok()?;
    valid_final(m,&Final::Lost(loss.clone())).then_some(loss)
}
fn failed(error:tokio_postgres::Error)->String {
    tracing::warn!("travel finalization remains pending: {}",crate::logging::chain(&error));
    "The travel outcome could not be confirmed and remains quarantined.".into()
}
fn next_claim()->u64 {
    static NEXT:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1,std::sync::atomic::Ordering::Relaxed)
}
struct Busy {world:World,generation:i64,claim:u64}
impl Drop for Busy {
    fn drop(&mut self) {
        if let Some(p)=self.world.lock().pending_travel.get_mut(&self.generation).filter(|p|p.claim==self.claim) {p.busy=false;}
    }
}

/// Store the known final snapshot before awaiting SQL. No success or retry
/// publishes a session move; the current connection or a later login does so.
pub async fn finalize(server:&Server,db:&mut Client,marker:Marker,final_state:Final)->Result<(),String> {
    if !valid_final(marker,&final_state){return Err("The final travel snapshot is invalid and cannot be accepted.".into());}
    let pending={
        let mut w=server.world.lock();
        let p=w.pending_travel.entry(marker.generation).or_insert_with(||Pending {
            marker,final_state:None,busy:false,claim:0});
        if p.marker!=marker || p.busy {return Err("This travel generation is already being finalized.".into());}
        if p.final_state.is_none(){p.final_state=Some(final_state);}
        p.busy=true;p.claim=next_claim();p.clone()
    };
    perform(server,db,pending).await
}

async fn perform(server:&Server,db:&mut Client,pending:Pending)->Result<(),String> {
    let _busy=Busy {world:server.world.clone(),generation:pending.marker.generation,claim:pending.claim};
    let result=attempt(db,&pending).await;
    if result.is_ok() {
        let mut w=server.world.lock();
        if w.pending_travel.get(&pending.marker.generation).is_some_and(|p|p.claim==pending.claim && p.marker==pending.marker) {
            w.pending_travel.remove(&pending.marker.generation);
        }
    }
    result
}

async fn attempt(db:&mut Client,pending:&Pending)->Result<(),String> {
    let m=pending.marker;
    let final_state=pending.final_state.as_ref().ok_or("The last source combat state is unknown; travel remains quarantined.")?;
    if let Final::Lost(loss)=final_state {
        // First retain the immutable confirmed loss. If the subsequent staging
        // transaction fails, restart can still recover this known outcome.
        let raw=serde_json::to_string(loss).map_err(|_|"The final travel loss could not be encoded.")?;
        if decoded_loss(&raw,m).is_none(){return Err("The final travel loss is invalid.".into());}
        let tx=db.transaction().await.map_err(failed)?;
        let locked=storage::locked(&tx,m).await?;
        match locked.resolution {
            Resolution::Finalized=>return Ok(()),
            Resolution::Accepted=>{},
            _=>return Err("The accepted travel custody could not be verified.".into()),
        }
        tx.execute(r#"UPDATE "travelReceipts" SET loss=COALESCE(loss,$2) WHERE generation=$1 AND state=0"#,
            &[&m.generation,&raw]).await.map_err(failed)?;
        tx.commit().await.map_err(failed)?;
    }
    let tx=db.transaction().await.map_err(failed)?;
    let locked=storage::locked(&tx,m).await?;
    match locked.resolution {
        Resolution::Finalized=>return Ok(()),
        Resolution::Accepted=>{},
        _=>return Err("The accepted travel custody could not be verified.".into()),
    }
    if let Some(raw)=locked.loss {
        let loss=decoded_loss(&raw,m).ok_or("The durable travel loss is corrupt and remains quarantined.")?;
        if !crate::losses::journal::stage(&tx,&loss).await.map_err(failed)? {
            return Err("The arriving loss could not be staged under its current custody.".into());
        }
    }else if let Final::Healthy{ship,tick}=final_state {
        if !crate::space_state::write_arrival(&tx,m.destination.system,ship,*tick).await.map_err(failed)? {
            return Err("The final arrival checkpoint could not be validated.".into());
        }
    }else{return Err("The final travel loss has no durable payload.".into());}
    tx.execute(r#"UPDATE "travelReceipts" SET state=1,"completedAt"=$2 WHERE generation=$1 AND state=0"#,
        &[&m.generation,&crate::combat::now_ms()]).await.map_err(failed)?;
    tx.commit().await.map_err(failed)?;
    Ok(())
}

pub async fn retry(server:&Server,db:&mut Client) {
    let mut keys:Vec<_>=server.world.lock().pending_travel.iter().filter(|(_,p)|p.final_state.is_some()).map(|(k,_)|*k).collect();
    keys.sort_unstable();
    for generation in keys {
        let pending={let mut w=server.world.lock();w.pending_travel.get_mut(&generation).filter(|p|!p.busy).map(|p|{
            p.busy=true;p.claim=next_claim();p.clone()})};
        if let Some(p)=pending {if let Err(error)=perform(server,db,p).await {tracing::warn!(generation,"travel remains pending: {error}");}}
    }
    let cursor=server.world.lock().travel_retry_cursor;
    let rows=match db.query(r#"SELECT generation,"characterID","shipID","shipTypeID",source,destination,loss FROM "travelReceipts"
        WHERE state=0 AND loss IS NOT NULL ORDER BY CASE WHEN generation>$1 THEN 0 ELSE 1 END,generation LIMIT 64"#,&[&cursor]).await {
        Ok(rows)=>rows,Err(error)=>{tracing::warn!("reading travel recovery receipts: {}",crate::logging::chain(&error));return;}
    };
    for row in rows {
        let generation:i64=row.get(0);server.world.lock().travel_retry_cursor=generation;
        if server.world.lock().pending_travel.contains_key(&generation){continue;}
        let Some(marker)=storage::marker(&row) else {tracing::warn!(generation,"invalid travel receipt remains quarantined");continue;};
        let raw:String=row.get(6);
        let Some(loss)=decoded_loss(&raw,marker) else {tracing::warn!(generation,"invalid travel loss remains quarantined");continue;};
        if let Err(error)=finalize(server,db,marker,Final::Lost(loss)).await {tracing::warn!(generation,"durable travel loss remains pending: {error}");}
    }
}
