use eve_marshal::Value;
use tracing::{error, warn};

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{cell, rowset};

const BOOKMARK_HEADER: &[&str] = &[
    "bookmarkID", "ownerID", "itemID", "typeID", "flag", "memo", "created", "x", "y", "z",
    "locationID", "note",
];

const BOOKMARK_SELECT: &str = r#"SELECT "bookmarkID","ownerID","itemID","typeID","flag","memo",
                                        "created","x","y","z","locationID","note"
                                   FROM bookmarks"#;

/// The longest note the client's bookmark windows take (addressbook.py).
const NOTE_MAX: usize = 6000;

fn clip_note(note: Option<String>) -> Option<String> {
    note.map(|n| n.chars().take(NOTE_MAX).collect())
}

pub async fn bookmark(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg = |i: usize| call.arg_i64(i);
    let value = match call.method.as_str() {
        "GetBookmarks" => get_bookmarks(ctx).await,
        // BookmarkLocation(itemID, ownerID, memo, note).
        "BookmarkLocation" => {
            let note = clip_note(call.arg_text(3)).unwrap_or_default();
            bookmark_location(ctx, arg(0).unwrap_or(0), call.arg_text(2), note).await
        }
        // UpdateBookmark(bookmarkID, ownerID, memo, note=note).
        "UpdateBookmark" => {
            let note = clip_note(call.kwarg("note").and_then(|v| v.as_string()).or_else(|| call.arg_text(3)));
            update_bookmark(ctx, arg(0).unwrap_or(0), call.arg_text(2), note).await
        }
        "DeleteBookmarks" => delete_bookmarks(ctx, call.args.first()).await,

        // CreateBookmarkVoucher(bookmarkID, memo) -> the voucher, which the
        // container it was dropped on then takes (container.py:457).
        "CreateBookmarkVoucher" => match make_vouchers(ctx, &[(arg(0).unwrap_or(0), call.arg_text(1))]).await {
            Ok(mut made) => made.pop().unwrap_or(Value::None),
            Err(refusal) => return Some(refusal),
        },
        // CreateBookmarkVouchers([(bookmarkID, memo) or (bookmarkID,)]).
        "CreateBookmarkVouchers" => {
            let wanted: Vec<(i64, Option<String>)> = call
                .args
                .first()
                .and_then(|v| v.seq())
                .unwrap_or(&[])
                .iter()
                .filter_map(|p| {
                    let p = p.seq()?;
                    Some((p.first()?.as_i64()?, p.get(1).and_then(|m| m.as_string())))
                })
                .collect();
            match make_vouchers(ctx, &wanted).await {
                Ok(made) => Value::List(made),
                Err(refusal) => return Some(refusal),
            }
        }
        // AddBookmarkFromVoucher(voucherID): the voucher's bookmark, copied
        // into the caller's places; the voucher stays.
        "AddBookmarkFromVoucher" => from_voucher(ctx, arg(0).unwrap_or(0)).await,

        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// typeBookmark, of groupVoucher.
const TYPE_BOOKMARK: i64 = 51;
const FLAG_HANGAR: i64 = 4;
const FLAG_CARGO: i64 = 5;

/// Bookmarks made into vouchers: in the station hangar when docked, else in
/// the ship's hold. Each answer carries its voucherID.
async fn make_vouchers(ctx: &Ctx<'_>, wanted: &[(i64, Option<String>)]) -> Result<Vec<Value>, Outcome> {
    let Some(me) = ctx.session.char_id else { return Err(Outcome::custom("No character.")) };
    let (location, flag) = match (ctx.session.station_id, ctx.session.ship_id) {
        (Some(station), _) => (station, FLAG_HANGAR),
        (None, Some(ship)) => (ship, FLAG_CARGO),
        _ => return Err(Outcome::custom("You have nowhere to keep a voucher.")),
    };
    let made = async {
        let mut client = crate::db::connect(ctx.db_url).await?;
        let mut txn = crate::items::ItemTxn::begin(&mut client).await?;
        let mut out = Vec::new();
        for (bookmark_id, memo) in wanted {
            let sql = r#"SELECT "itemID","typeID","flag","memo","created","x","y","z","locationID","note" FROM bookmarks
                          WHERE "bookmarkID" = $1 AND "ownerID" = $2"#;
            let Some(b) = txn.tx().query_opt(sql, &[&(*bookmark_id as i32), &(me as i32)]).await? else {
                out.push(Value::None);
                continue;
            };
            let id = txn.create(crate::items::NewItem { type_id: TYPE_BOOKMARK, owner_id: me, location_id: location, flag, quantity: 1, singleton: true }).await?;
            let memo: String = memo.clone().unwrap_or_else(|| b.get(3));
            let sql = r#"INSERT INTO "bookmarkVouchers" ("voucherID","itemID","typeID","flag","memo","created","x","y","z","locationID","note")
                         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)"#;
            let (item, type_id, bflag): (i32, i32, Option<i32>) = (b.get(0), b.get(1), b.get(2));
            let (created, x, y, z, loc): (i64, Option<f64>, Option<f64>, Option<f64>, i32) = (b.get(4), b.get(5), b.get(6), b.get(7), b.get(8));
            let note: String = b.get(9);
            txn.tx().execute(sql, &[&(id as i32), &item, &type_id, &bflag, &memo, &created, &x, &y, &z, &loc, &note]).await?;
            out.push(crate::svc::rows::key_val(vec![("voucherID", Value::Int(id))]));
        }
        let events = txn.commit().await?;
        Ok::<_, tokio_postgres::Error>((out, events))
    }
    .await;
    match made {
        Ok((out, events)) => {
            crate::items::notify(ctx.world, ctx.client_id, events);
            let count = out.iter().filter(|v| !matches!(v, Value::None)).count();
            tracing::info!("{me} made {count} bookmark voucher(s)");
            Ok(out)
        }
        Err(e) => {
            error!("making bookmark vouchers: {}", crate::logging::chain(&e));
            Err(Outcome::custom("The voucher could not be made."))
        }
    }
}

async fn from_voucher(ctx: &Ctx<'_>, voucher_id: i64) -> Value {
    let Some(me) = ctx.session.char_id else { return Value::None };
    let sql = format!(
        r#"WITH v AS (SELECT b.* FROM "bookmarkVouchers" b JOIN entity e ON e."itemID" = b."voucherID"
                       WHERE b."voucherID" = $1 AND e."ownerID" = $2)
           INSERT INTO bookmarks ("ownerID","itemID","typeID","flag","memo","created","x","y","z","locationID","note")
           SELECT $2, "itemID","typeID","flag","memo","created","x","y","z","locationID","note" FROM v
           RETURNING "bookmarkID","ownerID","itemID","typeID","flag","memo","created","x","y","z","locationID","note""#
    );
    match ctx.query_one(sql.as_str(), &[&(voucher_id as i32), &(me as i32)]).await {
        Some(r) => crate::svc::rows::row(BOOKMARK_HEADER, crate::svc::rows::line(&r)),
        None => Value::None,
    }
}

/// `voucher`: GetObject(voucherID), the voucher.Bookmark the client keeps
/// (voucherCache.py): its item row as header and data, and the memo it
/// shows as its name.
pub async fn voucher(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if call.method != "GetObject" {
        return None;
    }
    let id = call.arg_id(0) as i32;
    let sql = format!(r#"{} e."itemID" = $1"#, crate::svc::inventory::INVITEM_SELECT);
    let Some(item) = ctx.query_one(sql.as_str(), &[&id]).await else { return Some(Outcome::Value(Value::None)) };
    let memo = ctx
        .query_one(r#"SELECT "memo" FROM "bookmarkVouchers" WHERE "voucherID" = $1"#, &[&id])
        .await
        .and_then(|r| crate::svc::rows::text(&r, 0))
        .unwrap_or_else(|| "Bookmark".into());
    let header = Value::List(crate::svc::inventory::INVITEM_HEADER.iter().map(|h| Value::bytes(*h)).collect());
    let state = Value::Dict(vec![
        (Value::bytes("header"), header),
        (Value::bytes("data"), Value::List(crate::svc::inventory::invitem_cells(&item))),
        (Value::bytes("description"), Value::text(memo)),
    ]);
    Some(Outcome::Value(Value::Instance { class: Box::new(Value::bytes("voucher.Bookmark")), state: Box::new(state) }))
}

async fn get_bookmarks(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return rowset(BOOKMARK_HEADER, Vec::new()) };
    let sql = format!(r#"{BOOKMARK_SELECT} WHERE "ownerID" = $1 ORDER BY "bookmarkID""#);
    ctx.rowset(BOOKMARK_HEADER, sql.as_str(), &[&(char_id as i32)]).await
}

const GROUP_REGION: i32 = 3;
const GROUP_CONSTELLATION: i32 = 4;
const GROUP_SOLAR_SYSTEM: i32 = 5;
const LOCATION_UNIVERSE: i32 = 9;
const TYPE_SOLAR_SYSTEM: i32 = 5;

/// Where `item_id` is now in the caller's system: their own ship (also for
/// the system itself, the spot they are at), or another ball they can see.
fn ball_position(ctx: &Ctx<'_>, item_id: i64) -> Option<(i32, crate::ballpark::Vec3)> {
    let system = ctx.session.solar_system_id?;
    let ship = ctx.session.ship_id?;
    let target = if item_id == system { ship } else { item_id };
    let mut world = ctx.world.lock();
    let park = world.parks.get_mut(&system)?;
    park.advance(crate::destiny::tick_now());
    if target != ship
        && (park.ships.get(&target).is_some_and(|s| s.cloaked) || park.beacons.get(&target).is_some_and(|b| b.hidden))
    {
        return None;
    }
    let (at, _) = park.ball(target)?;
    Some((i32::try_from(system).ok()?, at))
}

async fn bookmark_location(ctx: &Ctx<'_>, item_id: i64, memo: Option<String>, note: String) -> Value {
    let failed = || {
        Value::Tuple(vec![
            Value::Int(0),
            Value::Int(item_id),
            Value::None,
            Value::None,
            Value::None,
            Value::None,
            Value::None,
        ])
    };
    let Some(char_id) = ctx.session.char_id else { return failed() };

    let sql = r#"SELECT "typeID","solarSystemID","x","y","z",
                        "groupID","constellationID","regionID"
                   FROM "mapDenormalize" WHERE "itemID" = $1"#;
    let found = match ctx.db.query(sql, &[&(item_id as i32)]).await {
        Ok(rows) => rows.into_iter().next(),
        Err(e) => {
            error!("BookmarkLocation({item_id}) lookup: {err}", err = crate::logging::chain(&e));
            return failed();
        }
    };
    let Some(r) = found else {
        // Not a celestial: "Save Location" in space (the client sends the
        // ship or the system, addressbook.BookmarkCurrentLocation) or a
        // ball in the park. It becomes a coordinate bookmark -- itemID and
        // locationID the system, typeSolarSystem -- since the client takes
        // a bookmark whose itemID is still a ball to be that ball's current
        // position (menusvc CelestialMenu).
        let Some((system, at)) = ball_position(ctx, item_id) else {
            warn!("BookmarkLocation({item_id}): neither a celestial nor in the caller's ballpark");
            return failed();
        };
        let insert = r#"INSERT INTO bookmarks
                            ("ownerID","itemID","typeID","flag","memo","created","x","y","z","locationID","note")
                        VALUES ($1,$2,$3,NULL,$4,$5,$6,$7,$8,$2,$9)
                        RETURNING "bookmarkID""#;
        let memo = memo.unwrap_or_default();
        let created = crate::bulkdata::filetime_now();
        let params: [&(dyn tokio_postgres::types::ToSql + Sync); 9] =
            [&(char_id as i32), &system, &TYPE_SOLAR_SYSTEM, &memo, &created, &at.x, &at.y, &at.z, &note];
        return match ctx.db.query_one(insert, &params).await {
            Ok(row) => Value::Tuple(vec![
                cell(&row, 0),
                Value::Int(system as i64),
                Value::Int(TYPE_SOLAR_SYSTEM as i64),
                Value::Double(at.x),
                Value::Double(at.y),
                Value::Double(at.z),
                Value::Int(system as i64),
            ]),
            Err(e) => {
                error!("BookmarkLocation({item_id}) insert: {err}", err = crate::logging::chain(&e));
                failed()
            }
        };
    };

    let type_id: i32 = r.get(0);
    let col = |i: usize| r.try_get::<_, Option<i32>>(i).ok().flatten();
    // A system, constellation or region is filed under its parent with no
    // position: the client offers "Warp to" for any bookmark whose locationID
    // is the current system and warps to its x/y/z (menusvc CelestialMenu),
    // so a system filed under a system would warp to galactic coordinates.
    let (location_id, at) = match col(5) {
        Some(GROUP_SOLAR_SYSTEM) => (col(6), None),
        Some(GROUP_CONSTELLATION) => (col(7), None),
        Some(GROUP_REGION) => (Some(LOCATION_UNIVERSE), None),
        _ => (col(1), Some((r.get::<_, f64>(2), r.get::<_, f64>(3), r.get::<_, f64>(4)))),
    };
    let location_id = location_id
        .or_else(|| ctx.session.solar_system_id2.and_then(|v| i32::try_from(v).ok()))
        .unwrap_or(0);
    let (x, y, z) = (at.map(|p| p.0), at.map(|p| p.1), at.map(|p| p.2));
    let coord = |v: Option<f64>| v.map(Value::Double).unwrap_or(Value::None);

    let insert = r#"INSERT INTO bookmarks
                        ("ownerID","itemID","typeID","flag","memo","created","x","y","z","locationID","note")
                    VALUES ($1,$2,$3,NULL,$4,$5,$6,$7,$8,$9,$10)
                    RETURNING "bookmarkID""#;
    let memo = memo.unwrap_or_default();
    let created = crate::bulkdata::filetime_now();
    match ctx
        .db
        .query_one(
            insert,
            &[
                &(char_id as i32),
                &(item_id as i32),
                &type_id,
                &memo,
                &created,
                &x,
                &y,
                &z,
                &location_id,
                &note,
            ],
        )
        .await
    {
        Ok(row) => Value::Tuple(vec![
            cell(&row, 0),
            Value::Int(item_id),
            Value::Int(type_id as i64),
            coord(x),
            coord(y),
            coord(z),
            Value::Int(location_id as i64),
        ]),
        Err(e) => {
            error!("BookmarkLocation({item_id}) insert: {err}", err = crate::logging::chain(&e));
            failed()
        }
    }
}

async fn update_bookmark(ctx: &Ctx<'_>, bookmark_id: i64, memo: Option<String>, note: Option<String>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    if memo.is_none() && note.is_none() {
        return Value::None;
    }
    let sql = r#"UPDATE bookmarks SET "memo" = COALESCE($1, "memo"), "note" = COALESCE($2, "note")
                  WHERE "bookmarkID" = $3 AND "ownerID" = $4"#;
    if let Err(e) = ctx.db.execute(sql, &[&memo, &note, &(bookmark_id as i32), &(char_id as i32)]).await {
        error!("UpdateBookmark({bookmark_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

async fn delete_bookmarks(ctx: &Ctx<'_>, arg: Option<&Value>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let ids: Vec<i32> = arg
        .and_then(|v| v.seq())
        .map(|s| s.iter().filter_map(|v| v.as_i64()).filter_map(|v| i32::try_from(v).ok()).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        return Value::None;
    }
    let sql = r#"DELETE FROM bookmarks
                  WHERE "bookmarkID" = ANY($1) AND "ownerID" = $2"#;
    if let Err(e) = ctx.db.execute(sql, &[&ids, &(char_id as i32)]).await {
        error!("DeleteBookmarks({} ids): {err}", ids.len(), err = crate::logging::chain(&e));
    }
    Value::None
}

