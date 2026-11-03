//! A confirmed player hull loss is an obligation, not a best-effort callback.
//! Persist the original attribution before settlement; commit destruction,
//! insurance/clone consequences and the kill record together.
use super::*;
#[cfg(test)] mod tests;

pub async fn ensure(db:&Client)->Result<(),tokio_postgres::Error> {
    db.batch_execute(r#"CREATE TABLE IF NOT EXISTS "pendingShipLosses" (
        "shipID" integer PRIMARY KEY REFERENCES entity("itemID") ON DELETE CASCADE,
        "characterID" integer NOT NULL,"solarSystemID" integer NOT NULL,"shipTypeID" integer NOT NULL,
        occupied boolean NOT NULL,"createdAt" bigint NOT NULL,loss text NOT NULL);
        CREATE INDEX IF NOT EXISTS "pendingShipLosses_character" ON "pendingShipLosses"("characterID");"#).await
}

pub fn blocked_in_memory(world:&crate::world::World,ship:i64)->bool {
    world.lock().pending_ship_losses.contains_key(&ship)
}
pub fn blocked_character_in_memory(world:&crate::world::World,pilot:i64)->bool {
    world.lock().pending_ship_losses.values().any(|loss|loss.char_id==pilot && !loss.unpiloted)
}

pub fn valid(loss:&Loss)->bool {
    let id=|n:i64|n>0 && n<=i32::MAX as i64;
    id(loss.ship_id) && id(loss.char_id) && id(loss.ship_type) && id(loss.system) && loss.pos.is_finite()
        && loss.arrival_custody.as_ref().is_none_or(|arrival|id(arrival.system) && arrival.at.is_finite())
        && loss.capsule==(loss.ship_type==TYPE_CAPSULE)
        && loss.final_blow.is_none_or(|n|i32::try_from(n).is_ok())
        && loss.attackers.iter().all(|a|i32::try_from(a.who).is_ok() && i32::try_from(a.ship_type).is_ok()
            && i32::try_from(a.weapon_type).is_ok() && a.damage.is_finite() && a.damage>=0.0)
}

fn decode(raw:&str,ship:i64,pilot:i64,system:i64,ty:i64)->Option<Loss> {
    if raw.len()>1_048_576{return None;}
    let loss:Loss=serde_json::from_str(raw).ok()?;
    (valid(&loss) && (loss.ship_id,loss.char_id,loss.system,loss.ship_type)==(ship,pilot,system,ty)).then_some(loss)
}

/// The caller holds character/hull custody in the usual order. This also
/// supports checkpointing an observed lethal threshold before profile reload.
pub async fn stage(tx:&tokio_postgres::Transaction<'_>,loss:&Loss)->Result<bool,tokio_postgres::Error> {
    if !valid(loss){return Ok(false);}
    let Ok(raw)=serde_json::to_string(loss) else{return Ok(false);};
    if raw.len()>1_048_576{return Ok(false);}
    if tx.query_opt(r#"SELECT 1 FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID" JOIN "invGroups" g ON g."groupID"=t."groupID"
        WHERE e."itemID"=$1 AND e."ownerID"=$2 AND e."locationID"=$3 AND e."typeID"=$4 AND e.flag=0 AND e.quantity=1 AND e.singleton<>0
        AND g."categoryID"=6 FOR UPDATE OF e"#,
        &[&(loss.ship_id as i32),&(loss.char_id as i32),&(loss.custody_system() as i32),&(loss.ship_type as i32)]).await?.is_none(){return Ok(false);}
    let occupied=!loss.unpiloted && tx.query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2
        AND "solarSystemID"=$3 AND COALESCE("stationID",0)=0"#,
        &[&(loss.char_id as i32),&(loss.ship_id as i32),&(loss.custody_system() as i32)]).await?.is_some();
    // The indexed system remains the encounter's attribution. Explicit
    // destination custody is part of the same immutable serialized record.
    tx.execute(r#"INSERT INTO "pendingShipLosses"("shipID","characterID","solarSystemID","shipTypeID",occupied,"createdAt",loss)
        VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT("shipID") DO NOTHING"#,
        &[&(loss.ship_id as i32),&(loss.char_id as i32),&(loss.system as i32),&(loss.ship_type as i32),&occupied,&combat::now_ms(),&raw]).await?;
    Ok(true)
}

async fn capture(db:&mut Client,loss:&Loss)->Result<Option<Loss>,tokio_postgres::Error> {
    let tx=db.transaction().await?;
    if crate::svc::login::lock_character_lifecycle(&tx,loss.char_id as i32).await? {
        tx.query_one(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#,&[&(loss.char_id as i32)]).await?;
    }
    if !stage(&tx,loss).await? {return Ok(None);}
    let row=tx.query_one(r#"SELECT "characterID","solarSystemID","shipTypeID",loss,occupied FROM "pendingShipLosses" WHERE "shipID"=$1"#,
        &[&(loss.ship_id as i32)]).await?;
    let mut recorded=decode(&row.get::<_,String>(3),loss.ship_id,i64::from(row.get::<_,i32>(0)),i64::from(row.get::<_,i32>(1)),i64::from(row.get::<_,i32>(2)));
    if !row.get::<_,bool>(4) {if let Some(loss)=recorded.as_mut(){loss.unpiloted=true;}}
    tx.commit().await?;
    Ok(recorded)
}

pub async fn handle(server:&Server,db:&mut Client,loss:Loss) {
    if !valid(&loss){error!(ship=loss.ship_id,"invalid confirmed hull loss; refusing destructive settlement");return;}
    // Keep the first observed loss. Later retries cannot replace attribution
    // with a newly loaded inactive hull profile or a stale connection ID.
    let loss=server.world.lock().pending_ship_losses.entry(loss.ship_id).or_insert(loss).clone();
    let recorded=match capture(db,&loss).await {
        Ok(Some(mut recorded))=>{
            recorded.client_id=server.world.lock().clients_of(recorded.char_id).first().copied().unwrap_or(0);
            recorded
        }
        Ok(None)=>{
            if matches!(completed(db,&loss).await,Ok(true)) {server.world.lock().pending_ship_losses.remove(&loss.ship_id);}
            return;
        }
        Err(error)=>{error!(ship=loss.ship_id,"saving confirmed space loss: {}",crate::logging::chain(&error));return;}
    };
    if recorded.capsule && !recorded.unpiloted {podded(server,db,&recorded).await;}
    else {ship_lost(server,db,&recorded).await;}
    if matches!(completed(db,&recorded).await,Ok(true)) {server.world.lock().pending_ship_losses.remove(&recorded.ship_id);}
}

async fn completed(db:&impl GenericClient,loss:&Loss)->Result<bool,tokio_postgres::Error> {
    Ok(db.query_one(r#"SELECT NOT EXISTS(SELECT 1 FROM entity WHERE "itemID"=$1)
        AND EXISTS(SELECT 1 FROM "killLog" WHERE "victimItemID"=$1 AND "victimCharID"=$2)"#,
        &[&(loss.ship_id as i32),&(loss.char_id as i32)]).await?.get(0))
}

pub async fn retry(server:&Server,db:&mut Client) {
    let pending:Vec<_>=server.world.lock().pending_ship_losses.values().cloned().collect();
    let ids:std::collections::BTreeSet<_>=pending.iter().map(|l|l.ship_id).collect();
    for loss in pending {handle(server,db,loss).await;}
    let cursor=server.world.lock().pending_ship_loss_cursor;
    let rows=match db.query(r#"SELECT "shipID","characterID","solarSystemID","shipTypeID",loss FROM "pendingShipLosses"
        WHERE "shipID">$1 ORDER BY "shipID" LIMIT 64"#,&[&cursor]).await {
        Ok(rows)=>rows,Err(error)=>{warn!("reading confirmed space losses: {}",crate::logging::chain(&error));return;}
    };
    // Include invalid rows and memory duplicates in scan progress. A complete
    // page of quarantined losses must not monopolize every recovery attempt.
    server.world.lock().pending_ship_loss_cursor=rows.last().map(|row|row.get::<_,i32>(0)).unwrap_or(0);
    for row in rows {
        let ship=i64::from(row.get::<_,i32>(0));if ids.contains(&ship){continue;}
        if let Some(loss)=decode(&row.get::<_,String>(4),ship,i64::from(row.get::<_,i32>(1)),i64::from(row.get::<_,i32>(2)),i64::from(row.get::<_,i32>(3))) {
            handle(server,db,loss).await;
        }else{warn!(ship,"invalid durable space loss remains quarantined");}
    }
}
