//! Atomic player motion checkpoints. Lock inventory before taking a live
//! snapshot, so queued writes cannot carry a pre-jump position across a move.
use crate::{
    ballpark::{Motion, Ship},
    combat, destiny,
    svc::inflight,
    services::Ctx,
    world::World,
};
use tokio_postgres::{Client, Transaction};
use tracing::{error, warn};

#[cfg(test)]
mod tests;

// Keep the motion fields at the top level so existing checkpoints and motion
// inspection tools remain compatible. Module custody is revalidated on restore.
#[derive(serde::Serialize,serde::Deserialize)]
struct Snapshot {
    #[serde(flatten)]
    motion:Motion,
    #[serde(default)]
    cloak:crate::cloak::SavedState,
}

pub async fn ensure(db: &Client) -> Result<(), tokio_postgres::Error> {
    db.batch_execute(r#"
        CREATE TABLE IF NOT EXISTS "shipSpaceMotion" (
            "itemID" integer PRIMARY KEY REFERENCES entity("itemID") ON DELETE CASCADE,
            "characterID" integer NOT NULL, "solarSystemID" integer NOT NULL,
            tick bigint NOT NULL, "savedAt" bigint NOT NULL, state text NOT NULL);
        CREATE OR REPLACE FUNCTION invalidate_ship_space_motion() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            DELETE FROM "shipSpaceMotion" WHERE "itemID"=OLD."itemID";
            RETURN NEW;
        END $$;
        CREATE OR REPLACE TRIGGER invalidate_ship_space_motion
            BEFORE UPDATE OF "ownerID","locationID",flag,"typeID",quantity,x,y,z ON entity
            FOR EACH ROW EXECUTE FUNCTION invalidate_ship_space_motion();
    "#).await
}

pub(crate) async fn lock_current(
    tx: &Transaction<'_>,
    pilot: i64,
    ship: i64,
    system: i64,
) -> Result<bool, tokio_postgres::Error> {
    if tx
        .query_opt(
            r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2
        AND "solarSystemID"=$3 AND COALESCE("stationID",0)=0 FOR UPDATE"#,
            &[&(pilot as i32), &(ship as i32), &(system as i32)],
        )
        .await?
        .is_none()
    {
        return Ok(false);
    }
    Ok(tx
        .query_opt(
            r#"SELECT 1 FROM entity WHERE "itemID"=$1 AND "ownerID"=$2
        AND "locationID"=$3 AND COALESCE(flag,0)=0 AND quantity>0 FOR UPDATE"#,
            &[&(ship as i32), &(pilot as i32), &(system as i32)],
        )
        .await?
        .is_some())
}

async fn write(
    tx: &Transaction<'_>,
    system: i64,
    ship: &Ship,
    tick: i64,
) -> Result<bool, tokio_postgres::Error> {
    if crate::losses::pending::blocked(tx,ship.id).await? {return Ok(false);}
    // The inactive profile loaded after restart can have a different hull
    // maximum. Preserve a confirmed lethal result, including attribution,
    // rather than interpreting its raw damage against that new maximum.
    if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_max.is_finite() && f.vitals.hull_max>0.0
        && f.vitals.hull_damage.is_finite() && f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {
        return crate::losses::journal::stage(tx,&crate::losses::Loss::of(ship,system,ship.pilot.type_id==670)).await;
    }
    write_snapshot(tx,system,ship,tick).await
}

/// The travel journal has already locked and validated its exact generation.
/// Only that owner may write through its own unresolved arrival marker.
pub(crate) async fn write_arrival(tx:&Transaction<'_>,system:i64,ship:&Ship,tick:i64)->Result<bool,tokio_postgres::Error> {
    if crate::losses::pending::blocked_except_travel(tx,ship.id).await?
        || ship.fight.as_ref().is_none_or(|f| !f.vitals.hull_max.is_finite() || f.vitals.hull_max<=0.0
            || !f.vitals.hull_damage.is_finite() || f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {return Ok(false);}
    write_snapshot(tx,system,ship,tick).await
}

/// Only the portal journal holding the exact receipt and custody locks may
/// checkpoint through its own marker. The source remains live during startup.
pub(crate) async fn write_portal(tx:&Transaction<'_>,system:i64,ship:&Ship,tick:i64)->Result<bool,tokio_postgres::Error> {
    if crate::losses::pending::blocked_except_portal(tx,ship.id).await?
        || ship.fight.as_ref().is_none_or(|f| !f.vitals.hull_max.is_finite() || f.vitals.hull_max<=0.0
            || !f.vitals.hull_damage.is_finite() || f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {return Ok(false);}
    write_snapshot(tx,system,ship,tick).await
}

pub(crate) async fn write_logout(tx:&Transaction<'_>,system:i64,ship:&Ship,tick:i64)->Result<bool,tokio_postgres::Error> {
    if crate::losses::pending::blocked_except_logout(tx,ship.id).await? {return Ok(false);}
    if ship.fight.as_ref().is_none_or(|f| !f.vitals.hull_max.is_finite() || f.vitals.hull_max<=0.0 || !f.vitals.hull_damage.is_finite()) {return Ok(false);}
    if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {
        return crate::losses::journal::stage(tx,&crate::losses::Loss::of(ship,system,ship.pilot.type_id==670)).await;
    }
    write_snapshot(tx,system,ship,tick).await
}

async fn write_snapshot(tx:&Transaction<'_>,system:i64,ship:&Ship,tick:i64)->Result<bool,tokio_postgres::Error> {
    let motion = ship.motion(tick);
    // Validate before touching inventory. Corrupt physics must not become the
    // only durable location a character can load.
    if !ship.clone().restore_motion(&motion, tick) {
        return Ok(false);
    }
    let Ok(state) = serde_json::to_string(&Snapshot {motion,cloak:crate::cloak::snapshot(ship)}) else {
        return Ok(false);
    };
    let saved_at = combat::now_ms();
    tx.execute(
        r#"UPDATE entity SET x=$2,y=$3,z=$4 WHERE "itemID"=$1"#,
        &[&(ship.id as i32), &ship.pos.x, &ship.pos.y, &ship.pos.z],
    )
    .await?;
    if let Some(fighter) = &ship.fight {
        inflight::write_vitals_at(tx, ship.id, fighter, false, saved_at).await?;
    }
    crate::cloak::durability::save_history(tx,ship,saved_at).await?;
    tx.execute(r#"INSERT INTO "shipSpaceMotion" ("itemID","characterID","solarSystemID",tick,"savedAt",state)
        VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT ("itemID") DO UPDATE SET "characterID"=$2,
        "solarSystemID"=$3,tick=$4,"savedAt"=$5,state=$6"#,
        &[&(ship.id as i32),&(ship.pilot.char_id as i32),&(system as i32),&tick,&saved_at,&state]).await?;
    Ok(true)
}

#[cfg(test)]
pub async fn save_detached(
    db: &mut Client,
    system: i64,
    ship: &Ship,
    tick: i64,
) -> Result<bool, tokio_postgres::Error> {
    let tx = db.transaction().await?;
    if !lock_current(&tx, ship.pilot.char_id, ship.id, system).await? {
        return Ok(false);
    }
    if !write(&tx, system, ship, tick).await? {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

pub async fn checkpoint(world: &World, db: &mut Client) {
    let candidates: Vec<_> = {
        let world = world.lock();
        world
            .parks
            .iter()
            .flat_map(|(system, park)| {
                park.ships
                    .values()
                    .filter(|s| s.is_player() && s.client_id != 0 && !s.identity_reserved)
                    .map(|s| (*system, s.id, s.pilot.char_id, s.client_id))
            })
            .collect()
    };
    for (system, id, pilot, client) in candidates {
        if let Err(e) = checkpoint_one(world, db, system, id, pilot, client).await {
            error!("checkpointing ship {id}: {}", crate::logging::chain(&e));
        }
    }
}

pub(crate) async fn checkpoint_one(
    world: &World,
    db: &mut Client,
    system: i64,
    id: i64,
    pilot: i64,
    client: i64,
) -> Result<bool, tokio_postgres::Error> {
    let tx = db.transaction().await?;
    if !lock_current(&tx, pilot, id, system).await? {
        return Ok(false);
    }
    let snapshot = {
        let world = world.lock();
        world.parks.get(&system).and_then(|park| {
            park.ships
                .get(&id)
                .filter(|s| {
                    s.is_player()
                        && !s.identity_reserved
                        && s.client_id == client
                        && s.pilot.char_id == pilot
                })
                .map(|ship| (ship.clone(), park.tick))
        })
    };
    let Some((ship, tick)) = snapshot else {
        return Ok(false);
    };
    if !write(&tx, system, &ship, tick).await? {
        return Ok(false);
    }
    tx.commit().await?;
    Ok(true)
}

/// Called only for a reconnect, not when undocking, boarding or jumping.
pub async fn restore(
    ctx: &Ctx<'_>,
    system: i64,
    ship: &mut Ship,
) -> Result<bool, tokio_postgres::Error> {
    let db=ctx.db;
    let row = db
        .query_opt(
            r#"SELECT m.state,m.tick,m."savedAt" FROM "shipSpaceMotion" m
        JOIN entity e ON e."itemID"=m."itemID"
        JOIN "character_" c ON c."characterID"=m."characterID" AND c."shipID"=e."itemID"
        WHERE m."itemID"=$1 AND m."characterID"=$2 AND m."solarSystemID"=$3
        AND e."ownerID"=$2 AND e."locationID"=$3 AND COALESCE(e.flag,0)=0
        AND c."solarSystemID"=$3 AND COALESCE(c."stationID",0)=0"#,
            &[
                &(ship.id as i32),
                &(ship.pilot.char_id as i32),
                &(system as i32),
            ],
        )
        .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let raw: String = row.get(0);
    let snapshot = if raw.len() <= 16_384 {
        serde_json::from_str::<Snapshot>(&raw).ok()
    } else {
        None
    };
    let now = combat::now_ms();
    let saved_at: i64 = row.get(2);
    let Some(snapshot) =
        snapshot.filter(|m| m.motion.tick == row.get::<_, i64>(1) && saved_at >= 0 && saved_at <= now)
    else {
        warn!(
            ship = ship.id,
            "invalid durable motion; using stored position"
        );
        return Ok(false);
    };
    if !ship.restore_motion(&snapshot.motion, destiny::tick_now()) {
        warn!(
            ship = ship.id,
            "invalid durable trajectory; using stored position"
        );
        return Ok(false);
    }
    if let Some(fighter) = ship.fight.as_mut() {
        for row in db.query(r#"SELECT "attributeID",COALESCE("valueFloat","valueInt"::float8) FROM entity_attributes
            WHERE "itemID"=$1 AND "attributeID" IN (18,264)"#,&[&(ship.id as i32)]).await? {
            let Some(value)=row.get::<_,Option<f64>>(1).filter(|v|v.is_finite()) else { continue; };
            match row.get::<_,i32>(0) {
                combat::attr::CHARGE=>fighter.vitals.capacitor.set(saved_at,value),
                combat::attr::SHIELD_CHARGE=>fighter.vitals.shield.set(saved_at,value),
                _=>{}
            }
        }
    }
    inflight::restore_cloak(ctx,ship,snapshot.cloak).await?;
    Ok(true)
}
