//! Disconnected ships stay live until their final snapshot can be journaled.
//! Portal sources additionally remain through the accepted generator cycle.
use crate::{ballpark::Ship,combat,conn::Server,world::World};
use tokio_postgres::{Client,Error,GenericClient};

#[derive(Clone,Debug)]
pub(crate) struct Pending {generation:i64,system:i64,ship:Ship,tick:i64}

pub async fn ensure(db:&Client)->Result<(),Error> {
    db.batch_execute(r#"
        CREATE SEQUENCE IF NOT EXISTS ship_logout_generation;
        CREATE TABLE IF NOT EXISTS "pendingShipLogouts" (
            "shipID" integer PRIMARY KEY REFERENCES entity("itemID") ON DELETE CASCADE,
            "characterID" integer NOT NULL,"solarSystemID" integer NOT NULL,
            generation bigint NOT NULL DEFAULT nextval('ship_logout_generation'),"createdAt" bigint NOT NULL);
        CREATE INDEX IF NOT EXISTS "pendingShipLogouts_character" ON "pendingShipLogouts"("characterID");
    "#).await
}

pub async fn blocked(db:&impl GenericClient,ship:i64)->Result<bool,Error> {
    Ok(db.query_one(r#"SELECT EXISTS(SELECT 1 FROM "pendingShipLogouts" WHERE "shipID"=$1)"#,&[&(ship as i32)]).await?.get(0))
}
pub async fn blocked_character(db:&impl GenericClient,pilot:i64)->Result<bool,Error> {
    Ok(db.query_one(r#"SELECT EXISTS(SELECT 1 FROM "pendingShipLogouts" WHERE "characterID"=$1)"#,&[&(pilot as i32)]).await?.get(0))
}
pub fn blocked_character_in_memory(world:&World,pilot:i64)->bool {
    world.lock().pending_logouts.values().any(|p|p.ship.pilot.char_id==pilot)
}

/// The persistence worker resumes attempts begun by disconnect handlers. The
/// character lifecycle lock serializes both callers. A marker without a final
/// snapshot after restart is deliberately not interpreted as survival.
pub async fn retry(server:&Server,db:&mut Client) {
    let ids:std::collections::BTreeSet<_>={let w=server.world.lock();w.retained_logouts.keys().chain(w.pending_logouts.keys()).copied().collect()};
    for id in ids {
        if let Err(error)=retry_one(&server.world,db,id).await {
            tracing::warn!(ship=id,"saving retained logout: {}",crate::logging::chain(&error));
        }
    }
}

pub(crate) async fn retry_one(world:&World,db:&mut Client,id:i64)->Result<(),Error> {
    let missing={let w=world.lock();w.retained_logouts.get(&id).copied().filter(|_|!w.pending_logouts.contains_key(&id)
        && !w.parks.values().any(|park|park.ships.contains_key(&id)))};
    if let Some(identity)=missing {
        let remains=db.query_one(r#"SELECT EXISTS(SELECT 1 FROM entity e JOIN "character_" c ON c."characterID"=e."ownerID"
            WHERE e."itemID"=$1 AND c."characterID"=$2 AND c."shipID"=$1 AND COALESCE(c."stationID",0)=0)"#,
            &[&(id as i32),&(identity.0 as i32)]).await?.get::<_,bool>(0);
        if !remains {let mut w=world.lock();if w.retained_logouts.get(&id)==Some(&identity) && !w.pending_logouts.contains_key(&id) {w.retained_logouts.remove(&id);}}
        return Ok(());
    }
    let (pilot,client,system,pending)={
        let mut w=world.lock();
        if let Some(p)=w.pending_logouts.get(&id) {(p.ship.pilot.char_id,p.ship.client_id,p.system,Some(p.clone()))}
        else {
            let Some(&(pilot,client))=w.retained_logouts.get(&id) else {return Ok(());};
            let live=w.parks.iter().find_map(|(system,park)|park.ships.get(&id).map(|s|(*system,s)));
            let Some((system,ship))=live else {return Ok(());};
            if !ship.is_player() || ship.pilot.char_id!=pilot || ship.client_id!=client {
                w.retained_logouts.remove(&id);return Ok(());
            }
            if !w.clients_of(pilot).is_empty() || ship.identity_reserved || crate::svc::jumps::portal::active_until_locked(&w,id).is_some() {return Ok(());}
            (pilot,client,system,None)
        }
    };
    let tx=db.transaction().await?;
    let available=crate::svc::login::lock_character_lifecycle(&tx,pilot as i32).await?;
    if let Some(p)=&pending {
        // Loss settlement may already have removed the old hull after an
        // accepted final COMMIT lost its reply. Inspect completion before
        // requiring that obsolete hull to remain the current active ship.
        let current=tx.query_opt(r#"SELECT generation FROM "pendingShipLogouts" WHERE "shipID"=$1"#,&[&(id as i32)]).await?.map(|row|row.get::<_,i64>(0));
        if current!=Some(p.generation) {
            let mut w=world.lock();
            if w.pending_logouts.get(&id).is_some_and(|old|old.generation==p.generation) {
                w.pending_logouts.remove(&id);
                if current.is_none() && w.retained_logouts.get(&id)==Some(&(pilot,client)) {w.retained_logouts.remove(&id);}
            }
            return Ok(());
        }
    }
    if !available || !crate::space_state::lock_current(&tx,pilot,id,system).await? {return Ok(());}
    let marker=tx.query_opt(r#"SELECT generation,"characterID","solarSystemID" FROM "pendingShipLogouts" WHERE "shipID"=$1 FOR UPDATE"#,&[&(id as i32)]).await?;
    if let Some(p)=&pending {
        if marker.as_ref().is_none_or(|row|row.get::<_,i64>(0)!=p.generation) {
            // Absence means the final snapshot and marker deletion committed.
            // Never replay this captured snapshot over a newer generation.
            let mut w=world.lock();
            if w.pending_logouts.get(&id).is_some_and(|old|old.generation==p.generation) {
                w.pending_logouts.remove(&id);
                if marker.is_none() && w.retained_logouts.get(&id)==Some(&(pilot,client)) {w.retained_logouts.remove(&id);}
            }
            return Ok(());
        }
    }
    if crate::losses::pending::blocked_except_logout(&tx,id).await? {return Ok(());}
    let generation=if let Some(row)=marker {
        if row.get::<_,i32>(1)!=pilot as i32 || row.get::<_,i32>(2)!=system as i32 {return Ok(());}
        row.get::<_,i64>(0)
    }else{
        // No live removal before this COMMIT. A lost acknowledgement simply
        // leaves the same obligation for the next custody-locked attempt.
        {
            let w=world.lock();
            if !ready(&w,id,pilot,client,system) {return Ok(());}
        }
        tx.execute(r#"INSERT INTO "pendingShipLogouts"("shipID","characterID","solarSystemID","createdAt") VALUES($1,$2,$3,$4)"#,
            &[&(id as i32),&(pilot as i32),&(system as i32),&combat::now_ms()]).await?;
        tx.commit().await?;
        return Ok(());
    };
    let p=if let Some(p)=pending {p}else{
        let mut w=world.lock();
        if !ready(&w,id,pilot,client,system) {return Ok(());}
        let tick=w.parks[&system].tick;
        let Some(ship)=crate::svc::space::remove_ship(&mut w,system,id) else {return Ok(());};
        let p=Pending{generation,system,ship,tick};w.pending_logouts.insert(id,p.clone());p
    };
    if !crate::space_state::write_logout(&tx,p.system,&p.ship,p.tick).await? {return Ok(());}
    tx.execute(r#"DELETE FROM "pendingShipLogouts" WHERE "shipID"=$1 AND generation=$2"#,&[&(id as i32),&generation]).await?;
    tx.commit().await?;
    let mut w=world.lock();
    if w.pending_logouts.get(&id).is_some_and(|current|current.generation==generation) {w.pending_logouts.remove(&id);w.retained_logouts.remove(&id);}
    Ok(())
}

fn ready(w:&crate::world::WorldState,id:i64,pilot:i64,client:i64,system:i64)->bool {
    w.clients_of(pilot).is_empty() && !w.pending_ship_losses.contains_key(&id)
        && crate::svc::jumps::portal::active_until_locked(w,id).is_none()
        && w.parks.get(&system).and_then(|park|park.ships.get(&id)).is_some_and(|ship|
            ship.is_player() && ship.pilot.char_id==pilot && ship.client_id==client && !ship.identity_reserved
            && ship.fight.as_ref().is_none_or(|fight|!fight.modules.iter().any(|m|m.action==combat::Action::Cyno)))
}

#[cfg(test)] mod tests;
