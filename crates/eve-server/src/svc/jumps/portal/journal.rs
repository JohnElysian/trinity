//! Portal payment is a durable operation, not a ship relocation. A receipt
//! never replaces a live ship with an earlier snapshot. Unknown final combat
//! state after process loss remains quarantined; no offline timer is invented.
use super::*;
use tokio_postgres::{Client,GenericClient,Transaction};
use crate::{conn::Server,losses::Loss};

#[cfg(test)] mod tests;

#[derive(Debug,Clone,PartialEq,serde::Serialize,serde::Deserialize)]
struct Intent {
    pilot:i64,ship:i64,ship_type:i64,system:i64,member:Option<i64>,beacon:i64,to:i64,alliance:Option<i64>,
    quote:Quote,started:i64,expires:i64,boot:String,
}
#[derive(Debug,Clone,PartialEq)]
pub(super) struct Receipt {pub generation:i64,intent:Intent}
#[derive(Debug,Clone)]
pub struct Pending {receipt:Receipt,client:i64,before:Charged,reserved:Charged,loss:Option<Loss>,busy:u64}
impl Pending {
    pub(super) fn owns(&self,ship:i64,pilot:i64,client:i64)->bool {self.receipt.intent.ship==ship && self.receipt.intent.pilot==pilot && self.client==client}
    pub(super) fn owned(receipt:Receipt,client:i64,before:Charged,reserved:Charged)->Self {
        Self {receipt,client,before,reserved,loss:None,busy:claim()}
    }
}
#[derive(Debug,Clone,Copy,PartialEq)] enum Resolution {Accepted,Ready,Closed,Superseded,RolledBack,Unknown}
struct Locked {resolution:Resolution,custody:bool,module:bool,loss:Option<Loss>}
fn claim()->u64 {static NEXT:std::sync::atomic::AtomicU64=std::sync::atomic::AtomicU64::new(1);NEXT.fetch_add(1,std::sync::atomic::Ordering::Relaxed)}
fn boot()->&'static str {
    static BOOT:std::sync::OnceLock<String>=std::sync::OnceLock::new();
    BOOT.get_or_init(||format!("{}-{}",std::process::id(),std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()))
}
fn valid(r:&Receipt)->bool {
    let i=&r.intent;let id=|v:i64|v>0 && v<=i32::MAX as i64;
    r.generation>0 && [i.pilot,i.ship,i.ship_type,i.system,i.beacon,i.to,i.quote.generator_id,i.quote.generator_type].into_iter().all(id)
        && i.member.is_none_or(id) && i.alliance.is_none_or(id) && i.system!=i.to && i.started>0
        && i.expires.checked_sub(i.started)==Some(i.quote.duration) && i.quote.duration>0 && i.quote.duration<=i32::MAX as i64
        && i.quote.capacitor.is_finite() && i.quote.capacitor>=0.0 && i.quote.fuel>=0 && i.quote.fuel<=i32::MAX as i64
        && (i.quote.fuel==0 || id(i.quote.fuel_type)) && i.quote.range.is_finite() && i.quote.range>0.0
        && i.quote.mass_factor.is_finite() && i.quote.mass_factor>0.0 && [0,2].contains(&i.quote.harmonics) && i.boot.len()<=128
}
fn loss_valid(loss:&Loss,r:&Receipt)->bool {
    let i=&r.intent;crate::losses::journal::valid(loss) && !loss.unpiloted && loss.arrival_custody.is_none()
        && (loss.char_id,loss.ship_id,loss.ship_type,loss.system)==(i.pilot,i.ship,i.ship_type,i.system)
}
fn decode(row:&tokio_postgres::Row)->Option<Receipt> {
    let raw:&str=row.get("intent");if raw.len()>16384{return None;}
    let receipt=Receipt {generation:row.get("generation"),intent:serde_json::from_str(raw).ok()?};
    (valid(&receipt) && receipt.intent.pilot==i64::from(row.get::<_,i32>("characterID"))
        && receipt.intent.ship==i64::from(row.get::<_,i32>("shipID"))).then_some(receipt)
}
pub async fn ensure(db:&Client)->Result<(),tokio_postgres::Error> {
    db.batch_execute(r#"CREATE TABLE IF NOT EXISTS "portalReceipts" (
        generation bigserial PRIMARY KEY,"characterID" integer NOT NULL,"shipID" integer NOT NULL,intent text NOT NULL,
        state smallint NOT NULL DEFAULT 0 CHECK(state IN(0,1,2)),"completedAt" bigint,loss text);
        CREATE UNIQUE INDEX IF NOT EXISTS "portalReceipts_active_ship" ON "portalReceipts"("shipID") WHERE state IN(0,1);
        CREATE INDEX IF NOT EXISTS "portalReceipts_pending_character" ON "portalReceipts"("characterID") WHERE state=0;"#).await
}
pub async fn blocked(db:&impl GenericClient,ship:i64)->Result<bool,tokio_postgres::Error> {
    Ok(db.query_one(r#"SELECT EXISTS(SELECT 1 FROM "portalReceipts" WHERE "shipID"=$1 AND state=0)"#,&[&(ship as i32)]).await?.get(0))
}
pub async fn blocked_character(db:&impl GenericClient,pilot:i64)->Result<bool,tokio_postgres::Error> {
    Ok(db.query_one(r#"SELECT EXISTS(SELECT 1 FROM "portalReceipts" WHERE "characterID"=$1 AND state=0)"#,&[&(pilot as i32)]).await?.get(0))
}
pub fn blocked_in_memory(world:&World,ship:i64)->bool {world.lock().pending_portals.values().any(|p|p.receipt.intent.ship==ship)}
pub fn blocked_character_in_memory(world:&World,pilot:i64)->bool {world.lock().pending_portals.values().any(|p|p.receipt.intent.pilot==pilot)}

pub(super) async fn begin(tx:&Transaction<'_>,r:&Request,quote:&Quote,ship_type:i64)->Refused<Receipt> {
    let started=combat::now_ms();let intent=Intent {pilot:r.pilot,ship:r.ship,ship_type,system:r.from,member:r.member,beacon:r.beacon,to:r.to,
        alliance:r.alliance,quote:quote.clone(),started,expires:started.checked_add(quote.duration).ok_or("Invalid portal duration.")?,boot:boot().into()};
    let raw=serde_json::to_string(&intent).map_err(|_|"Invalid portal receipt.")?;
    let generation=tx.query_one(r#"INSERT INTO "portalReceipts"("characterID","shipID",intent) VALUES($1,$2,$3) RETURNING generation"#,
        &[&(r.pilot as i32),&(r.ship as i32),&raw]).await.map_err(database)?.get(0);
    let receipt=Receipt {generation,intent};if !valid(&receipt){return Err("Invalid portal receipt.".into());}Ok(receipt)
}

/// The caller already holds lifecycle, character and hull custody. A runtime
/// closure may precede its asynchronous durable closure; retire only that row.
pub(super) async fn retire_ready(tx:&Transaction<'_>,world:&World,ship:i64)->Refused<bool> {
    let rows=tx.query(r#"SELECT generation,state FROM "portalReceipts" WHERE "shipID"=$1 AND state IN(0,1) FOR UPDATE"#,&[&(ship as i32)]).await.map_err(database)?;
    for row in rows {
        let generation:i64=row.get(0);let state:i16=row.get(1);
        let live={let w=world.lock();w.pending_portals.contains_key(&generation) || w.bridges.get(&ship).is_some_and(|b|
            b.generation==generation && b.until_ms>combat::now_ms() && bridge_live(&w,ship,b))};
        if state==0 || live {return Ok(false);}
        close(tx,generation).await?;
    }Ok(true)
}
async fn close(tx:&Transaction<'_>,generation:i64)->Refused<()> {
    tx.execute(r#"UPDATE "portalReceipts" SET state=2,"completedAt"=$2 WHERE generation=$1 AND state IN(0,1)"#,
        &[&generation,&combat::now_ms()]).await.map_err(database)?;Ok(())
}

async fn locked(tx:&Transaction<'_>,r:&Receipt)->Refused<Locked> {
    if !valid(r){return Err("Invalid portal receipt remains quarantined.".into());}
    let i=&r.intent;
    let available=crate::svc::login::lock_character_lifecycle(tx,i.pilot as i32).await.map_err(database)?;
    let pilot=tx.query_opt(r#"SELECT "shipID","solarSystemID",COALESCE("stationID",0) FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#,&[&(i.pilot as i32)]).await.map_err(database)?;
    let hull=tx.query_opt(r#"SELECT "typeID","ownerID","locationID",flag,quantity,singleton FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(i.ship as i32)]).await.map_err(database)?;
    let module=tx.query_opt(r#"SELECT "typeID","ownerID","locationID",flag,quantity,singleton FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(i.quote.generator_id as i32)]).await.map_err(database)?;
    let online=tx.query_opt(r#"SELECT COALESCE("valueInt"::float8,"valueFloat",0) FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=2 FOR UPDATE"#,&[&(i.quote.generator_id as i32)]).await.map_err(database)?;
    let custody=available && pilot.as_ref().is_some_and(|p|p.get::<_,Option<i32>>(0)==Some(i.ship as i32) && p.get::<_,Option<i32>>(1)==Some(i.system as i32) && p.get::<_,i32>(2)==0)
        && hull.as_ref().is_some_and(|h|h.get::<_,Option<i32>>(0)==Some(i.ship_type as i32) && h.get::<_,Option<i32>>(1)==Some(i.pilot as i32)
            && h.get::<_,Option<i32>>(2)==Some(i.system as i32) && h.get::<_,Option<i32>>(3)==Some(0) && h.get::<_,Option<i32>>(4)==Some(1) && h.get::<_,Option<i32>>(5).is_some_and(|v|v!=0));
    let module=module.as_ref().is_some_and(|m|m.get::<_,Option<i32>>(0)==Some(i.quote.generator_type as i32)
        && m.get::<_,Option<i32>>(1)==Some(i.pilot as i32) && m.get::<_,Option<i32>>(2)==Some(i.ship as i32)
        && m.get::<_,Option<i32>>(3).is_some_and(|v|(11..=35).contains(&v)) && m.get::<_,Option<i32>>(4)==Some(1) && m.get::<_,Option<i32>>(5).is_some_and(|v|v!=0))
        && online.is_some_and(|v|v.get::<_,f64>(0)>0.0);
    let row=tx.query_opt(r#"SELECT generation,"characterID","shipID",intent,state,loss FROM "portalReceipts" WHERE generation=$1 FOR UPDATE"#,&[&r.generation]).await.map_err(database)?;
    let newer=tx.query_one(r#"SELECT EXISTS(SELECT 1 FROM "portalReceipts" WHERE generation>$1 AND ("shipID"=$2 OR "characterID"=$3))"#,
        &[&r.generation,&(i.ship as i32),&(i.pilot as i32)]).await.map_err(database)?.get::<_,bool>(0);
    let Some(row)=row else {
        return Ok(Locked {resolution:if custody && !newer {Resolution::RolledBack}else{Resolution::Unknown},custody,module,loss:None});
    };
    if decode(&row).as_ref()!=Some(r){return Err("Portal receipt identity mismatch remains quarantined.".into());}
    let loss=match row.get::<_,Option<String>>("loss") {
        Some(raw)=>{let loss:Loss=if raw.len()<=1048576 {serde_json::from_str(&raw).map_err(|_|"Corrupt portal loss remains quarantined.")?}else{return Err("Oversized portal loss remains quarantined.".into());};
            if !loss_valid(&loss,r){return Err("Invalid portal loss remains quarantined.".into());}Some(loss)},None=>None};
    let resolution=match row.get::<_,i16>("state") {0 if !newer=>Resolution::Accepted,1 if !newer=>Resolution::Ready,2 if newer=>Resolution::Superseded,2=>Resolution::Closed,_=>Resolution::Unknown};
    Ok(Locked {resolution,custody,module,loss})
}
fn request(r:&Receipt,world:&World,client:i64)->Request {let i=&r.intent;Request {world:world.clone(),url:String::new(),pilot:i.pilot,client,ship:i.ship,from:i.system,
    member:i.member,beacon:i.beacon,to:i.to,alliance:i.alliance,bonuses:BTreeMap::new()}}
fn bridge(r:&Receipt)->crate::world::Bridge {let i=&r.intent;crate::world::Bridge {generation:r.generation,started_ms:i.started,titan_pilot:i.pilot,
    generator_id:i.quote.generator_id,system:i.system,to_system:i.to,beacon:i.beacon,member:i.member,until_ms:i.expires,mass_factor:i.quote.mass_factor,harmonics:i.quote.harmonics}}
async fn fixed(tx:&Transaction<'_>,i:&Intent)->Refused<bool> {
    if i.member.is_some(){return Ok(true);}
    Ok(tx.query_opt(r#"SELECT 1 FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID" JOIN "posStructures" p ON p."itemID"=e."itemID"
        JOIN corporation dest ON dest."corporationID"=e."ownerID" JOIN "character_" c ON c."characterID"=$4 JOIN corporation source ON source."corporationID"=c."corporationID"
        WHERE e."itemID"=$1 AND e."locationID"=$2 AND t."groupID"=838 AND p.state IN(4,5) AND dest."allianceID"=$3 AND source."allianceID"=$3
        FOR SHARE OF e,p,dest,source"#,&[&(i.beacon as i32),&(i.to as i32),&(i.alliance.unwrap_or(0) as i32),&(i.pilot as i32)]).await.map_err(database)?.is_some())
}
fn remember_loss(world:&World,r:&Receipt,loss:Loss) {
    let mut w=world.lock();if let Some(p)=w.pending_portals.get_mut(&r.generation).filter(|p|p.receipt==*r) {if p.loss.is_none(){p.loss=Some(loss);}}
    // The portal marker already quarantines this source. Do not expose the
    // loss to independent settlement before the portal closure commits.
}
fn handoff_loss(world:&World,loss:Loss) {world.lock().pending_ship_losses.entry(loss.ship_id).or_insert(loss);}
async fn settled_source(tx:&Transaction<'_>,r:&Receipt)->Refused<bool> {
    let i=&r.intent;
    Ok(tx.query_one(r#"SELECT NOT EXISTS(SELECT 1 FROM entity WHERE "itemID"=$1)
        AND EXISTS(SELECT 1 FROM "killLog" WHERE "victimItemID"=$1 AND "victimCharID"=$2 AND "victimShipTypeID"=$3 AND "solarSystemID"=$4)"#,
        &[&(i.ship as i32),&(i.pilot as i32),&(i.ship_type as i32),&(i.system as i32)]).await.map_err(database)?.get(0))
}
fn forget(world:&World,r:&Receipt) {let mut w=world.lock();if w.pending_portals.get(&r.generation).is_some_and(|p|p.receipt==*r) {w.pending_portals.remove(&r.generation);}}
fn release(world:&World,r:&Receipt,refund:bool,closed:bool) {
    let mut w=world.lock();let Some(p)=w.pending_portals.remove(&r.generation) else{return;};
    if p.receipt!=*r {w.pending_portals.insert(r.generation,p);return;}
    let i=&r.intent;
    // Historical completion is never authority over a newer same-pilot,
    // same-client reservation. Retire only our own memory obligation.
    if w.pending_portals.values().any(|p|p.receipt.intent.ship==i.ship || p.receipt.intent.pilot==i.pilot)
        || w.bridges.get(&i.ship).is_some_and(|b|b.generation>r.generation)
        || w.portal_cycles.get(&i.ship).is_some_and(|b|b.generation>r.generation) {return;}
    w.pending_bridges.remove(&i.ship);
    if closed && w.bridges.get(&i.ship).is_some_and(|b|b.generation==r.generation) {
        w.bridges.remove(&i.ship);announce(&w,&request(r,world,p.client),false);
    }
    // A revoked route does not prove the module's accepted timed cycle ended.
    // This lifecycle is separate from traveler access and keeps original906
    // restrictions until the already-recorded deadline, with no renewed grace.
    if !refund && i.expires>combat::now_ms() && w.parks.get(&i.system).and_then(|p|p.ships.get(&i.ship))
        .is_some_and(|s|s.pilot.char_id==i.pilot && s.client_id==p.client && !s.unpiloted && s.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage<f.vitals.hull_max-1e-6)) {
        if w.portal_cycles.get(&i.ship).is_none_or(|cycle|cycle.generation<=r.generation){w.portal_cycles.insert(i.ship,bridge(r));}
    }
    let mut amount=None;
    if let Some(s)=w.parks.get_mut(&i.system).and_then(|p|p.ships.get_mut(&i.ship)).filter(|s|s.pilot.char_id==i.pilot && s.client_id==p.client && s.identity_reserved) {
        if let Some(f)=&mut s.fight {if refund {if f.vitals.capacitor==p.reserved {f.vitals.capacitor=p.before;}else{f.vitals.capacitor.add(combat::now_ms(),i.quote.capacitor);}}
            amount=Some(f.vitals.capacitor.at(combat::now_ms()));}s.identity_reserved=false;
    }
    if let Some(amount)=amount {w.notify(&[p.client],"OnModuleAttributeChanges",Value::Tuple(vec![Value::List(vec![Value::Tuple(vec![
        Value::bytes("OnModuleAttributeChange"),Value::Int(i.pilot),Value::Int(i.ship),Value::Int(combat::attr::CHARGE as i64),
        Value::Long(combat::filetime(combat::now_ms())),Value::Double(amount),Value::Double(amount)])]) ]));}
}
pub(super) struct Busy {world:World,generation:i64,claim:u64}
impl Drop for Busy {fn drop(&mut self){if let Some(p)=self.world.lock().pending_portals.get_mut(&self.generation).filter(|p|p.busy==self.claim){p.busy=0;}}}
pub(super) fn owner_guard(world:&World,generation:i64)->Busy {Busy {world:world.clone(),generation,claim:world.lock().pending_portals[&generation].busy}}

/// Resolve only, never replay payment. The source remains live: each failed
/// final write is retried from current combat state, not a retained healthy copy.
async fn attempt(world:&World,db:&mut Client,pending:&Pending)->Refused<Resolution> {
    let r=&pending.receipt;let i=&r.intent;let req=request(r,world,pending.client);
    let tx=db.transaction().await.map_err(database)?;let state=locked(&tx,r).await?;
    match state.resolution {Resolution::RolledBack=>{return Ok(Resolution::RolledBack)},Resolution::Superseded=>return Ok(Resolution::Superseded),Resolution::Unknown=>return Err("Portal payment outcome remains unknown.".into()),_=>{}}
    if state.resolution==Resolution::Closed && (state.loss.is_some() || !state.custody) {
        if state.custody {if let Some(loss)=state.loss {handoff_loss(world,loss);}}return Ok(Resolution::Closed);
    }
    if !state.custody {
        if settled_source(&tx,r).await? {close(&tx,r.generation).await?;tx.commit().await.map_err(database)?;return Ok(Resolution::Closed);}
        return Err("Portal source custody changed; outcome remains quarantined.".into());
    }
    let live={let w=world.lock();w.parks.get(&i.system).and_then(|p|p.ships.get(&i.ship).filter(|s|
        s.pilot.char_id==i.pilot && s.pilot.type_id==i.ship_type && !s.unpiloted && s.client_id==pending.client && s.identity_reserved).map(|s|(s.clone(),p.tick)))};
    let loss=state.loss.or_else(||pending.loss.clone()).or_else(||live.as_ref().and_then(|(s,_)|s.fight.as_ref().filter(|f|
        f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6).map(|_|Loss::of(s,i.system,s.pilot.type_id==670))));
    if let Some(loss)=loss {
        if !loss_valid(&loss,r){return Err("Invalid portal source loss.".into());}remember_loss(world,r,loss.clone());
        // Retain known lethality first, so failure of the subsequent loss stage
        // cannot turn a restart into an invented healthy source.
        let raw=serde_json::to_string(&loss).map_err(|_|"Could not encode portal source loss.")?;
        tx.execute(r#"UPDATE "portalReceipts" SET loss=COALESCE(loss,$2),state=0 WHERE generation=$1"#,&[&r.generation,&raw]).await.map_err(database)?;
        tx.commit().await.map_err(database)?;
        let tx=db.transaction().await.map_err(database)?;let l=locked(&tx,r).await?;
        if l.resolution==Resolution::Closed{return Ok(Resolution::Closed);}
        let loss=l.loss.ok_or("Portal loss is not durable.")?;
        if !l.custody && settled_source(&tx,r).await? {close(&tx,r.generation).await?;tx.commit().await.map_err(database)?;return Ok(Resolution::Closed);}
        if !l.custody || !crate::losses::journal::stage(&tx,&loss).await.map_err(database)? {return Err("Portal loss custody could not be confirmed.".into());}
        close(&tx,r.generation).await?;tx.commit().await.map_err(database)?;handoff_loss(world,loss);return Ok(Resolution::Closed);
    }
    let Some((ship,tick))=live else {return if state.resolution==Resolution::Closed {Ok(Resolution::Closed)}else{Err("The last portal source combat state is unknown; it remains quarantined.".into())};};
    let target=state.resolution!=Resolution::Closed && state.module && fixed(&tx,i).await? && i.boot==boot() && i.expires>combat::now_ms()
        && {let w=world.lock();source(&w,&req,true).is_ok() && destination(&w,&req,i.quote.harmonics).is_ok()};
    // Even a Ready checkpoint with a lost ACK can have newer live combat.
    // Save the fresh same-reservation snapshot, never a retained healthy copy;
    // this writes the existing charge, without performing another debit.
    if !crate::space_state::write_portal(&tx,i.system,&ship,tick).await.map_err(database)? {return Err("Portal source checkpoint remains quarantined.".into());}
    if target {tx.execute(r#"UPDATE "portalReceipts" SET state=1,"completedAt"=$2 WHERE generation=$1 AND state=0"#,&[&r.generation,&combat::now_ms()]).await.map_err(database)?;}
    else {close(&tx,r.generation).await?;}
    tx.commit().await.map_err(database)?;
    let lethal={let w=world.lock();w.parks.get(&i.system).and_then(|p|p.ships.get(&i.ship))
        .filter(|s|s.pilot.char_id==i.pilot && s.client_id==pending.client && s.identity_reserved)
        .filter(|s|s.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6))
        .map(|s|Loss::of(s,i.system,s.pilot.type_id==670))};
    if let Some(loss)=lethal {remember_loss(world,r,loss);return Err("Portal source became lethal during checkpoint COMMIT; retaining its loss.".into());}
    if !target {return Ok(Resolution::Closed);}
    // No SQL await between the final runtime validation and publication.
    let mut w=world.lock();
    if !w.pending_portals.get(&r.generation).is_some_and(|p|p.receipt==*r) {return Err("Portal generation was superseded.".into());}
    if source(&w,&req,true).is_err() || destination(&w,&req,i.quote.harmonics).is_err() || i.expires<=combat::now_ms() {
        if let Some(s)=w.parks.get(&i.system).and_then(|p|p.ships.get(&i.ship)).filter(|s|s.pilot.char_id==i.pilot && s.client_id==pending.client) {
            if s.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {
                let loss=Loss::of(s,i.system,s.pilot.type_id==670);drop(w);remember_loss(world,r,loss);
            }
        }
        return Err("Accepted portal source or destination changed; closing its paid cycle.".into());
    }
    if w.bridges.get(&i.ship).is_some_and(|b|b.generation!=r.generation){return Err("A newer portal owns this ship.".into());}
    if !w.bridges.contains_key(&i.ship) {
        w.bridges.insert(i.ship,bridge(r));announce(&w,&req,true);let tick=w.parks[&i.system].tick;
        crate::svc::space::broadcast(&w,i.system,None,vec![crate::svc::space::special_fx(tick,i.ship,None,"effects.JumpPortal")]);
    }Ok(Resolution::Ready)
}

pub(super) async fn complete_owned(r:&Request,receipt:&Receipt,reservation:&mut Reservation,events:Vec<Value>,ack:bool)->Refused<()> {
    let mut notified=false;let mut accepted=ack;
    loop {
        let result:Refused<bool>=async {
            let mut db=tokio::time::timeout(std::time::Duration::from_secs(10),crate::db::connect(&r.url)).await.map_err(|_|"Portal recovery connection timed out.".to_string())?.map_err(database)?;
            let tx=db.transaction().await.map_err(database)?;let outcome=locked(&tx,receipt).await?.resolution;drop(tx);
            if outcome==Resolution::Superseded {forget(&r.world,receipt);reservation.state=CommitState::Released;return Ok(true);}
            if matches!(outcome,Resolution::Accepted|Resolution::Ready|Resolution::Closed){accepted=true;reservation.state=CommitState::Accepted;}
            if outcome==Resolution::RolledBack && !accepted {release(&r.world,receipt,true,false);reservation.state=CommitState::Released;return Ok(false);}
            if !accepted || outcome==Resolution::Unknown {return Err("Portal COMMIT outcome remains unknown.".into());}
            if !notified {let w=r.world.lock();for e in &events {w.notify(&[r.client],"OnItemChange",e.clone());}notified=true;}
            let pending=r.world.lock().pending_portals.get(&receipt.generation).cloned().ok_or("Portal operation ownership was lost.")?;
            match attempt(&r.world,&mut db,&pending).await? {
                outcome @ (Resolution::Ready|Resolution::Closed)=>{release(&r.world,receipt,false,outcome==Resolution::Closed);reservation.state=CommitState::Released;Ok(true)},
                Resolution::Superseded=>{forget(&r.world,receipt);reservation.state=CommitState::Released;Ok(true)},
                _=>Err("Accepted portal finalization remains unresolved.".into()),
            }
        }.await;
        match result {Ok(true)=>return Ok(()),Ok(false)=>return Err("The portal payment was rolled back.".into()),Err(error)=>tracing::warn!(generation=receipt.generation,"{error}")}
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

pub async fn retry(server:&Server,db:&mut Client) {
    let mut pending:Vec<_>=server.world.lock().pending_portals.keys().copied().collect();pending.sort_unstable();
    for generation in pending {
        let Some(p)=({let mut w=server.world.lock();w.pending_portals.get_mut(&generation).filter(|p|p.busy==0).map(|p|{p.busy=claim();p.clone()})}) else {continue;};
        let _busy=Busy {world:server.world.clone(),generation:p.receipt.generation,claim:p.busy};
        match attempt(&server.world,db,&p).await {Ok(Resolution::RolledBack)=>release(&server.world,&p.receipt,true,false),
            Ok(outcome @ (Resolution::Ready|Resolution::Closed))=>release(&server.world,&p.receipt,false,outcome==Resolution::Closed),
            Ok(Resolution::Superseded)=>forget(&server.world,&p.receipt),Ok(_)=>{},Err(e)=>tracing::warn!(generation=p.receipt.generation,"portal retry: {e}")}
    }
    let cursor=server.world.lock().portal_retry_cursor;
    let rows=match db.query(r#"SELECT generation,"characterID","shipID",intent,state,loss FROM "portalReceipts" WHERE state IN(0,1)
        ORDER BY CASE WHEN generation>$1 THEN 0 ELSE 1 END,generation LIMIT 64"#,&[&cursor]).await {Ok(r)=>r,Err(e)=>{tracing::warn!("portal recovery scan: {e}");return;}};
    for row in rows {
        let generation:i64=row.get("generation");server.world.lock().portal_retry_cursor=generation;
        if server.world.lock().pending_portals.contains_key(&generation){continue;}
        let Some(r)=decode(&row) else {tracing::warn!(generation,"invalid portal receipt remains quarantined");continue;};
        if row.get::<_,i16>("state")==0 {
            let Some(raw)=row.get::<_,Option<String>>("loss") else {
                // A generic loss may already have committed before this
                // receipt retained its loss payload. Only exact destruction
                // proof can retire such a marker; surviving/unknown hulls
                // remain quarantined and are never reconstructed here.
                let result=async {let tx=db.transaction().await.map_err(database)?;let state=locked(&tx,&r).await?;
                    if state.resolution==Resolution::Accepted && !state.custody && settled_source(&tx,&r).await? {close(&tx,generation).await?;}
                    tx.commit().await.map_err(database)}.await;
                if let Err(error)=result {tracing::warn!(generation,"portal completion proof: {error}");}continue;
            };
            let Some(loss)= (if raw.len()<=1048576 {serde_json::from_str::<Loss>(&raw).ok()}else{None}).filter(|l|loss_valid(l,&r)) else{continue;};
            let mut p=Pending::owned(r.clone(),0,Charged::full(0.0,0.0,0),Charged::full(0.0,0.0,0));p.loss=Some(loss);p.busy=0;
            server.world.lock().pending_portals.entry(generation).or_insert(p);continue;
        }
        // A completed payment does not authorize recreating an offline ship or
        // a transient cyno ID from a prior process. Never renew its deadline.
        let live={let w=server.world.lock();w.bridges.get(&r.intent.ship).is_some_and(|b|b.generation==generation && b.until_ms>combat::now_ms() && bridge_live(&w,r.intent.ship,b))};
        if !live {
            let result=async {let tx=db.transaction().await.map_err(database)?;let l=locked(&tx,&r).await?;
                if l.resolution==Resolution::Ready {close(&tx,generation).await?;}tx.commit().await.map_err(database)}.await;
            if let Err(error)=result {tracing::warn!(generation,"portal closure: {error}");}
        }
    }
}
