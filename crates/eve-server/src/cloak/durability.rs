//! Preserve observed cooldown deadlines independently of a ship's trajectory.
//!
//! Trinity exposes delay durations and refusal messages, but the supplied
//! client does not establish whether the original server reset them on docking.
//! Keeping an already observed deadline is this server's continuity policy;
//! this is not a new original dogma attribute or a reconstructed timer formula.
use super::History;
use crate::{ballpark::Ship, items::Item};
use tokio_postgres::{Client, GenericClient, Transaction};

#[cfg(test)]
mod tests;

pub async fn ensure(db:&Client)->Result<(),tokio_postgres::Error> {
    db.batch_execute(r#"
        CREATE TABLE IF NOT EXISTS "cloakCooldowns" (
            "itemID" integer PRIMARY KEY REFERENCES entity("itemID") ON DELETE CASCADE,
            "shipID" integer NOT NULL REFERENCES entity("itemID") ON DELETE CASCADE,
            "ownerID" integer NOT NULL, "typeID" integer NOT NULL, "shipTypeID" integer NOT NULL,
            "targetingAt" bigint NOT NULL DEFAULT 0, "readyAt" bigint NOT NULL DEFAULT 0);
        CREATE INDEX IF NOT EXISTS "cloakCooldowns_ship" ON "cloakCooldowns" ("shipID");
        CREATE OR REPLACE FUNCTION invalidate_cloak_cooldowns() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF OLD."ownerID" IS DISTINCT FROM NEW."ownerID"
                OR OLD."typeID" IS DISTINCT FROM NEW."typeID"
                OR OLD.singleton IS DISTINCT FROM NEW.singleton
                OR OLD.quantity IS DISTINCT FROM NEW.quantity THEN
                DELETE FROM "cloakCooldowns" WHERE "itemID"=OLD."itemID" OR "shipID"=OLD."itemID";
            ELSIF OLD."locationID" IS DISTINCT FROM NEW."locationID"
                OR (OLD.flag IS DISTINCT FROM NEW.flag AND COALESCE(NEW.flag,0) NOT BETWEEN 11 AND 35) THEN
                -- A hull may dock, undock or be stored with its fittings. Only
                -- a fitted module leaving its hull invalidates this binding.
                DELETE FROM "cloakCooldowns" WHERE "itemID"=OLD."itemID" AND "shipID"<>OLD."itemID";
            END IF;
            RETURN NEW;
        END $$;
        CREATE OR REPLACE TRIGGER invalidate_cloak_cooldowns
            BEFORE UPDATE OF "ownerID","typeID",singleton,quantity,"locationID",flag ON entity
            FOR EACH ROW EXECUTE FUNCTION invalidate_cloak_cooldowns();
    "#).await
}

/// The caller normally already holds the character and hull custody locks.
/// Check them again and lock fitted modules before writing their deadlines.
/// Only observed timestamps are saved; repeated saves never restart a timer.
pub async fn save_history(tx:&Transaction<'_>,ship:&Ship,now:i64)->Result<(),tokio_postgres::Error> {
    let Some(fighter)=&ship.fight else{return Ok(());};
    let mut history=fighter.cloak_history.clone();history.prune(now);
    let owner=ship.pilot.char_id as i32;
    let hull=ship.id as i32;
    let ty=ship.pilot.type_id as i32;
    if tx.query_opt(r#"SELECT 1 FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID"
        JOIN "invGroups" g ON g."groupID"=t."groupID"
        WHERE e."itemID"=$1 AND e."ownerID"=$2 AND e."typeID"=$3
        AND e.quantity=1 AND e.singleton<>0 AND g."categoryID"=6 FOR UPDATE OF e"#,
        &[&hull,&owner,&ty]).await?.is_none() {return Ok(());}
    tx.execute(r#"DELETE FROM "cloakCooldowns" WHERE "shipID"=$1 AND "targetingAt"<=$2 AND "readyAt"<=$2"#,
        &[&hull,&now]).await?;
    if history.targeting_at>now {
        save(tx,hull,hull,owner,ty,ty,history.targeting_at,0).await?;
    }
    let ids:Vec<i32>=history.module_ready.keys().take(25).map(|id|*id as i32).collect();
    if ids.is_empty() {return Ok(());}
    let rows=tx.query(r#"SELECT e."itemID",e."typeID" FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID"
        WHERE e."itemID"=ANY($1) AND e."ownerID"=$2 AND e."locationID"=$3
        AND e.quantity=1 AND e.singleton<>0 AND e.flag BETWEEN 11 AND 35 AND t."groupID"=330
        ORDER BY e."itemID" FOR UPDATE OF e"#,&[&ids,&owner,&hull]).await?;
    for row in rows {
        let id:i32=row.get(0);let module_type:i32=row.get(1);
        save(tx,id,hull,owner,module_type,ty,0,history.module_ready[&i64::from(id)]).await?;
    }
    Ok(())
}

async fn save(tx:&Transaction<'_>,item:i32,ship:i32,owner:i32,ty:i32,ship_type:i32,targeting:i64,ready:i64)->Result<(),tokio_postgres::Error> {
    let latest=(i64::MAX/10_000)-11_644_473_600_000;
    tx.execute(r#"INSERT INTO "cloakCooldowns" ("itemID","shipID","ownerID","typeID","shipTypeID","targetingAt","readyAt")
        VALUES ($1,$2,$3,$4,$5,$6,$7) ON CONFLICT ("itemID") DO UPDATE SET
        "shipID"=$2,"ownerID"=$3,"typeID"=$4,"shipTypeID"=$5,
        "targetingAt"=CASE WHEN "cloakCooldowns"."shipID"=$2 AND "cloakCooldowns"."ownerID"=$3
            AND "cloakCooldowns"."typeID"=$4 AND "cloakCooldowns"."shipTypeID"=$5
            THEN GREATEST(CASE WHEN "cloakCooldowns"."targetingAt" BETWEEN 0 AND $8 THEN "cloakCooldowns"."targetingAt" ELSE 0 END,$6) ELSE $6 END,
        "readyAt"=CASE WHEN "cloakCooldowns"."shipID"=$2 AND "cloakCooldowns"."ownerID"=$3
            AND "cloakCooldowns"."typeID"=$4 AND "cloakCooldowns"."shipTypeID"=$5
            THEN GREATEST(CASE WHEN "cloakCooldowns"."readyAt" BETWEEN 0 AND $8 THEN "cloakCooldowns"."readyAt" ELSE 0 END,$7) ELSE $7 END"#,
        &[&item,&ship,&owner,&ty,&ship_type,&targeting,&ready,&latest]).await?;
    Ok(())
}

/// Reads deadlines only. It cannot restore active effects or native movement.
/// Each row must still describe the same assembled hull and fitted module.
pub async fn load_history(db:&impl GenericClient,owner:i64,ship:i64,now:i64)->Result<History,tokio_postgres::Error> {
    let rows=db.query(r#"SELECT c."itemID",c."targetingAt",c."readyAt" FROM "cloakCooldowns" c
        JOIN entity h ON h."itemID"=c."shipID" AND h."ownerID"=c."ownerID" AND h."typeID"=c."shipTypeID"
        JOIN "invTypes" ht ON ht."typeID"=h."typeID" JOIN "invGroups" hg ON hg."groupID"=ht."groupID"
        JOIN entity e ON e."itemID"=c."itemID" AND e."ownerID"=c."ownerID" AND e."typeID"=c."typeID"
        JOIN "invTypes" et ON et."typeID"=e."typeID"
        WHERE c."shipID"=$1 AND c."ownerID"=$2 AND h.quantity=1 AND h.singleton<>0 AND hg."categoryID"=6
        AND e.quantity=1 AND e.singleton<>0
        AND (e."itemID"=h."itemID" OR (e."locationID"=h."itemID" AND e.flag BETWEEN 11 AND 35 AND et."groupID"=330))
        ORDER BY c."itemID""#,&[&(ship as i32),&(owner as i32)]).await?;
    let mut history=History::default();
    for row in rows {
        let id=i64::from(row.get::<_,i32>(0));
        if id==ship {history.targeting_at=row.get(1);}
        else if history.module_ready.len()<25 {history.module_ready.insert(id,row.get(2));}
    }
    history.prune(now);
    Ok(history)
}

/// Use after ItemTxn::item has locked/re-read the source module. The supplied
/// custody predicates and current hull binding are rechecked in this query.
pub async fn module_reactivation_remaining(db:&impl GenericClient,item:&Item,now:i64)->Result<Option<i64>,tokio_postgres::Error> {
    if item.category_id!=7 || !item.singleton || item.quantity!=1 || !(11..=35).contains(&item.flag) {
        return Ok(None);
    }
    let history=load_history(db,item.owner_id,item.location_id,now).await?;
    let current=db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1 AND "typeID"=$2 AND "ownerID"=$3
        AND "locationID"=$4 AND flag=$5 AND singleton<>0 AND quantity=1"#,
        &[&(item.id as i32),&(item.type_id as i32),&(item.owner_id as i32),&(item.location_id as i32),&(item.flag as i32)]).await?;
    Ok(current.and_then(|_|history.module_ready.get(&item.id).copied()).map(|ready|ready-now))
}
