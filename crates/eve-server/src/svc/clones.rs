//! `jumpCloneSvc`, bound as (locationID, groupID): jump clones
//! (clonejumpsvc.py, medical.py, charactersheet.py).
//!
//! A jump clone is a body waiting somewhere, with its own implants (items
//! located on the clone, flag implant). A pilot docked in a station with a
//! clone bay installs one there (`InstallCloneInStation`, for a fee and as
//! many as Infomorph Psychology allows); a ship with a clone vat bay fitted
//! carries them for others (`OfferShipCloneInstallation`, accepted by the
//! pilot asked). `CloneJump` swaps bodies: the pilot's current body, with its
//! implants, stays behind as a jump clone in the station they leave, and they
//! wake in the clone they jump to, with its implants, in a fresh capsule.
//! It can be done once a day.
//!
//! Choices the client leaves to the server (the rules of the time):
//! - NPC stations take clones from pilots their owner corporation stands at
//!   `game.jumpCloneMinStanding` or better (8.0); stations of the pilot's own
//!   corporation or alliance take them from anyone;
//! - one clone per pilot per station;
//! - a clone left behind replaces one already at the station only when the
//!   pilot agrees (JumpCheckWillLoseExistingClone[AndImplants]).

use eve_marshal::Value;
use tokio_postgres::GenericClient;
use tracing::info;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{float, int, rowset};
use crate::world::Move;

const CLONE_HEADER: &[&str] = &["jumpCloneID", "locationID"];
const IMPLANT_HEADER: &[&str] = &["jumpCloneID", "typeID"];
const SHIP_CLONE_HEADER: &[&str] = &["jumpCloneID", "ownerID", "locationID"];

/// What installing a clone costs.
pub const PRICE: f64 = 900_000.0;
/// One jump a day.
const COOLDOWN_FT: i64 = 24 * 3_600 * 10_000_000;
const FLAG_IMPLANT: i32 = 89;
const FLAG_HANGAR: i32 = 4;
const SKILL_INFOMORPH_PSYCHOLOGY: i32 = 24242;
const SERVICE_CLONING: i32 = 512;
const SERVICE_JUMP_CLONE: i32 = 8_388_608;
pub const CLONE_VAT_BAYS: [i32; 2] = [23735, 24547];
const TYPE_CAPSULE: i32 = 670;
/// market_refTypes: "Jump Clone Installation Fee" is not in the table of the
/// time; the clone fee goes as "Clone Activation".
const REF_CLONE: i32 = 8;
/// Fitted modules sit in the low, medium and high slots.
const SLOT_FLAGS: (i32, i32) = (11, 34);

type Refused<T> = Result<T, Outcome>;

fn refuse<T>(text: &str) -> Refused<T> {
    Err(Outcome::custom(text))
}

fn db_err(e: tokio_postgres::Error) -> Outcome {
    tracing::error!("jump clones: {}", crate::logging::chain(&e));
    Outcome::custom("The clone bay could not do that just now.")
}

pub async fn jump_clone_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let Some(me) = ctx.session.char_id else { return Some(Outcome::custom("Select a character first.")) };
    let result = match call.method.as_str() {
        "GetCloneState" => clone_state(ctx, me).await,
        "GetShipCloneState" => Ok(clones_at(ctx, ctx.session.ship_id).await),
        "GetStationCloneState" => Ok(clones_at(ctx, ctx.session.station_id).await),
        "GetPriceForClone" => Ok(Value::Double(PRICE)),
        "InstallCloneInStation" => install(ctx, me).await,
        "DestroyInstalledClone" => destroy(ctx, me, call.arg_id(0)).await,
        "CloneJump" => jump(ctx, me, call.arg_id(0), call.arg_i64(1).unwrap_or(0) != 0).await,
        "OfferShipCloneInstallation" => offer(ctx, me, call.arg_id(0)).await,
        "AcceptShipCloneInstallation" => accept(ctx, me).await,
        "CancelShipCloneInstallation" => {
            cancel(ctx, me);
            Ok(Value::None)
        }
        _ => return None,
    };
    Some(result.map_or_else(|o| o, Outcome::Value))
}

/// (the pilot's clones, their implants).
async fn clone_state(ctx: &Ctx<'_>, me: i64) -> Refused<Value> {
    let clones = ctx
        .rowset(CLONE_HEADER, r#"SELECT "jumpCloneID", "locationID" FROM "chrJumpClones" WHERE "ownerID" = $1 ORDER BY 1"#, &[&(me as i32)])
        .await;
    let implants = ctx
        .rowset(
            IMPLANT_HEADER,
            r#"SELECT j."jumpCloneID", e."typeID" FROM "chrJumpClones" j JOIN entity e ON e."locationID" = j."jumpCloneID"
                WHERE j."ownerID" = $1 AND e."flag" = $2 ORDER BY 1, 2"#,
            &[&(me as i32), &FLAG_IMPLANT],
        )
        .await;
    Ok(Value::Tuple(vec![clones, implants]))
}

/// Everyone's clones at a station or in a ship.
async fn clones_at(ctx: &Ctx<'_>, location: Option<i64>) -> Value {
    let Some(location) = location else { return rowset(SHIP_CLONE_HEADER, Vec::new()) };
    ctx.rowset(
        SHIP_CLONE_HEADER,
        r#"SELECT "jumpCloneID", "ownerID", "locationID" FROM "chrJumpClones" WHERE "locationID" = $1 ORDER BY 1"#,
        &[&(location as i32)],
    )
    .await
}

/// How many clones a pilot may keep: a level of Infomorph Psychology each.
async fn allowed(db: &impl GenericClient, me: i64) -> Result<i64, tokio_postgres::Error> {
    Ok(crate::svc::skills::levels(db, me, &[SKILL_INFOMORPH_PSYCHOLOGY]).await?.get(&SKILL_INFOMORPH_PSYCHOLOGY).copied().unwrap_or(0))
}

async fn count(db: &impl GenericClient, me: i64) -> Result<i64, tokio_postgres::Error> {
    Ok(db.query_one(r#"SELECT count(*) FROM "chrJumpClones" WHERE "ownerID" = $1"#, &[&(me as i32)]).await?.get(0))
}

fn invalidated(ctx: &Ctx<'_>, owner: i64, location: i64, station: bool) {
    let w = ctx.world.lock();
    w.notify(&w.clients_of(owner), "OnJumpCloneCacheInvalidated", Value::Tuple(Vec::new()));
    let event = if station { "OnStationJumpCloneCacheInvalidated" } else { "OnShipJumpCloneCacheInvalidated" };
    let here: Vec<i64> = w.players().iter().filter(|p| p.station_id == Some(location) || p.solar_system_id == location).map(|p| p.char_id).flat_map(|c| w.clients_of(c)).collect();
    w.notify(&here, event, Value::Tuple(vec![Value::Int(location), Value::Int(owner)]));
}

async fn install(ctx: &Ctx<'_>, me: i64) -> Refused<Value> {
    let Some(station) = ctx.session.station_id else { return refuse("You must be docked to install a clone.") };
    let row = ctx
        .db
        .query_opt(
            r#"SELECT s."corporationID", COALESCE((SELECT bit_or(os."serviceID") FROM "staOperationServices" os WHERE os."operationID" = s."operationID"), 0),
                      COALESCE((SELECT n."standing" FROM "chrNPCStandings" n WHERE n."characterID" = $2 AND n."fromID" = s."corporationID"), 0)::float8,
                      COALESCE(co."allianceID", 0)
                 FROM "staStations" s LEFT JOIN "corporation" co ON co."corporationID" = s."corporationID"
                WHERE s."stationID" = $1"#,
            &[&(station as i32), &(me as i32)],
        )
        .await
        .map_err(db_err)?;
    let Some(row) = row else { return refuse("This station has no clone bay.") };
    let (owner, services, standing, owner_alliance) = (int(&row, 0).unwrap_or(0), int(&row, 1).unwrap_or(0), float(&row, 2).unwrap_or(0.0), int(&row, 3).unwrap_or(0));
    let ours = Some(owner) == ctx.session.corp_id || (owner_alliance != 0 && Some(owner_alliance) == ctx.session.alliance_id);
    if services & (SERVICE_CLONING | SERVICE_JUMP_CLONE) as i64 == 0 {
        return refuse("This station has no clone bay.");
    }
    let need = crate::config::jump_clone_min_standing();
    if !ours && standing < need {
        return refuse(&format!("The station's owners install clones only for pilots they stand at {need:.1} or better towards; they stand at {standing:.1} towards you."));
    }
    let here = ctx
        .db
        .query_opt(r#"SELECT 1 FROM "chrJumpClones" WHERE "ownerID" = $1 AND "locationID" = $2"#, &[&(me as i32), &(station as i32)])
        .await
        .map_err(db_err)?;
    if here.is_some() {
        return refuse("You already have a clone at this station.");
    }
    let (have, may) = (count(ctx.db, me).await.map_err(db_err)?, allowed(ctx.db, me).await.map_err(db_err)?);
    if have >= may {
        return refuse(&format!("You have {have} jump clone(s), as many as your Infomorph Psychology allows."));
    }
    pay(ctx, me, PRICE).await?;
    ctx.db
        .execute(r#"INSERT INTO "chrJumpClones" ("ownerID","locationID","createdAt") VALUES ($1, $2, $3)"#, &[&(me as i32), &(station as i32), &crate::bulkdata::filetime_now()])
        .await
        .map_err(db_err)?;
    info!("{me} installed a jump clone at station {station}");
    invalidated(ctx, me, station, true);
    Ok(Value::None)
}

async fn pay(ctx: &Ctx<'_>, me: i64, amount: f64) -> Refused<()> {
    use crate::svc::wallet::{move_cash, Payment, Transfer};
    let payment = Payment { payer: me, payee: None, amount, ref_type: REF_CLONE, arg_id1: "", reason: "Jump clone installation" };
    match move_cash(ctx.db_url, payment).await {
        Ok(()) => {
            tell_balance(ctx, me).await;
            Ok(())
        }
        Err(Transfer::NotEnough { need, have }) => Err(Outcome::not_enough_money(need, have)),
        Err(Transfer::NoSuchOwner) => refuse("No wallet."),
        Err(Transfer::Db(e)) => Err(db_err(e)),
    }
}

async fn tell_balance(ctx: &Ctx<'_>, who: i64) {
    let balance = ctx.query_one(r#"SELECT "balance" FROM "character_" WHERE "characterID" = $1"#, &[&(who as i32)]).await.and_then(|r| float(&r, 0));
    if let Some(balance) = balance {
        let w = ctx.world.lock();
        w.notify(&w.clients_of(who), "OnAccountChange", Value::Tuple(vec![Value::bytes("cash"), Value::Int(who), Value::Double(balance)]));
    }
}

/// A clone and its implants, gone.
async fn remove(db: &impl GenericClient, clone: i64) -> Result<(), tokio_postgres::Error> {
    db.execute(r#"DELETE FROM entity WHERE "locationID" = $1"#, &[&(clone as i32)]).await?;
    db.execute(r#"DELETE FROM "chrJumpClones" WHERE "jumpCloneID" = $1"#, &[&(clone as i32)]).await?;
    Ok(())
}

/// Jump clones carried in a ship, gone with it.
pub async fn ship_destroyed(db: &impl GenericClient, ship: i64) -> Result<u64, tokio_postgres::Error> {
    db.execute(r#"DELETE FROM entity WHERE "locationID" IN (SELECT "jumpCloneID" FROM "chrJumpClones" WHERE "locationID" = $1)"#, &[&(ship as i32)]).await?;
    db.execute(r#"DELETE FROM "chrJumpClones" WHERE "locationID" = $1"#, &[&(ship as i32)]).await
}

/// DestroyInstalledClone(cloneID): one's own clone where one is, or anyone's
/// in the ship one flies.
async fn destroy(ctx: &Ctx<'_>, me: i64, clone: i64) -> Refused<Value> {
    let row = ctx
        .db
        .query_opt(r#"SELECT "ownerID", "locationID" FROM "chrJumpClones" WHERE "jumpCloneID" = $1"#, &[&(clone as i32)])
        .await
        .map_err(db_err)?;
    let Some(row) = row else { return refuse("That clone no longer exists.") };
    let (owner, location) = (int(&row, 0).unwrap_or(0), int(&row, 1).unwrap_or(0));
    let mine_here = owner == me && ctx.session.station_id == Some(location);
    let in_my_ship = ctx.session.ship_id == Some(location);
    if !mine_here && !in_my_ship {
        return refuse("You can only destroy your clone at the station you are in, or clones in your own ship.");
    }
    remove(ctx.db, clone).await.map_err(db_err)?;
    info!("{me} destroyed jump clone {clone} of {owner} at {location}");
    invalidated(ctx, owner, location, !in_my_ship);
    Ok(Value::None)
}

/// CloneJump(destLocationID, force).
async fn jump(ctx: &Ctx<'_>, me: i64, dest: i64, force: bool) -> Refused<Value> {
    let Some(here) = ctx.session.station_id else { return refuse("You must be docked to clone jump.") };
    if dest == here {
        return refuse("You are already at that clone.");
    }
    let row = ctx
        .db
        .query_opt(
            r#"SELECT j."jumpCloneID", c."lastCloneJump" FROM "chrJumpClones" j JOIN "character_" c ON c."characterID" = j."ownerID"
                WHERE j."ownerID" = $1 AND j."locationID" = $2"#,
            &[&(me as i32), &(dest as i32)],
        )
        .await
        .map_err(db_err)?;
    let Some(row) = row else { return refuse("You have no clone there.") };
    let (target, last) = (int(&row, 0).unwrap_or(0), int(&row, 1).unwrap_or(0));
    let now = crate::bulkdata::filetime_now();
    if last + COOLDOWN_FT > now {
        let hours = (last + COOLDOWN_FT - now) as f64 / 36_000_000_000.0;
        return refuse(&format!("Your body has not recovered from your last clone jump: you can jump again in {hours:.1} hours."));
    }
    // A clone already here would be replaced by the body left behind.
    let existing = ctx
        .db
        .query_opt(
            r#"SELECT j."jumpCloneID", (SELECT count(*) FROM entity e WHERE e."locationID" = j."jumpCloneID") FROM "chrJumpClones" j
                WHERE j."ownerID" = $1 AND j."locationID" = $2"#,
            &[&(me as i32), &(here as i32)],
        )
        .await
        .map_err(db_err)?;
    if let Some(e) = &existing {
        if !force {
            let msg = if int(e, 1).unwrap_or(0) > 0 { "JumpCheckWillLoseExistingCloneAndImplants" } else { "JumpCheckWillLoseExistingClone" };
            return Err(Outcome::UserError { msg: msg.into(), args: Vec::new() });
        }
    }
    // Where the clone waits: a station, or a ship (docked, or in space).
    let place = ctx
        .db
        .query_opt(
            r#"SELECT s."stationID", s."solarSystemID", s."constellationID", s."regionID", NULL::float8, NULL::float8, NULL::float8
                 FROM "staStations" s WHERE s."stationID" = $1
               UNION ALL
               SELECT st."stationID", m."solarSystemID", m."constellationID", m."regionID", e."x", e."y", e."z"
                 FROM entity e
                 LEFT JOIN "staStations" st ON st."stationID" = e."locationID"
                 JOIN "mapSolarSystems" m ON m."solarSystemID" = COALESCE(st."solarSystemID", e."locationID")
                WHERE e."itemID" = $1
               LIMIT 1"#,
            &[&(dest as i32)],
        )
        .await
        .map_err(db_err)?;
    let Some(place) = place else { return refuse("Your clone's whereabouts are unknown.") };
    let station = int(&place, 0);
    let (system, constellation, region) = (int(&place, 1).unwrap_or(0), int(&place, 2).unwrap_or(0), int(&place, 3).unwrap_or(0));
    let at = (float(&place, 4).unwrap_or(0.0), float(&place, 5).unwrap_or(0.0), float(&place, 6).unwrap_or(0.0));

    let mut client = crate::db::connect(ctx.db_url).await.map_err(db_err)?;
    let tx = client.transaction().await.map_err(db_err)?;
    if let Some(e) = &existing {
        remove(&tx, int(e, 0).unwrap_or(0)).await.map_err(db_err)?;
    }
    // The body left behind, with its implants.
    let left: i32 = tx
        .query_one(r#"INSERT INTO "chrJumpClones" ("ownerID","locationID","createdAt") VALUES ($1, $2, $3) RETURNING "jumpCloneID""#, &[&(me as i32), &(here as i32), &now])
        .await
        .map_err(db_err)?
        .get(0);
    tx.execute(r#"UPDATE entity SET "locationID" = $2 WHERE "locationID" = $1 AND "flag" = $3"#, &[&(me as i32), &left, &FLAG_IMPLANT]).await.map_err(db_err)?;
    // The body woken in, with its implants.
    tx.execute(r#"UPDATE entity SET "locationID" = $1 WHERE "locationID" = $2 AND "flag" = $3"#, &[&(me as i32), &(target as i32), &FLAG_IMPLANT]).await.map_err(db_err)?;
    tx.execute(r#"DELETE FROM "chrJumpClones" WHERE "jumpCloneID" = $1"#, &[&(target as i32)]).await.map_err(db_err)?;
    // A fresh capsule, in the station's hangar or in space by the ship.
    let (location, flag) = match station {
        Some(s) => (s, FLAG_HANGAR),
        None => (system, 0),
    };
    let capsule: i32 = tx
        .query_one(
            r#"INSERT INTO entity ("itemID","itemName","typeID","ownerID","locationID","flag","contraband","singleton","quantity","x","y","z")
               VALUES (nextval('dyn_id_seq'), '', $1, $2, $3, $4, 0, 1, 1, $5, $6, $7) RETURNING "itemID""#,
            &[&TYPE_CAPSULE, &(me as i32), &(location as i32), &flag, &at.0, &at.1, &at.2],
        )
        .await
        .map_err(db_err)?
        .get(0);
    tx.execute(
        r#"UPDATE "character_" SET "stationID" = $2, "solarSystemID" = $3, "constellationID" = $4, "regionID" = $5, "shipID" = $6, "lastCloneJump" = $7
            WHERE "characterID" = $1"#,
        &[&(me as i32), &station.map(|s| s as i32), &(system as i32), &(constellation as i32), &(region as i32), &capsule, &now],
    )
    .await
    .map_err(db_err)?;
    tx.execute(r#"UPDATE entity SET "locationID" = $2 WHERE "itemID" = $1"#, &[&(me as i32), &(location as i32)]).await.map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    info!("{me} clone jumped from station {here} to {dest}, in capsule {capsule}");

    match station {
        Some(station_id) => {
            ctx.world.lock().push_move(ctx.client_id, Move::Relocate { station_id: Some(station_id), solar_system_id: system, constellation_id: constellation, region_id: region, ship_id: capsule as i64 });
        }
        None => {
            // Out of the ship's clone vat bay, into space beside it.
            let pos = crate::ballpark::Vec3::new(at.0, at.1, at.2) + crate::ballpark::Vec3::new(2_500.0, 0.0, 0.0);
            space_capsule(ctx, me, capsule as i64, system, pos).await;
            ctx.world.lock().push_move(ctx.client_id, Move::Relocate { station_id: None, solar_system_id: system, constellation_id: constellation, region_id: region, ship_id: capsule as i64 });
        }
    }
    invalidated(ctx, me, here, true);
    Ok(Value::None)
}

/// Puts a pilot's new capsule in space, before their session moves there.
async fn space_capsule(ctx: &Ctx<'_>, me: i64, capsule: i64, system: i64, pos: crate::ballpark::Vec3) {
    crate::svc::space::ensure_park(ctx, system).await;
    let profile = crate::svc::inflight::profile(ctx.db, ctx.dogma, me, capsule, &Default::default(), crate::combat::now_ms()).await;
    if let Ok(Some(p)) = profile {
        let mut ship = crate::ballpark::Ship::new(capsule, ctx.client_id, p.stats, p.pilot, pos, crate::ballpark::Vec3::ZERO);
        ship.fight = Some(p.fighter);
        crate::svc::space::add_ship(&mut ctx.world.lock(), system, ship);
    }
}

/// Whether a ship has one of `types` fitted.
pub async fn has_module(db: &impl GenericClient, ship: i64, types: &[i32]) -> Result<bool, tokio_postgres::Error> {
    let row = db
        .query_opt(
            r#"SELECT 1 FROM entity WHERE "locationID" = $1 AND "typeID" = ANY($2) AND "flag" BETWEEN $3 AND $4 LIMIT 1"#,
            &[&(ship as i32), &types, &SLOT_FLAGS.0, &SLOT_FLAGS.1],
        )
        .await?;
    Ok(row.is_some())
}

fn offer_args(offerer: i64, target: i64, ship: i64) -> Value {
    Value::Tuple(vec![Value::Tuple(vec![Value::Int(offerer), Value::Int(target), Value::Int(ship), Value::None])])
}

/// OfferShipCloneInstallation(charID): a pilot whose ship has a clone vat
/// bay offers another pilot in the same system a clone in it.
async fn offer(ctx: &Ctx<'_>, me: i64, target: i64) -> Refused<Value> {
    let Some(ship) = ctx.session.ship_id else { return refuse("You are not in a ship.") };
    if !has_module(ctx.db, ship, &CLONE_VAT_BAYS).await.map_err(db_err)? {
        return refuse("Your ship has no clone vat bay.");
    }
    let mut w = ctx.world.lock();
    let Some(there) = w.presence_of(target) else { return refuse("That pilot is not online.") };
    if Some(there.solar_system_id) != ctx.session.solar_system_id2.or(ctx.session.solar_system_id) {
        return refuse("That pilot is not in this system.");
    }
    w.clone_offers.insert(target, (me, ship));
    let clients = w.clients_of(target);
    w.notify(&clients, "OnShipJumpCloneInstallationOffered", offer_args(me, target, ship));
    info!("{me} offered {target} a clone in ship {ship}");
    Ok(Value::None)
}

/// AcceptShipCloneInstallation(): the pilot asked takes the clone, and pays.
async fn accept(ctx: &Ctx<'_>, me: i64) -> Refused<Value> {
    let Some((offerer, ship)) = ctx.world.lock().clone_offers.remove(&me) else { return refuse("Nobody is offering you a clone.") };
    let (have, may) = (count(ctx.db, me).await.map_err(db_err)?, allowed(ctx.db, me).await.map_err(db_err)?);
    let done = async {
        if have >= may {
            return refuse(&format!("You have {have} jump clone(s), as many as your Infomorph Psychology allows."));
        }
        let already = ctx
            .db
            .query_opt(r#"SELECT 1 FROM "chrJumpClones" WHERE "ownerID" = $1 AND "locationID" = $2"#, &[&(me as i32), &(ship as i32)])
            .await
            .map_err(db_err)?;
        if already.is_some() {
            return refuse("You already have a clone in that ship.");
        }
        pay(ctx, me, PRICE).await?;
        ctx.db
            .execute(r#"INSERT INTO "chrJumpClones" ("ownerID","locationID","createdAt") VALUES ($1, $2, $3)"#, &[&(me as i32), &(ship as i32), &crate::bulkdata::filetime_now()])
            .await
            .map_err(db_err)?;
        Ok(())
    }
    .await;
    let w = ctx.world.lock();
    let both: Vec<i64> = w.clients_of(me).into_iter().chain(w.clients_of(offerer)).collect();
    match done {
        Ok(()) => {
            w.notify(&both, "OnShipJumpCloneInstallationDone", offer_args(offerer, me, ship));
            w.notify(&w.clients_of(offerer), "OnShipJumpCloneCacheInvalidated", Value::Tuple(vec![Value::Int(ship), Value::Int(me)]));
            w.notify(&w.clients_of(me), "OnJumpCloneCacheInvalidated", Value::Tuple(Vec::new()));
            info!("{me} has a jump clone in {offerer}'s ship {ship}");
            Ok(Value::None)
        }
        Err(why) => {
            w.notify(&w.clients_of(offerer), "OnShipJumpCloneInstallationCanceled", offer_args(offerer, me, ship));
            Err(why)
        }
    }
}

/// CancelShipCloneInstallation(): whichever side calls it, the offer is off.
fn cancel(ctx: &Ctx<'_>, me: i64) {
    let mut w = ctx.world.lock();
    let mine: Vec<(i64, i64, i64)> = w.clone_offers.iter().filter(|(t, (o, _))| **t == me || *o == me).map(|(t, (o, s))| (*o, *t, *s)).collect();
    for (offerer, target, ship) in mine {
        w.clone_offers.remove(&target);
        let other = if offerer == me { target } else { offerer };
        let clients = w.clients_of(other);
        w.notify(&clients, "OnShipJumpCloneInstallationCanceled", offer_args(offerer, target, ship));
    }
}
