//! `repairSvc`: the station repair shop (base_repairshop.py), bound per station,
//! and repackaging from the item menu (menusvc.py UnasembleItems), unbound.
//!
//! Quotes include a ship's hull, armour, fitted modules and drones. Repair
//! prices remain zero until the original server's pricing formula is known.

use eve_marshal::Value;
use tracing::{error, info};

use crate::services::{Call, Ctx, Outcome};
use crate::svc::bound;
use crate::svc::inventory::{invitem_cells, item_change, INVITEM_SELECT};
use crate::svc::rows::{float, int, key_val};

const ATTR_DAMAGE: i32 = 3;
const ATTR_HP: i32 = 9;
const ATTR_ARMOR_DAMAGE: i32 = 266;
const ATTR_ARMOR_HP: i32 = 265;
/// Column position of `singleton` in an item row (const.ixSingleton).
const IX_SINGLETON: i64 = 6;

pub async fn repair_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let station = bound::parse(&call.service).and_then(|b| b.params.first().copied());
    if matches!(call.method.as_str(), "GetDamageReports" | "RepairItems" | "UnasembleItem" | "UnasembleItems")
        && (ctx.session.station_id.is_none() || station.is_some_and(|id| Some(id) != ctx.session.station_id))
    {
        return Some(Outcome::custom("Dock at this station to use its repair service."));
    }
    let value = match call.method.as_str() {
        "GetDamageReports" => damage_reports(ctx, &ids(call.args.first())).await,
        "RepairItems" => return Some(repair(ctx, &ids(call.args.first())).await),
        // UnasembleItem(itemID), on the station's repair shop.
        "UnasembleItem" => {
            let station = station.or(ctx.session.station_id)?;
            return Some(repackage(ctx, station, &[call.arg_id(0)]).await);
        }
        // UnasembleItems({stationID: [itemID]}), from the item menu.
        "UnasembleItems" => {
            let Some(Value::Dict(by_station)) = call.args.first() else { return Some(Outcome::Value(Value::None)) };
            for (station, items) in by_station {
                let outcome = repackage(ctx, station.as_i64().unwrap_or(0), &ids(Some(items))).await;
                if !matches!(outcome, Outcome::Value(_)) {
                    return Some(outcome);
                }
            }
            Value::None
        }
        _ => return None,
    };
    Some(Outcome::Value(value))
}

fn ids(v: Option<&Value>) -> Vec<i64> {
    v.and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64())
        .filter(|id| *id > 0 && i32::try_from(*id).is_ok()).collect()
}

/// An item at the station, or fitted to a ship docked there. Ownership must
/// hold for both the item and its containing ship.
const LOCAL_ITEMS: &str = r#"
    SELECT e."itemID" FROM entity e
     WHERE e."ownerID" = $2 AND (e."locationID" = $3 OR EXISTS (
        SELECT 1 FROM entity ship JOIN "invTypes" t ON t."typeID" = ship."typeID"
        JOIN "invGroups" g ON g."groupID" = t."groupID"
        WHERE ship."itemID" = e."locationID" AND ship."ownerID" = $2
          AND ship."locationID" = $3 AND g."categoryID" = 6))"#;

/// {itemID: KeyVal(quote=[KeyVal(itemID, typeID, damage, maxHealth,
/// costToRepairOneUnitOfDamage)])} for the caller's items
/// (base_repairshop.py:176-187).
async fn damage_reports(ctx: &Ctx<'_>, item_ids: &[i64]) -> Value {
    let (Some(me), Some(station)) = (ctx.session.char_id, ctx.session.station_id) else { return Value::Dict(Vec::new()) };
    let ids: Vec<i32> = item_ids.iter().map(|i| *i as i32).collect();
    let sql = format!(r#"
        WITH local_items AS ({LOCAL_ITEMS}), requested AS (
            SELECT "itemID" FROM local_items WHERE "itemID" = ANY($1))
        SELECT requested."itemID", e."itemID", e."typeID",
               coalesce(d."valueFloat", d."valueInt"::float8, 0)
                 + coalesce(ad."valueFloat", ad."valueInt"::float8, 0),
               coalesce(hp."value"::float8, 0) + coalesce(ahp."value"::float8, 0)
          FROM requested JOIN entity e ON e."itemID" = requested."itemID" OR
            (e."locationID" = requested."itemID" AND (e.flag BETWEEN 11 AND 35 OR e.flag BETWEEN 92 AND 99 OR e.flag = 87))
          JOIN local_items local ON local."itemID" = e."itemID"
          LEFT JOIN entity_attributes d ON d."itemID" = e."itemID" AND d."attributeID" = {ATTR_DAMAGE}
          LEFT JOIN entity_attributes ad ON ad."itemID" = e."itemID" AND ad."attributeID" = {ATTR_ARMOR_DAMAGE}
          LEFT JOIN "dgmTypeAttribs" hp ON hp."typeID" = e."typeID" AND hp."attributeID" = {ATTR_HP}
          LEFT JOIN "dgmTypeAttribs" ahp ON ahp."typeID" = e."typeID" AND ahp."attributeID" = {ATTR_ARMOR_HP}
          ORDER BY requested."itemID", e."itemID""#);
    let rows = ctx.query(&sql, &[&ids, &(me as i32), &(station as i32)]).await;
    let mut reports: std::collections::BTreeMap<i64, Vec<Value>> = std::collections::BTreeMap::new();
    for r in &rows {
            let item_id = int(r, 1).unwrap_or(0);
            let quote = key_val(vec![
                ("itemID", Value::Int(item_id)),
                ("typeID", Value::int_or_none(int(r, 2))),
                ("damage", Value::Double(float(r, 3).unwrap_or(0.0))),
                ("maxHealth", Value::Double(float(r, 4).unwrap_or(0.0))),
                ("costToRepairOneUnitOfDamage", Value::Double(0.0)),
            ]);
            reports.entry(int(r, 0).unwrap_or(0)).or_default().push(quote);
    }
    Value::Dict(reports.into_iter().map(|(id, quotes)| (Value::Int(id), key_val(vec![("quote", Value::List(quotes))]))).collect())
}

async fn repair(ctx: &Ctx<'_>, item_ids: &[i64]) -> Outcome {
    let (Some(me), Some(station)) = (ctx.session.char_id, ctx.session.station_id) else {
        return Outcome::custom("Dock at a station before repairing items.");
    };
    let ids: Vec<i32> = item_ids.iter().map(|i| *i as i32).collect();
    let result = async {
        let mut db = crate::db::connect(ctx.db_url).await?;
        let txn = db.transaction().await?;
        let sql = format!(r#"WITH local_items AS ({LOCAL_ITEMS})
            SELECT e."itemID" FROM entity e JOIN local_items local ON local."itemID" = e."itemID"
            WHERE e."itemID" = ANY($1) ORDER BY e."itemID" FOR UPDATE OF e"#);
        let allowed = txn.query(&sql, &[&ids, &(me as i32), &(station as i32)]).await?;
        let allowed: Vec<i32> = allowed.iter().map(|row| row.get(0)).collect();
        if ids.iter().any(|id| !allowed.contains(id)) { return Ok(None); }
        let changed = txn.query(r#"DELETE FROM entity_attributes WHERE "itemID" = ANY($1)
            AND "attributeID" IN (3,266) RETURNING "itemID", "attributeID", COALESCE("valueFloat", "valueInt"::float8)"#,
            &[&allowed]).await?;
        txn.commit().await?;
        Ok::<_, tokio_postgres::Error>(Some(changed))
    }.await;
    match result {
        Ok(Some(changed)) => {
            let now = crate::bulkdata::filetime_now();
            let changes = changed.iter().map(|row| Value::Tuple(vec![
                Value::bytes("OnModuleAttributeChange"), Value::Int(me), Value::Int(i64::from(row.get::<_, i32>(0))),
                Value::Int(i64::from(row.get::<_, i32>(1))), Value::Long(now), Value::Int(0), Value::Double(row.get(2)),
            ])).collect();
            ctx.world.lock().notify(&[ctx.client_id], "OnModuleAttributeChanges", Value::Tuple(vec![Value::List(changes)]));
            Outcome::Value(Value::None)
        }
        Ok(None) => Outcome::custom("You can only repair your items at this station."),
        Err(error) => {
            error!("station repair: {}", crate::logging::chain(&error));
            Outcome::custom("Your items could not be repaired.")
        }
    }
}

/// Repackage assembled items the caller owns in `station`: never the active
/// ship, and only once they are empty.
async fn repackage(ctx: &Ctx<'_>, station: i64, item_ids: &[i64]) -> Outcome {
    let Some(me) = ctx.session.char_id else { return Outcome::Value(Value::None) };
    if ctx.session.station_id != Some(station) {
        return Outcome::custom("Dock at this station before repackaging items.");
    }
    let ids: Vec<i32> = item_ids.iter().map(|i| *i as i32).collect();
    let active_ship = ctx.session.ship_id.map(|s| s as i32);

    let damaged = ctx.query_one(r#"SELECT count(*) FROM entity_attributes WHERE "itemID" = ANY($1)
        AND "attributeID" IN (3,266) AND COALESCE("valueFloat", "valueInt"::float8, 0) > 0"#, &[&ids])
        .await.and_then(|row| int(&row, 0)).unwrap_or(0);
    if damaged > 0 {
        return Outcome::custom("Repair damaged items before repackaging them.");
    }

    let sql = r#"SELECT count(*) FROM entity WHERE "locationID" = ANY($1)"#;
    let holding = ctx.query_one(sql, &[&ids]).await.and_then(|r| int(&r, 0)).unwrap_or(0);
    if holding > 0 {
        return Outcome::custom("Remove everything from an item before repackaging it.");
    }
    let sql = r#"UPDATE entity SET "singleton" = 0
                  WHERE "itemID" = ANY($1) AND "ownerID" = $2 AND "locationID" = $3
                    AND "singleton" = 1 AND "itemID" IS DISTINCT FROM $4
                RETURNING "itemID""#;
    let done: Vec<i32> = ctx
        .query(sql, &[&ids, &(me as i32), &(station as i32), &active_ship])
        .await
        .iter()
        .filter_map(|r| int(r, 0))
        .map(|i| i as i32)
        .collect();
    if done.is_empty() {
        return Outcome::Value(Value::None);
    }

    let sql = format!(r#"{INVITEM_SELECT} e."itemID" = ANY($1)"#);
    let rows = ctx.query(&sql, &[&done]).await;
    let world = ctx.world.lock();
    for r in &rows {
        let event = item_change(invitem_cells(r), vec![(IX_SINGLETON, Value::Int(1))]);
        world.notify(&[ctx.client_id], "OnItemChange", event);
    }
    info!("repackaged {} item(s) at station {station}", done.len());
    Outcome::Value(Value::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field<'a>(value: &'a Value, key: &str) -> &'a Value {
        let Value::Instance { state, .. } = value else { panic!("expected object") };
        let Value::Dict(fields) = state.as_ref() else { panic!("expected object state") };
        &fields.iter().find(|(name, _)| *name == Value::bytes(key)).expect("field").1
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE with imported static data"]
    async fn station_repairs_clear_hull_and_armour_and_refuse_remote_items() {
        let database = crate::test_support::Database::load().await;
        let owner = database.id().await;
        let ship = database.id().await;
        let module = database.id().await;
        let remote = database.id().await;
        for (id, type_id, location, flag) in [(ship, 606, 60014689, 4), (module, 3634, ship, 27), (remote, 606, 60000004, 4)] {
            database.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton)
                VALUES ($1,$2,$3,$4,$5,1,1)"#, &[&id, &type_id, &owner, &location, &flag]).await.unwrap();
        }
        for (id, attribute, value) in [(ship, 3, 15.0), (ship, 266, 25.0), (module, 3, 10.0), (remote, 3, 50.0)] {
            database.db.execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueFloat") VALUES ($1,$2,$3)"#,
                &[&id, &attribute, &value]).await.unwrap();
        }
        let session = crate::session::Session { char_id: Some(i64::from(owner)), station_id: Some(60014689), ..Default::default() };
        let ctx = database.ctx(&session);
        let Value::Dict(reports) = damage_reports(&ctx, &[i64::from(ship)]).await else { panic!("reports") };
        let quotes = field(&reports[0].1, "quote").seq().unwrap();
        assert_eq!(quotes.len(), 2, "ship and fitted module");
        let hull = quotes.iter().find(|quote| field(quote, "itemID") == &Value::Int(i64::from(ship))).unwrap();
        assert_eq!(field(hull, "damage"), &Value::Double(40.0), "hull plus armour");
        assert!(matches!(repair(&ctx, &[i64::from(ship), i64::from(remote)]).await, Outcome::UserError { .. }));
        let count = database.db.query_one(r#"SELECT count(*) FROM entity_attributes WHERE "itemID" = $1"#, &[&ship]).await.unwrap().get::<_, i64>(0);
        assert_eq!(count, 2, "invalid mixed requests cannot partly repair local items");
        assert!(matches!(repackage(&ctx, 60014689, &[i64::from(module)]).await, Outcome::UserError { .. }));
        assert!(matches!(repair(&ctx, &[i64::from(ship), i64::from(module)]).await, Outcome::Value(Value::None)));
        let count = database.db.query_one(r#"SELECT count(*) FROM entity_attributes WHERE "itemID" = ANY($1) AND "attributeID" IN (3,266)"#,
            &[&vec![ship, module]]).await.unwrap().get::<_, i64>(0);
        assert_eq!(count, 0);
        let remote_damage = database.db.query_one(r#"SELECT "valueFloat" FROM entity_attributes WHERE "itemID" = $1 AND "attributeID" = 3"#, &[&remote]).await.unwrap().get::<_, f64>(0);
        assert_eq!(remote_damage, 50.0);
    }
}
