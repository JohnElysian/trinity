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
use crate::svc::rows::{float, int, key_val, rowset};
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
    let Some(me) = ctx.session.char_id else {
        return Some(Outcome::custom("Select a character first."));
    };
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

/// clonejumpsvc.GetCloneState reads named members, including its cooldown.
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
    let last = ctx
        .db
        .query_opt(
            r#"SELECT "lastCloneJump" FROM "character_" WHERE "characterID"=$1"#,
            &[&(me as i32)],
        )
        .await
        .map_err(db_err)?
        .map_or(0, |r| r.get::<_, i64>(0));
    Ok(key_val(vec![
        ("clones", clones),
        ("implants", implants),
        ("timeLastJump", Value::Long(last)),
    ]))
}

/// Everyone's clones at a station or in a ship.
async fn clones_at(ctx: &Ctx<'_>, location: Option<i64>) -> Value {
    let Some(location) = location else {
        return rowset(SHIP_CLONE_HEADER, Vec::new());
    };
    ctx.rowset(
        SHIP_CLONE_HEADER,
        r#"SELECT "jumpCloneID", "ownerID", "locationID" FROM "chrJumpClones" WHERE "locationID" = $1 ORDER BY 1"#,
        &[&(location as i32)],
    )
    .await
}

/// How many clones a pilot may keep: a level of Infomorph Psychology each.
async fn allowed(db: &impl GenericClient, me: i64) -> Result<i64, tokio_postgres::Error> {
    Ok(
        crate::svc::skills::levels(db, me, &[SKILL_INFOMORPH_PSYCHOLOGY])
            .await?
            .get(&SKILL_INFOMORPH_PSYCHOLOGY)
            .copied()
            .unwrap_or(0),
    )
}

async fn count(db: &impl GenericClient, me: i64) -> Result<i64, tokio_postgres::Error> {
    Ok(db
        .query_one(
            r#"SELECT count(*) FROM "chrJumpClones" WHERE "ownerID" = $1"#,
            &[&(me as i32)],
        )
        .await?
        .get(0))
}

async fn lock_pilot(
    tx: &tokio_postgres::Transaction<'_>,
    ctx: &Ctx<'_>,
    me: i64,
) -> Refused<tokio_postgres::Row> {
    if !super::login::lock_character_lifecycle(tx, me as i32)
        .await
        .map_err(db_err)?
    {
        return refuse("That character no longer exists.");
    }
    let pilot=tx.query_opt(r#"SELECT "stationID","solarSystemID","shipID","lastCloneJump" FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#,&[&(me as i32)]).await.map_err(db_err)?.ok_or_else(||Outcome::custom("That character no longer exists."))?;
    if int(&pilot, 0).unwrap_or(0) != ctx.session.station_id.unwrap_or(0)
        || int(&pilot, 2).filter(|id| *id > 0) != ctx.session.ship_id
    {
        return refuse("Your location or active ship has changed. Refresh the clone bay.");
    }
    Ok(pilot)
}

fn invalidated(ctx: &Ctx<'_>, owner: i64, location: i64, station: bool) {
    let w = ctx.world.lock();
    w.notify(
        &w.clients_of(owner),
        "OnJumpCloneCacheInvalidated",
        Value::Tuple(Vec::new()),
    );
    let event = if station {
        "OnStationJumpCloneCacheInvalidated"
    } else {
        "OnShipJumpCloneCacheInvalidated"
    };
    let here: Vec<i64> = w
        .players()
        .iter()
        .filter(|p| p.station_id == Some(location) || p.solar_system_id == location)
        .map(|p| p.char_id)
        .flat_map(|c| w.clients_of(c))
        .collect();
    w.notify(
        &here,
        event,
        Value::Tuple(vec![Value::Int(location), Value::Int(owner)]),
    );
}

async fn install(ctx: &Ctx<'_>, me: i64) -> Refused<Value> {
    let Some(station) = ctx.session.station_id else {
        return refuse("You must be docked to install a clone.");
    };
    let mut client = crate::db::connect(ctx.db_url).await.map_err(db_err)?;
    let tx = client.transaction().await.map_err(db_err)?;
    lock_pilot(&tx, ctx, me).await?;
    let row = tx
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
    let Some(row) = row else {
        return refuse("This station has no clone bay.");
    };
    let (owner, services, standing, owner_alliance) = (
        int(&row, 0).unwrap_or(0),
        int(&row, 1).unwrap_or(0),
        float(&row, 2).unwrap_or(0.0),
        int(&row, 3).unwrap_or(0),
    );
    let ours = Some(owner) == ctx.session.corp_id
        || (owner_alliance != 0 && Some(owner_alliance) == ctx.session.alliance_id);
    if services & (SERVICE_CLONING | SERVICE_JUMP_CLONE) as i64 == 0 {
        return refuse("This station has no clone bay.");
    }
    let need = crate::config::jump_clone_min_standing();
    if !ours && standing < need {
        return refuse(&format!("The station's owners install clones only for pilots they stand at {need:.1} or better towards; they stand at {standing:.1} towards you."));
    }
    let here = tx
        .query_opt(
            r#"SELECT 1 FROM "chrJumpClones" WHERE "ownerID" = $1 AND "locationID" = $2"#,
            &[&(me as i32), &(station as i32)],
        )
        .await
        .map_err(db_err)?;
    if here.is_some() {
        return refuse("You already have a clone at this station.");
    }
    let (have, may) = (
        count(&tx, me).await.map_err(db_err)?,
        allowed(&tx, me).await.map_err(db_err)?,
    );
    if have >= may {
        return refuse(&format!(
            "You have {have} jump clone(s), as many as your Infomorph Psychology allows."
        ));
    }
    pay(&tx, me, PRICE).await?;
    tx.execute(
        r#"INSERT INTO "chrJumpClones" ("ownerID","locationID","createdAt") VALUES ($1, $2, $3)"#,
        &[
            &(me as i32),
            &(station as i32),
            &crate::bulkdata::filetime_now(),
        ],
    )
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    super::wallet::notify_balance(ctx, me).await;
    info!("{me} installed a jump clone at station {station}");
    invalidated(ctx, me, station, true);
    Ok(Value::None)
}

async fn pay(tx: &tokio_postgres::Transaction<'_>, me: i64, amount: f64) -> Refused<()> {
    use crate::svc::wallet::{post, Entry, Transfer};
    let entry = Entry {
        payer: me,
        payee: 0,
        ref_type: REF_CLONE,
        arg_id1: "",
        reason: "Jump clone installation",
    };
    match post(tx, me, -amount, &entry).await {
        Ok(_) => Ok(()),
        Err(Transfer::NotEnough { need, have }) => Err(Outcome::not_enough_money(need, have)),
        Err(Transfer::NoSuchOwner) => refuse("No wallet."),
        Err(Transfer::Db(e)) => Err(db_err(e)),
    }
}

/// A clone and its implants, gone.
async fn remove(db: &impl GenericClient, clone: i64) -> Result<(), tokio_postgres::Error> {
    db.execute(r#"WITH removed AS (DELETE FROM entity WHERE "locationID"=$1 RETURNING "itemID") DELETE FROM entity_attributes WHERE "itemID" IN (SELECT "itemID" FROM removed)"#, &[&(clone as i32)]).await?;
    db.execute(
        r#"DELETE FROM "chrJumpClones" WHERE "jumpCloneID" = $1"#,
        &[&(clone as i32)],
    )
    .await?;
    Ok(())
}

/// Jump clones carried in a ship, gone with it.
pub async fn ship_destroyed(
    db: &impl GenericClient,
    ship: i64,
) -> Result<u64, tokio_postgres::Error> {
    db.execute(r#"WITH removed AS (DELETE FROM entity WHERE "locationID" IN (SELECT "jumpCloneID" FROM "chrJumpClones" WHERE "locationID" = $1) RETURNING "itemID") DELETE FROM entity_attributes WHERE "itemID" IN (SELECT "itemID" FROM removed)"#, &[&(ship as i32)]).await?;
    db.execute(
        r#"DELETE FROM "chrJumpClones" WHERE "locationID" = $1"#,
        &[&(ship as i32)],
    )
    .await
}

/// DestroyInstalledClone(cloneID): one's own clone where one is, or anyone's
/// in the ship one flies.
async fn destroy(ctx: &Ctx<'_>, me: i64, clone: i64) -> Refused<Value> {
    let row = ctx
        .db
        .query_opt(
            r#"SELECT "ownerID", "locationID" FROM "chrJumpClones" WHERE "jumpCloneID" = $1"#,
            &[&(clone as i32)],
        )
        .await
        .map_err(db_err)?;
    let Some(row) = row else {
        return refuse("That clone no longer exists.");
    };
    let (owner, location) = (int(&row, 0).unwrap_or(0), int(&row, 1).unwrap_or(0));
    let mut client = crate::db::connect(ctx.db_url).await.map_err(db_err)?;
    let tx = client.transaction().await.map_err(db_err)?;
    super::login::lock_character_lifecycle(&tx, owner as i32)
        .await
        .map_err(db_err)?;
    let in_my_ship = if ctx.session.ship_id == Some(location) {
        tx.query_opt(r#"SELECT 1 FROM entity e JOIN "character_" c ON c."shipID"=e."itemID" WHERE e."itemID"=$1 AND e."ownerID"=$2 AND c."characterID"=$2 FOR UPDATE OF e"#,&[&(location as i32),&(me as i32)]).await.map_err(db_err)?.is_some()
    } else {
        false
    };
    if owner != me && !in_my_ship {
        return refuse("You can only destroy your own clone, or clones in your current ship.");
    }
    if tx.query_opt(r#"SELECT 1 FROM "chrJumpClones" WHERE "jumpCloneID"=$1 AND "ownerID"=$2 AND "locationID"=$3 FOR UPDATE"#,&[&(clone as i32),&(owner as i32),&(location as i32)]).await.map_err(db_err)?.is_none() {return refuse("That clone no longer exists.");}
    remove(&tx, clone).await.map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    info!("{me} destroyed jump clone {clone} of {owner} at {location}");
    invalidated(ctx, owner, location, !in_my_ship);
    Ok(Value::None)
}

/// CloneJump(destLocationID, force).
async fn jump(ctx: &Ctx<'_>, me: i64, dest: i64, force: bool) -> Refused<Value> {
    let Some(here) = ctx.session.station_id else {
        return refuse("You must be docked to clone jump.");
    };
    if dest == here {
        return refuse("You are already at that clone.");
    }
    let mut client = crate::db::connect(ctx.db_url).await.map_err(db_err)?;
    let tx = client.transaction().await.map_err(db_err)?;
    let pilot = lock_pilot(&tx, ctx, me).await?;
    let now = crate::bulkdata::filetime_now();
    let next = pilot.get::<_, i64>(3).saturating_add(COOLDOWN_FT);
    if next > now {
        return refuse(&format!("Your body has not recovered from your last clone jump: you can jump again in {:.1} hours.",(next-now) as f64/36_000_000_000.0));
    }
    let target = tx
        .query_opt(
            r#"SELECT "jumpCloneID" FROM "chrJumpClones" WHERE "ownerID"=$1 AND "locationID"=$2"#,
            &[&(me as i32), &(dest as i32)],
        )
        .await
        .map_err(db_err)?
        .ok_or_else(|| Outcome::custom("You have no clone there."))?
        .get::<_, i32>(0);
    // Carrier loss locks the carrier before its stored clones. Use the same
    // order here so its deletion cannot leave an arriving capsule orphaned.
    let station_row=tx.query_opt(r#"SELECT "stationID","solarSystemID","constellationID","regionID" FROM "staStations" WHERE "stationID"=$1"#,&[&(dest as i32)]).await.map_err(db_err)?;
    let (station, system, constellation, region, at) = if let Some(place) = station_row {
        (
            Some(place.get::<_, i32>(0) as i64),
            place.get::<_, i32>(1) as i64,
            place.get::<_, i32>(2) as i64,
            place.get::<_, i32>(3) as i64,
            crate::ballpark::Vec3::ZERO,
        )
    } else {
        let place=tx.query_opt(r#"SELECT st."stationID",m."solarSystemID",m."constellationID",m."regionID",e.x,e.y,e.z
            FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID" JOIN "invGroups" g ON g."groupID"=t."groupID"
            LEFT JOIN "staStations" st ON st."stationID"=e."locationID"
            JOIN "mapSolarSystems" m ON m."solarSystemID"=COALESCE(st."solarSystemID",e."locationID")
            WHERE e."itemID"=$1 AND g."categoryID"=6 FOR UPDATE OF e"#,&[&(dest as i32)]).await.map_err(db_err)?.ok_or_else(||Outcome::custom("The ship carrying your clone is no longer available."))?;
        if !has_module(&tx, dest, &CLONE_VAT_BAYS)
            .await
            .map_err(db_err)?
        {
            return refuse("The ship carrying your clone has no clone vat bay.");
        }
        if !force {
            return Err(Outcome::UserError {
                msg: "JumpCheckIntoShip".into(),
                args: vec![],
            });
        }
        let station = int(&place, 0).filter(|id| *id > 0);
        let system = int(&place, 1).unwrap_or(0);
        let at = if station.is_none() {
            let mut world = ctx.world.lock();
            let Some(park) = world.parks.get_mut(&system) else {
                return refuse("The ship carrying your clone is not available in space.");
            };
            park.advance(crate::destiny::tick_now());
            let Some(carrier) = park
                .ships
                .get(&dest)
                .filter(|ship| ship.is_pilot() && !ship.warping() && !ship.cloaked)
            else {
                return refuse("The ship carrying your clone cannot receive a clone jump now.");
            };
            carrier.pos + crate::ballpark::Vec3::new(carrier.stats.radius + 2_500.0, 0.0, 0.0)
        } else {
            crate::ballpark::Vec3::ZERO
        };
        (
            station,
            system,
            int(&place, 2).unwrap_or(0),
            int(&place, 3).unwrap_or(0),
            at,
        )
    };
    if tx.query_opt(r#"SELECT 1 FROM "chrJumpClones" WHERE "jumpCloneID"=$1 AND "ownerID"=$2 AND "locationID"=$3 FOR UPDATE"#,&[&target,&(me as i32),&(dest as i32)]).await.map_err(db_err)?.is_none() {
        return refuse("That clone is no longer available.");
    }
    let existing=tx.query_opt(r#"SELECT j."jumpCloneID",(SELECT count(*) FROM entity e WHERE e."locationID"=j."jumpCloneID") FROM "chrJumpClones" j WHERE j."ownerID"=$1 AND j."locationID"=$2 FOR UPDATE OF j"#,&[&(me as i32),&(here as i32)]).await.map_err(db_err)?;
    if let Some(old) = &existing {
        if !force {
            let msg = if old.get::<_, i64>(1) > 0 {
                "JumpCheckWillLoseExistingCloneAndImplants"
            } else {
                "JumpCheckWillLoseExistingClone"
            };
            return Err(Outcome::UserError {
                msg: msg.into(),
                args: vec![],
            });
        }
        remove(&tx, old.get::<_, i32>(0) as i64)
            .await
            .map_err(db_err)?;
    }
    let left=tx.query_one(r#"INSERT INTO "chrJumpClones" ("ownerID","locationID","createdAt") VALUES ($1,$2,$3) RETURNING "jumpCloneID""#,&[&(me as i32),&(here as i32),&now]).await.map_err(db_err)?.get::<_,i32>(0);
    tx.execute(
        r#"UPDATE entity SET "locationID"=$2 WHERE "locationID"=$1 AND "ownerID"=$1 AND flag=$3"#,
        &[&(me as i32), &left, &FLAG_IMPLANT],
    )
    .await
    .map_err(db_err)?;
    tx.execute(
        r#"UPDATE entity SET "locationID"=$1 WHERE "locationID"=$2 AND "ownerID"=$1 AND flag=$3"#,
        &[&(me as i32), &target, &FLAG_IMPLANT],
    )
    .await
    .map_err(db_err)?;
    tx.execute(
        r#"DELETE FROM "chrJumpClones" WHERE "jumpCloneID"=$1"#,
        &[&target],
    )
    .await
    .map_err(db_err)?;
    let location = station.unwrap_or(system);
    let flag = if station.is_some() { FLAG_HANGAR } else { 0 };
    let capsule=tx.query_one(r#"INSERT INTO entity ("itemID","itemName","typeID","ownerID","locationID",flag,contraband,singleton,quantity,x,y,z)
        VALUES (nextval('dyn_id_seq'),'',$1,$2,$3,$4,0,1,1,$5,$6,$7) RETURNING "itemID""#,&[&TYPE_CAPSULE,&(me as i32),&(location as i32),&flag,&at.x,&at.y,&at.z]).await.map_err(db_err)?.get::<_,i32>(0);
    tx.execute(r#"UPDATE "character_" SET "stationID"=$2,"solarSystemID"=$3,"constellationID"=$4,"regionID"=$5,"shipID"=$6,"lastCloneJump"=$7 WHERE "characterID"=$1"#,&[&(me as i32),&(station.unwrap_or(0) as i32),&(system as i32),&(constellation as i32),&(region as i32),&capsule,&now]).await.map_err(db_err)?;
    tx.execute(
        r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,
        &[&(me as i32), &(location as i32)],
    )
    .await
    .map_err(db_err)?;
    let live = if station.is_none() {
        crate::svc::space::ensure_park(ctx, system).await;
        let profile = crate::svc::inflight::profile(
            &tx,
            ctx.dogma,
            me,
            capsule as i64,
            &Default::default(),
            crate::combat::now_ms(),
        )
        .await
        .map_err(db_err)?
        .ok_or_else(|| Outcome::custom("The destination capsule could not be prepared."))?;
        let mut ship = crate::ballpark::Ship::new(
            capsule as i64,
            ctx.client_id,
            profile.stats,
            profile.pilot,
            at,
            crate::ballpark::Vec3::ZERO,
        );
        ship.fight = Some(profile.fighter);
        Some(ship)
    } else {
        None
    };
    tx.commit().await.map_err(db_err)?;
    {
        let mut world = ctx.world.lock();
        if let Some(ship) = live {
            crate::svc::space::add_ship(&mut world, system, ship);
        }
        world.push_move(
            ctx.client_id,
            Move::Relocate {
                station_id: station,
                solar_system_id: system,
                constellation_id: constellation,
                region_id: region,
                ship_id: capsule as i64,
            },
        );
    }
    info!("{me} clone jumped from station {here} to {dest}, in capsule {capsule}");
    invalidated(ctx, me, here, true);
    Ok(Value::None)
}

/// Whether a ship has one of `types` fitted.
pub async fn has_module(
    db: &impl GenericClient,
    ship: i64,
    types: &[i32],
) -> Result<bool, tokio_postgres::Error> {
    let row = db
        .query_opt(
            r#"SELECT 1 FROM entity WHERE "locationID" = $1 AND "typeID" = ANY($2) AND "flag" BETWEEN $3 AND $4 LIMIT 1"#,
            &[&(ship as i32), &types, &SLOT_FLAGS.0, &SLOT_FLAGS.1],
        )
        .await?;
    Ok(row.is_some())
}

fn offer_args(offerer: i64, target: i64, ship: i64) -> Value {
    Value::Tuple(vec![Value::Tuple(vec![
        Value::Int(offerer),
        Value::Int(target),
        Value::Int(ship),
        Value::None,
    ])])
}

/// OfferShipCloneInstallation(charID): a pilot whose ship has a clone vat
/// bay offers another pilot in the same system a clone in it.
async fn offer(ctx: &Ctx<'_>, me: i64, target: i64) -> Refused<Value> {
    let Some(ship) = ctx.session.ship_id else {
        return refuse("You are not in a ship.");
    };
    if !has_module(ctx.db, ship, &CLONE_VAT_BAYS)
        .await
        .map_err(db_err)?
    {
        return refuse("Your ship has no clone vat bay.");
    }
    let mut w = ctx.world.lock();
    let Some(there) = w.presence_of(target) else {
        return refuse("That pilot is not online.");
    };
    if Some(there.solar_system_id) != ctx.session.solar_system_id2.or(ctx.session.solar_system_id) {
        return refuse("That pilot is not in this system.");
    }
    w.clone_offers.insert(target, (me, ship));
    let clients = w.clients_of(target);
    w.notify(
        &clients,
        "OnShipJumpCloneInstallationOffered",
        offer_args(me, target, ship),
    );
    info!("{me} offered {target} a clone in ship {ship}");
    Ok(Value::None)
}

/// AcceptShipCloneInstallation(): the pilot asked takes the clone, and pays.
async fn accept(ctx: &Ctx<'_>, me: i64) -> Refused<Value> {
    let Some((offerer, ship)) = ctx.world.lock().clone_offers.remove(&me) else {
        return refuse("Nobody is offering you a clone.");
    };
    let done = async {
        let mut client=crate::db::connect(ctx.db_url).await.map_err(db_err)?;
        let tx=client.transaction().await.map_err(db_err)?;
        lock_pilot(&tx,ctx,me).await?;
        let carrier=tx.query_opt(r#"SELECT e."locationID",COALESCE(st."solarSystemID",e."locationID") FROM entity e LEFT JOIN "staStations" st ON st."stationID"=e."locationID" JOIN "character_" c ON c."shipID"=e."itemID" AND c."characterID"=e."ownerID" WHERE e."itemID"=$1 AND e."ownerID"=$2 FOR UPDATE OF e"#,&[&(ship as i32),&(offerer as i32)]).await.map_err(db_err)?;
        let Some(carrier)=carrier else {return refuse("The offering pilot no longer controls that ship.");};
        if Some(carrier.get::<_,i32>(1) as i64)!=ctx.session.solar_system_id2.or(ctx.session.solar_system_id) || !has_module(&tx,ship,&CLONE_VAT_BAYS).await.map_err(db_err)? {
            return refuse("The offered clone bay is no longer available in this system.");
        }
        let online=ctx.world.lock().presence_of(offerer).is_some_and(|p|Some(p.solar_system_id)==ctx.session.solar_system_id2.or(ctx.session.solar_system_id));
        if !online {return refuse("The offering pilot is no longer available.");}
        let (have, may) = (count(&tx, me).await.map_err(db_err)?, allowed(&tx, me).await.map_err(db_err)?);
        if have >= may {
            return refuse(&format!("You have {have} jump clone(s), as many as your Infomorph Psychology allows."));
        }
        let already = tx
            .query_opt(r#"SELECT 1 FROM "chrJumpClones" WHERE "ownerID" = $1 AND "locationID" = $2"#, &[&(me as i32), &(ship as i32)])
            .await
            .map_err(db_err)?;
        if already.is_some() {
            return refuse("You already have a clone in that ship.");
        }
        pay(&tx, me, PRICE).await?;
        tx
            .execute(r#"INSERT INTO "chrJumpClones" ("ownerID","locationID","createdAt") VALUES ($1, $2, $3)"#, &[&(me as i32), &(ship as i32), &crate::bulkdata::filetime_now()])
            .await
            .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        super::wallet::notify_balance(ctx,me).await;
        Ok(())
    }
    .await;
    let w = ctx.world.lock();
    let both: Vec<i64> = w
        .clients_of(me)
        .into_iter()
        .chain(w.clients_of(offerer))
        .collect();
    match done {
        Ok(()) => {
            w.notify(
                &both,
                "OnShipJumpCloneInstallationDone",
                offer_args(offerer, me, ship),
            );
            w.notify(
                &w.clients_of(offerer),
                "OnShipJumpCloneCacheInvalidated",
                Value::Tuple(vec![Value::Int(ship), Value::Int(me)]),
            );
            w.notify(
                &w.clients_of(me),
                "OnJumpCloneCacheInvalidated",
                Value::Tuple(Vec::new()),
            );
            info!("{me} has a jump clone in {offerer}'s ship {ship}");
            Ok(Value::None)
        }
        Err(why) => {
            w.notify(
                &w.clients_of(offerer),
                "OnShipJumpCloneInstallationCanceled",
                offer_args(offerer, me, ship),
            );
            Err(why)
        }
    }
}

/// CancelShipCloneInstallation(): whichever side calls it, the offer is off.
fn cancel(ctx: &Ctx<'_>, me: i64) {
    let mut w = ctx.world.lock();
    let mine: Vec<(i64, i64, i64)> = w
        .clone_offers
        .iter()
        .filter(|(t, (o, _))| **t == me || *o == me)
        .map(|(t, (o, s))| (*o, *t, *s))
        .collect();
    for (offerer, target, ship) in mine {
        w.clone_offers.remove(&target);
        let other = if offerer == me { target } else { offerer };
        let clients = w.clients_of(other);
        w.notify(
            &clients,
            "OnShipJumpCloneInstallationCanceled",
            offer_args(offerer, target, ship),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{session::Session, test_support::Database};

    async fn stations(db: &Database) -> Vec<(i32, i32, i32)> {
        db.db.query(r#"SELECT s."stationID",s."solarSystemID",s."corporationID" FROM "staStations" s WHERE EXISTS (SELECT 1 FROM "staOperationServices" os WHERE os."operationID"=s."operationID" AND os."serviceID" IN (512,8388608)) ORDER BY s."stationID" LIMIT 2"#,&[]).await.unwrap().iter().map(|r|(r.get(0),r.get(1),r.get(2))).collect()
    }
    async fn pilot(db: &Database, place: (i32, i32, i32)) -> Session {
        let me = db.id().await;
        let ship = db.id().await;
        db.db.execute(r#"INSERT INTO "character_" ("characterID","accountID","characterName",balance,"stationID","solarSystemID","corporationID","shipID") VALUES ($1,$1,$2,10000000,$3,$4,$5,$6)"#,&[&me,&format!("Clone fixture {me}"),&place.0,&place.1,&place.2,&ship]).await.unwrap();
        db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES ($1,587,$2,$3,4,1,1),($2,1377,$2,$3,0,1,1)"#,&[&ship,&me,&place.0]).await.unwrap();
        let skill=db.db.query_one(r#"INSERT INTO entity ("typeID","ownerID","locationID",flag,quantity,singleton) VALUES (24242,$1,$1,7,1,1) RETURNING "itemID""#,&[&me]).await.unwrap().get::<_,i32>(0);
        db.db.execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt") VALUES ($1,280,5)"#,&[&skill]).await.unwrap();
        Session {
            char_id: Some(me as i64),
            ship_id: Some(ship as i64),
            user_id: Some(me as i64),
            station_id: Some(place.0 as i64),
            solar_system_id2: Some(place.1 as i64),
            corp_id: Some(place.2 as i64),
            ..Default::default()
        }
    }
    async fn clone_at(db: &Database, me: i64, location: i64) -> i32 {
        db.db.query_one(r#"INSERT INTO "chrJumpClones" ("ownerID","locationID") VALUES ($1,$2) RETURNING "jumpCloneID""#,&[&(me as i32),&(location as i32)]).await.unwrap().get(0)
    }
    async fn implant(db: &Database, me: i64, location: i64) -> i32 {
        db.db.query_one(r#"INSERT INTO entity ("typeID","ownerID","locationID",flag,quantity,singleton) VALUES (9941,$1,$2,89,1,1) RETURNING "itemID""#,&[&(me as i32),&(location as i32)]).await.unwrap().get(0)
    }
    async fn balance(db: &Database, me: i64) -> f64 {
        db.db
            .query_one(
                r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,
                &[&(me as i32)],
            )
            .await
            .unwrap()
            .get(0)
    }
    fn field<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
        match value {
            Value::Instance { state, .. } => field(state, key),
            Value::Dict(items) => items
                .iter()
                .find(|(k, _)| k.as_bytes() == Some(key.as_bytes()))
                .map(|(_, v)| v),
            _ => None,
        }
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn clone_installation_and_remote_destruction_are_atomic() {
        let db = Database::load().await;
        let places = stations(&db).await;
        let mut session = pilot(&db, places[0]).await;
        let me = session.char_id.unwrap();
        let ctx = db.ctx(&session);
        let (one, two) = tokio::join!(install(&ctx, me), install(&ctx, me));
        assert_eq!([one, two].iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(balance(&db, me).await, 10_000_000.0 - PRICE);
        let state = clone_state(&ctx, me).await.ok().unwrap();
        assert!(field(&state, "clones").is_some());
        assert!(field(&state, "implants").is_some());
        assert_eq!(
            field(&state, "timeLastJump").and_then(Value::as_i64),
            Some(0)
        );
        let old = db
            .db
            .query_one(
                r#"SELECT "jumpCloneID" FROM "chrJumpClones" WHERE "ownerID"=$1"#,
                &[&(me as i32)],
            )
            .await
            .unwrap()
            .get::<_, i32>(0);
        let old_implant = implant(&db, me, old as i64).await;
        db.db.execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt") VALUES ($1,280,1)"#,&[&old_implant]).await.unwrap();
        session.station_id = Some(places[1].0 as i64);
        session.solar_system_id2 = Some(places[1].1 as i64);
        session.corp_id = Some(places[1].2 as i64);
        db.db.execute(r#"UPDATE "character_" SET "stationID"=$2,"solarSystemID"=$3,"corporationID"=$4 WHERE "characterID"=$1"#,&[&(me as i32),&places[1].0,&places[1].1,&places[1].2]).await.unwrap();
        db.db
            .execute(
                r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,
                &[&(session.ship_id.unwrap() as i32), &places[1].0],
            )
            .await
            .unwrap();
        let constraint = format!("clone_failure_fixture_{me}");
        db.db.batch_execute(&format!(r#"ALTER TABLE "chrJumpClones" ADD CONSTRAINT {constraint} CHECK (NOT ("ownerID"={me} AND "locationID"={}))"#,places[1].0)).await.unwrap();
        let failed = install(&db.ctx(&session), me).await;
        db.db
            .batch_execute(&format!(
                r#"ALTER TABLE "chrJumpClones" DROP CONSTRAINT {constraint}"#
            ))
            .await
            .unwrap();
        assert!(failed.is_err());
        assert_eq!(balance(&db, me).await, 10_000_000.0 - PRICE);
        assert!(
            destroy(&db.ctx(&session), me, old as i64).await.is_ok(),
            "the client permits destroying your own remote clone"
        );
        assert!(db
            .db
            .query_opt(
                r#"SELECT 1 FROM entity_attributes WHERE "itemID"=$1"#,
                &[&old_implant]
            )
            .await
            .unwrap()
            .is_none());
        assert!(db
            .db
            .query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#, &[&old_implant])
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn clone_jump_swaps_implants_once_and_rolls_back_failed_capsules() {
        let db = Database::load().await;
        let places = stations(&db).await;
        let session = pilot(&db, places[0]).await;
        let me = session.char_id.unwrap();
        let ctx = db.ctx(&session);
        let existing = clone_at(&db, me, places[0].0 as i64).await;
        let target = clone_at(&db, me, places[1].0 as i64).await;
        let active = implant(&db, me, me).await;
        let arriving = implant(&db, me, target as i64).await;
        let replaced = implant(&db, me, existing as i64).await;
        assert!(
            matches!(jump(&ctx,me,places[1].0 as i64,false).await,Err(Outcome::UserError {msg,..}) if msg=="JumpCheckWillLoseExistingCloneAndImplants")
        );
        let constraint = format!("capsule_failure_fixture_{me}");
        db.db.batch_execute(&format!(r#"ALTER TABLE entity ADD CONSTRAINT {constraint} CHECK (NOT ("ownerID"={me} AND "typeID"=670))"#)).await.unwrap();
        let failed = jump(&ctx, me, places[1].0 as i64, true).await;
        db.db
            .batch_execute(&format!("ALTER TABLE entity DROP CONSTRAINT {constraint}"))
            .await
            .unwrap();
        assert!(failed.is_err());
        let untouched: i32 = db
            .db
            .query_one(
                r#"SELECT "locationID" FROM entity WHERE "itemID"=$1"#,
                &[&active],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(untouched, me as i32);
        assert!(db
            .db
            .query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#, &[&replaced])
            .await
            .unwrap()
            .is_some());
        assert!(db
            .db
            .query_opt(
                r#"SELECT 1 FROM "chrJumpClones" WHERE "jumpCloneID"=$1"#,
                &[&target]
            )
            .await
            .unwrap()
            .is_some());
        let (one, two) = tokio::join!(
            jump(&ctx, me, places[1].0 as i64, true),
            jump(&ctx, me, places[1].0 as i64, true)
        );
        assert_eq!([one, two].iter().filter(|r| r.is_ok()).count(), 1);
        let saved=db.db.query_one(r#"SELECT "stationID","shipID","lastCloneJump" FROM "character_" WHERE "characterID"=$1"#,&[&(me as i32)]).await.unwrap();
        assert_eq!(saved.get::<_, i32>(0), places[1].0);
        assert!(saved.get::<_, i64>(2) > 0);
        let left=db.db.query_one(r#"SELECT "jumpCloneID" FROM "chrJumpClones" WHERE "ownerID"=$1 AND "locationID"=$2"#,&[&(me as i32),&places[0].0]).await.unwrap().get::<_,i32>(0);
        let locations=db.db.query(r#"SELECT "itemID","locationID" FROM entity WHERE "itemID"=ANY($1) ORDER BY "itemID""#,&[&vec![active,arriving,replaced]]).await.unwrap();
        assert_eq!(locations.len(), 2);
        for row in locations {
            assert_eq!(
                row.get::<_, i32>(1),
                if row.get::<_, i32>(0) == active {
                    left
                } else {
                    me as i32
                }
            );
        }
        let capsules: i64 = db
            .db
            .query_one(
                r#"SELECT count(*) FROM entity WHERE "ownerID"=$1 AND "typeID"=670"#,
                &[&(me as i32)],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(capsules, 1);
        let mut moved = session.clone();
        moved.station_id = Some(places[1].0 as i64);
        moved.solar_system_id2 = Some(places[1].1 as i64);
        moved.ship_id = Some(saved.get::<_, i32>(1) as i64);
        assert!(
            jump(&db.ctx(&moved), me, places[0].0 as i64, true)
                .await
                .is_err(),
            "cooldown survives a new session"
        );
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn ship_clone_offer_is_revalidated_and_arrival_matches_live_carrier_position() {
        let db = Database::load().await;
        let place = stations(&db).await[0];
        let borrower = pilot(&db, place).await;
        let mut host = pilot(&db, place).await;
        let me = borrower.char_id.unwrap();
        let captain = host.char_id.unwrap();
        let carrier = host.ship_id.unwrap();
        host.station_id = None;
        host.solar_system_id = Some(place.1 as i64);
        db.db
            .execute(
                r#"UPDATE "character_" SET "stationID"=0 WHERE "characterID"=$1"#,
                &[&(captain as i32)],
            )
            .await
            .unwrap();
        db.db
            .execute(
                r#"UPDATE entity SET "typeID"=11567,"locationID"=$2,flag=0 WHERE "itemID"=$1"#,
                &[&(carrier as i32), &place.1],
            )
            .await
            .unwrap();
        db.db.execute(r#"INSERT INTO entity ("typeID","ownerID","locationID",flag,quantity,singleton) VALUES (23735,$1,$2,27,1,1)"#,&[&(captain as i32),&(carrier as i32)]).await.unwrap();
        let at = crate::ballpark::Vec3::new(1_000_000.0, 2_000_000.0, 3_000_000.0);
        let mut carrier_ship =
            crate::svc::inflight::ship_for_space(&db.ctx(&host), captain, carrier)
                .await
                .unwrap();
        carrier_ship.client_id = 2;
        carrier_ship.arrive(at);
        let arrival = at + crate::ballpark::Vec3::new(carrier_ship.stats.radius + 2500.0, 0.0, 0.0);
        let (_a, _b) = {
            let mut world = db.world.lock();
            let a = world.connect(1);
            let b = world.connect(2);
            for (client, pilot, docked) in [(1, me, true), (2, captain, false)] {
                world.set_presence(
                    client,
                    Some(crate::world::Presence {
                        char_id: pilot,
                        corp_id: place.2 as i64,
                        solar_system_id: place.1 as i64,
                        docked,
                        station_id: if docked { Some(place.0 as i64) } else { None },
                        role: 2,
                    }),
                );
            }
            let mut park =
                crate::ballpark::Park::new(place.1 as i64, vec![], crate::destiny::tick_now());
            park.ships.insert(carrier, carrier_ship);
            world.parks.insert(place.1 as i64, park);
            (a, b)
        };
        let mut host_ctx = db.ctx(&host);
        host_ctx.client_id = 2;
        assert!(offer(&host_ctx, captain, me).await.is_ok());
        db.db
            .execute(
                r#"UPDATE entity SET "ownerID"=0 WHERE "itemID"=$1"#,
                &[&(carrier as i32)],
            )
            .await
            .unwrap();
        assert!(accept(&db.ctx(&borrower), me).await.is_err());
        assert_eq!(balance(&db, me).await, 10_000_000.0);
        db.db
            .execute(
                r#"UPDATE entity SET "ownerID"=$2 WHERE "itemID"=$1"#,
                &[&(carrier as i32), &(captain as i32)],
            )
            .await
            .unwrap();
        assert!(offer(&host_ctx, captain, me).await.is_ok());
        assert!(accept(&db.ctx(&borrower), me).await.is_ok());
        assert_eq!(balance(&db, me).await, 10_000_000.0 - PRICE);
        assert!(
            matches!(jump(&db.ctx(&borrower),me,carrier,false).await,Err(Outcome::UserError {msg,..}) if msg=="JumpCheckIntoShip")
        );
        assert!(jump(&db.ctx(&borrower), me, carrier, true).await.is_ok());
        let saved=db.db.query_one(r#"SELECT c."stationID",c."shipID",e.x,e.y,e.z FROM "character_" c JOIN entity e ON e."itemID"=c."shipID" WHERE c."characterID"=$1"#,&[&(me as i32)]).await.unwrap();
        assert_eq!(saved.get::<_, i32>(0), 0);
        let capsule = saved.get::<_, i32>(1) as i64;
        assert_eq!(
            crate::ballpark::Vec3::new(saved.get(2), saved.get(3), saved.get(4)),
            arrival
        );
        assert_eq!(
            db.world.lock().parks[&(place.1 as i64)].ships[&capsule].pos,
            arrival
        );
        assert_ne!(capsule, borrower.ship_id.unwrap());
    }
}
