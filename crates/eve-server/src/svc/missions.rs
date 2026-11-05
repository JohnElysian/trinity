//! `missionMgr`: pilots' courier missions and escrows (ui/station/missions).
//!
//! A courier mission is made at a station (the bound creator,
//! moniker.GetCourierMissionCreator(stationID)): the items are wrapped in a
//! package (Plastic Wrap), held by EVE System at the pickup station, and the
//! reward is taken from the issuer. Whoever accepts pays the collateral and
//! gets the package in their hangar there; delivering it at the drop-off
//! hands its contents to the issuer and pays the courier the reward and the
//! collateral back. An escrow is a package that stays where it is, sold for
//! its price: the buyer pays the issuer and unwraps it in their own hangar.
//!
//! The issuer may take back what nobody has accepted. What lapses unaccepted
//! goes back to the issuer, reward and all; an accepted mission that lapses
//! undelivered pays the collateral to the issuer, and the courier keeps the
//! package. The client's own rules stand: one package holds at most
//! const.maxCourierMissionPackage m3.

use eve_marshal::Value;
use tracing::{error, info};

use crate::items::{invitem_cells, ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Row, StoreError};
use crate::svc::bound;
use crate::svc::inventory::INVITEM_HEADER;
use crate::svc::membership::{db_failed, Refused, Undone};
use crate::svc::rows::{key_val, row, rowset};
use crate::svc::wallet::{self, Entry, Transfer};

/// The courier package.
pub const TYPE_PLASTIC_WRAP: i64 = 3468;
const OWNER_SYSTEM: i64 = 1;
const FLAG_HANGAR: i64 = 4;
/// const.maxCourierMissionPackage, in m3.
const MAX_PACKAGE: f64 = 20_000.0;
/// refCourierMissionEscrow: the reward and collateral held, and paid out.
const REF_ESCROW: i32 = 23;

const KIND_COURIER: i16 = 1;
const KIND_ESCROW: i16 = 2;

const OUTSTANDING: i16 = 0;
const ACCEPTED: i16 = 1;
const DONE: i16 = 2;
const CANCELLED: i16 = 3;
const LAPSED: i16 = 4;

const COURIER_HEADER: &[&str] = &[
    "missionID", "kind", "state", "availabilityID", "inOrder", "description", "issuerID",
    "assigneeID", "acceptFee", "acceptorID", "dateIssued", "dateExpires", "dateAccepted",
    "dateCompleted", "totalReward", "tracking", "pickupStationID", "craterID", "dropStationID",
    "volume", "pickupSolarSystemID", "pickupRegionID", "dropSolarSystemID", "dropRegionID",
];

const ESCROW_HEADER: &[&str] = &[
    "escrowID", "price", "stationID", "solarsystemID", "regionID", "issuerID", "assigneeID",
    "acceptorID", "dateExpires", "description",
];

/// What GetManifest lists of a package (missionentry.py Inspect).
const MANIFEST_HEADER: &[&str] = &["itemID", "typeID", "quantity", "damage", "copy", "productivityLevel", "materialLevel", "bpRuns"];

pub async fn mission_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let station = bound::parse(&call.service).and_then(|b| b.params.first().copied());
    settle_lapsed(ctx).await;
    let result = match (call.method.as_str(), station) {
        ("GetCourierOptions", Some(station)) => Ok(courier_options(ctx, station)),
        ("CreateCourierMission", Some(station)) => create(ctx, call, station).await,
        ("GetAvailableCourierMissions", _) => Ok(available(ctx, call)),
        ("GetMyCourierMissions", _) => Ok(mine(ctx)),
        ("GetAvailableEscrows", _) => Ok(escrows(ctx, call)),
        ("GetManifest", _) => Ok(manifest(ctx, call.arg_id(0))),
        ("AcceptCourierMission", _) => accept(ctx, call.arg_id(0)).await,
        ("DeliverCourierPackage", _) => deliver(ctx, call.arg_id(0)).await,
        ("CancelMission", _) => cancel(ctx, call.arg_id(0)).await,
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

// ---- listings ----------------------------------------------------------------------------

fn int(v: i64) -> store::Value {
    store::Value::Int(v)
}

/// GetCourierOptions(): the caller's items in the station's hangar, as
/// itemList.header and itemList.items {itemID: line} (base_missions.py:369).
fn courier_options(ctx: &Ctx<'_>, station: i64) -> Value {
    let me = ctx.session.char_id.unwrap_or(0);
    let items = ctx.store.read(|db| {
        let mut found: Vec<Row> = db.table("entity").find(&["locationID", "flag", "ownerID"], &[int(station), int(FLAG_HANGAR), int(me)]).collect();
        found.sort_by_key(|r| r.int("itemID"));
        found.iter().filter_map(|r| Some((Value::Int(r.int("itemID").unwrap_or(0)), Value::List(invitem_cells(r)?)))).collect()
    });
    let header = Value::List(INVITEM_HEADER.iter().map(|h| Value::bytes(*h)).collect());
    key_val(vec![("itemList", key_val(vec![("header", header), ("items", Value::Dict(items))]))])
}

/// Missions of `kind` still outstanding and not expired, that `open` lets
/// through, by missionID.
fn outstanding<'a>(db: &'a Db, kind: i16, open: impl Fn(&Row) -> bool) -> Vec<Row<'a>> {
    let now = crate::bulkdata::filetime_now();
    let mut found: Vec<Row> = db
        .table("courierMissions")
        .rows()
        .filter(|m| m.int("kind") == Some(kind.into()) && m.int("state") == Some(OUTSTANDING.into()))
        .filter(|m| m.int("dateExpires").is_some_and(|d| d > now))
        .filter(|m| open(m))
        .collect();
    found.sort_by_key(|m| m.int("missionID"));
    found
}

/// Whether a filter the client may leave out (None) lets `value` through.
fn within<T: PartialOrd>(limit: Option<T>, value: Option<T>, ok: impl Fn(&T, &T) -> bool) -> bool {
    match limit {
        None => true,
        Some(l) => value.is_some_and(|v| ok(&v, &l)),
    }
}

/// GetAvailableCourierMissions(maxVolume, maxCollateral, regionID,
/// solarSystemID, stationID): outstanding missions open to the caller, by
/// where they are picked up (None for any).
fn available(ctx: &Ctx<'_>, call: &Call) -> Value {
    let me = ctx.session.char_id.unwrap_or(0);
    let corp = ctx.session.corp_id.unwrap_or(0);
    let f = |i: usize| call.args.get(i).and_then(|v| v.as_f64());
    let id = |i: usize| call.arg_i64(i);
    ctx.store.read(|db| {
        let found = outstanding(db, KIND_COURIER, |m| {
            m.int("assigneeID").is_none_or(|a| a == me || a == corp)
                && within(f(0), m.float("volume"), |v, l| v <= l)
                && within(f(1), m.float("acceptFee"), |v, l| v <= l)
                && within(id(2), m.int("pickupRegionID"), |v, l| v == l)
                && within(id(3), m.int("pickupSolarSystemID"), |v, l| v == l)
                && within(id(4), m.int("pickupStationID"), |v, l| v == l)
        });
        rowset(COURIER_HEADER, found.iter().map(|m| Value::List(m.line(COURIER_HEADER))).collect())
    })
}

/// GetMyCourierMissions(): those the caller issued or carries, still open.
fn mine(ctx: &Ctx<'_>) -> Value {
    let me = ctx.session.char_id.unwrap_or(0);
    ctx.store.read(|db| {
        let mut found: Vec<Row> = db
            .table("courierMissions")
            .rows()
            .filter(|m| m.int("kind") == Some(KIND_COURIER.into()))
            .filter(|m| m.int("state").is_some_and(|s| s == i64::from(OUTSTANDING) || s == i64::from(ACCEPTED)))
            .filter(|m| m.int("issuerID") == Some(me) || m.int("acceptorID") == Some(me))
            .collect();
        found.sort_by_key(|m| m.int("missionID"));
        rowset(COURIER_HEADER, found.iter().map(|m| Value::List(m.line(COURIER_HEADER))).collect())
    })
}

/// GetAvailableEscrows(maxPrice, regionID, solarSystemID, stationID,
/// assigneeID): (escrows, names), names covering the stations and people.
fn escrows(ctx: &Ctx<'_>, call: &Call) -> Value {
    const COLUMNS: &[&str] = &[
        "missionID", "acceptFee", "pickupStationID", "pickupSolarSystemID", "pickupRegionID", "issuerID", "assigneeID",
        "acceptorID", "dateExpires", "description",
    ];
    let me = ctx.session.char_id.unwrap_or(0);
    let corp = ctx.session.corp_id.unwrap_or(0);
    let id = |i: usize| call.arg_i64(i);
    let max_price = call.args.first().and_then(|v| v.as_f64());
    let (lines, names) = ctx.store.read(|db| {
        let found = outstanding(db, KIND_ESCROW, |m| {
            (m.int("assigneeID").is_none_or(|a| a == me || a == corp) || m.int("issuerID") == Some(me))
                && within(max_price, m.float("acceptFee"), |v, l| v <= l)
                && within(id(1), m.int("pickupRegionID"), |v, l| v == l)
                && within(id(2), m.int("pickupSolarSystemID"), |v, l| v == l)
                && within(id(3), m.int("pickupStationID"), |v, l| v == l)
                && within(id(4), m.int("assigneeID"), |v, l| v == l)
        });
        let ids: Vec<i64> = found.iter().flat_map(|m| ["pickupStationID", "issuerID", "assigneeID"].map(|c| m.int(c))).flatten().collect();
        let name = |id: i64| {
            let station = || db.table("staStations").get_id(id).and_then(|s| s.text("stationName"));
            let character = || db.table("character_").get_id(id).and_then(|c| c.text("characterName"));
            let corporation = || db.table("corporation").get_id(id).and_then(|c| c.text("corporationName"));
            station().or_else(character).or_else(corporation).unwrap_or_default()
        };
        let names: Vec<(Value, Value)> = ids.iter().map(|id| (Value::Int(*id), Value::text(name(*id)))).collect();
        let lines: Vec<Value> = found.iter().map(|m| Value::List(m.line(COLUMNS))).collect();
        (lines, names)
    });
    key_val(vec![("escrows", rowset(ESCROW_HEADER, lines)), ("names", Value::Dict(names))])
}

/// GetManifest(escrowID): what the package holds.
fn manifest(ctx: &Ctx<'_>, mission_id: i64) -> Value {
    let lines = ctx.store.read(|db| {
        let Some(crater) = db.table("courierMissions").get_id(mission_id).and_then(|m| m.int("craterID")) else { return Vec::new() };
        let blueprints = db.table("invBlueprints");
        let mut inside: Vec<Row> = db.table("entity").find(&["locationID"], &[int(crater)]).collect();
        inside.sort_by_key(|e| e.int("itemID"));
        inside
            .iter()
            .map(|e| {
                let b = blueprints.get_id(e.int("itemID").unwrap_or(0));
                let blueprint = |c: &str| b.map_or(Value::None, |b| b.cell(c));
                Value::List(vec![
                    e.cell("itemID"),
                    e.cell("typeID"),
                    e.cell("quantity"),
                    Value::None,
                    Value::Int(b.and_then(|b| b.int("copy")).unwrap_or(0)),
                    blueprint("productivityLevel"),
                    blueprint("materialLevel"),
                    blueprint("licensedProductionRunsRemaining"),
                ])
            })
            .collect()
    });
    rowset(MANIFEST_HEADER, lines)
}

// ---- making one --------------------------------------------------------------------------

/// CreateCourierMission(items, dropStationID, description, collateral,
/// assignedTo=, tracking=, reward=, expiryDate=) -> (missionID, excluded):
/// excluded as (item, None, (reason,)) for what could not go in.
async fn create(ctx: &Ctx<'_>, call: &Call, station: i64) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let ids: Vec<i64> = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(item_id_of).collect();
    let drop = call.arg_i64(1).unwrap_or(station);
    let description: Option<String> = call.arg_text(2).map(|d| d.chars().take(1_000).collect());
    let collateral = call.args.get(3).and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0);
    let assignee = call.kwarg("assignedTo").and_then(|v| v.as_i64());
    let tracking = call.kwarg("tracking").and_then(|v| v.as_i64()).unwrap_or(0) as i16;
    let reward = call.kwarg("reward").and_then(|v| v.as_f64()).unwrap_or(0.0).max(0.0);
    let now = crate::bulkdata::filetime_now();
    let expires = call.kwarg("expiryDate").and_then(|v| v.as_i64()).unwrap_or(now + 24 * 3_600 * 10_000_000);
    let kind = if drop == station { KIND_ESCROW } else { KIND_COURIER };
    if expires <= now {
        return Err(Outcome::custom("The mission would have expired already."));
    }

    let (mission_id, excluded, taken, volume, events) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let mut txn = ItemTxn::new(tx);
            let place = |id: i64| {
                let s = txn.db().table("staStations").get_id(id)?;
                Some((s.int("solarSystemID")?, s.int("regionID")?))
            };
            let (Some((pickup_system, pickup_region)), Some((drop_system, drop_region))) = (place(station), place(drop)) else {
                return Err(Outcome::custom("That station does not exist.").into());
            };

            let mut taken = Vec::new();
            let mut excluded = Vec::new();
            let mut volume = 0.0;
            for id in ids {
                let Some(item) = txn.item(id).map_err(|e| db_failed("reading the items", e))? else { continue };
                let reason = if item.owner_id != me {
                    Some("Not yours")
                } else if item.location_id != station || item.flag != FLAG_HANGAR {
                    Some("Not in your hangar here")
                } else if item.type_id == TYPE_PLASTIC_WRAP {
                    Some("Already a package")
                } else if crate::svc::votes::locked(txn.db(), item.id) {
                    Some("Locked down")
                } else {
                    None
                };
                match reason {
                    Some(why) => {
                        let cells = txn.db().table("entity").get_id(id).and_then(|r| invitem_cells(&r));
                        let rec = row(INVITEM_HEADER, cells.unwrap_or_default());
                        excluded.push(Value::Tuple(vec![rec, Value::None, Value::Tuple(vec![Value::text(why)])]));
                    }
                    None => {
                        volume += item.volume * item.quantity as f64;
                        taken.push(item.id);
                    }
                }
            }
            if taken.is_empty() {
                return Err(Outcome::custom("None of those items can go in a package.").into());
            }
            if volume > MAX_PACKAGE {
                return Err(Outcome::custom(format!("A package holds at most {MAX_PACKAGE} m3; this one would be {volume:.1} m3.")).into());
            }
            if reward > 0.0 {
                let reason = format!("Courier mission reward held ({drop})");
                let entry = Entry { ref_type: REF_ESCROW, payer: me, payee: OWNER_SYSTEM, arg_id1: "", reason: &reason };
                pay(&mut txn, me, -reward, &entry)?;
            }
            let wrap = NewItem { type_id: TYPE_PLASTIC_WRAP, owner_id: OWNER_SYSTEM, location_id: station, flag: FLAG_HANGAR, quantity: 1, singleton: true };
            let crater = txn.create(wrap).map_err(|e| db_failed("wrapping the package", e))?;
            for id in &taken {
                txn.move_to(*id, crater, 0).map_err(|e| db_failed("wrapping the package", e))?;
            }
            let collateral = store::Value::Float(collateral);
            let mission = [
                ("kind", int(kind.into())),
                ("state", int(0)),
                ("availabilityID", int(0)),
                ("inOrder", int(0)),
                ("description", description.as_deref().map_or(store::Value::Null, |d| store::Value::Text(d.into()))),
                ("issuerID", int(me)),
                ("assigneeID", assignee.map_or(store::Value::Null, int)),
                ("acceptFee", collateral.clone()),
                ("dateIssued", int(now)),
                ("dateExpires", int(expires)),
                ("totalReward", store::Value::Float(reward)),
                ("tracking", int(tracking.into())),
                ("pickupStationID", int(station)),
                ("craterID", int(crater)),
                ("dropStationID", int(drop)),
                ("volume", store::Value::Float(volume)),
                ("pickupSolarSystemID", int(pickup_system)),
                ("pickupRegionID", int(pickup_region)),
                ("dropSolarSystemID", int(drop_system)),
                ("dropRegionID", int(drop_region)),
                ("price", collateral),
                ("regionID", int(pickup_region)),
                ("solarsystemID", int(pickup_system)),
                ("stationID", int(station)),
            ];
            let tx = txn.tx();
            let row = tx.insert("courierMissions", &mission).map_err(|e| db_failed("recording the mission", e))?;
            let mission_id = tx.table("courierMissions").row(row).and_then(|m| m.int("missionID")).unwrap_or(0);
            Ok((mission_id, excluded, taken, volume, txn.events()))
        })
        .map_err(|Undone(o)| o)?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    tell_balance(ctx, me);
    let what = if kind == KIND_ESCROW { "escrow" } else { "courier mission" };
    info!("{me} made {what} {mission_id}: {} item(s), {volume:.1} m3, {station} -> {drop}, reward {reward}, collateral {collateral}", taken.len());
    Ok(Value::Tuple(vec![Value::Int(mission_id), Value::List(excluded)]))
}

/// An item ID, or the itemID of an item row.
fn item_id_of(v: &Value) -> Option<i64> {
    if let Some(id) = v.as_i64() {
        return Some(id);
    }
    let Value::Instance { state, .. } = v else { return None };
    let Value::Dict(d) = state.as_ref() else { return None };
    d.iter().find(|(k, _)| *k == Value::bytes("line")).and_then(|(_, l)| l.seq()).and_then(|l| l.first()).and_then(|v| v.as_i64())
}

// ---- taking, delivering, cancelling ------------------------------------------------------

struct Open {
    kind: i16,
    state: i16,
    issuer: i64,
    assignee: Option<i64>,
    acceptor: Option<i64>,
    fee: f64,
    reward: f64,
    pickup: i64,
    drop: i64,
    crater: i64,
}

fn open_mission(db: &Db, id: i64) -> Refused<Open> {
    let r = db.table("courierMissions").get_id(id).ok_or_else(|| Outcome::custom("That mission no longer exists."))?;
    Ok(Open {
        kind: r.int("kind").unwrap_or(0) as i16,
        state: r.int("state").unwrap_or(0) as i16,
        issuer: r.int("issuerID").unwrap_or(0),
        assignee: r.int("assigneeID"),
        acceptor: r.int("acceptorID"),
        fee: r.float("acceptFee").unwrap_or(0.0),
        reward: r.float("totalReward").unwrap_or(0.0),
        pickup: r.int("pickupStationID").unwrap_or(0),
        drop: r.int("dropStationID").unwrap_or(0),
        crater: r.int("craterID").unwrap_or(0),
    })
}

/// Sets columns of the mission `id`.
fn set_mission(txn: &mut ItemTxn<'_, '_>, id: i64, sets: store::Sets, what: &str) -> Refused<()> {
    let tx = txn.tx();
    let Some(row) = tx.table("courierMissions").get_id(id).map(|m| m.id) else { return Ok(()) };
    tx.update("courierMissions", row, sets).map_err(|e| db_failed(what, e))
}

/// What AcceptCourierMission did.
enum Accepted {
    /// The issuer took their own escrow back.
    TakenBack,
    Taken,
}

/// AcceptCourierMission(missionID): take on a courier mission, or buy an
/// escrow; on one's own escrow, take it back.
async fn accept(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let corp = ctx.session.corp_id.unwrap_or(0);
    let (m, accepted, events) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let mut txn = ItemTxn::new(tx);
            let m = open_mission(txn.db(), id)?;
            if m.state != OUTSTANDING {
                return Err(Outcome::custom("Someone has already taken that.").into());
            }
            if m.kind == KIND_ESCROW && m.issuer == me {
                close_unaccepted(&mut txn, id, &m, CANCELLED)?;
                return Ok((m, Accepted::TakenBack, txn.events()));
            }
            if m.issuer == me {
                return Err(Outcome::custom("You cannot accept your own mission.").into());
            }
            if m.assignee.is_some_and(|a| a != me && a != corp) {
                return Err(Outcome::custom("That is meant for someone else.").into());
            }
            let now = crate::bulkdata::filetime_now();
            let reason = format!("Courier mission {id}");
            if m.kind == KIND_ESCROW {
                // The price goes to the seller; the goods come out of the wrap here.
                let entry = Entry { ref_type: REF_ESCROW, payer: me, payee: m.issuer, arg_id1: "", reason: &reason };
                pay(&mut txn, me, -m.fee, &entry)?;
                pay(&mut txn, m.issuer, m.fee, &entry)?;
                unwrap(&mut txn, m.crater, me, m.pickup)?;
                let done = [("state", int(DONE.into())), ("acceptorID", int(me)), ("dateAccepted", int(now)), ("dateCompleted", int(now))];
                set_mission(&mut txn, id, &done, "closing the escrow")?;
            } else {
                let entry = Entry { ref_type: REF_ESCROW, payer: me, payee: OWNER_SYSTEM, arg_id1: "", reason: &reason };
                if m.fee > 0.0 {
                    pay(&mut txn, me, -m.fee, &entry)?;
                }
                txn.set_owner(m.crater, me).map_err(|e| db_failed("handing over the package", e))?;
                let took = [("state", int(ACCEPTED.into())), ("acceptorID", int(me)), ("dateAccepted", int(now))];
                set_mission(&mut txn, id, &took, "accepting the mission")?;
            }
            Ok((m, Accepted::Taken, txn.events()))
        })
        .map_err(|Undone(o)| o)?;
    if let Accepted::TakenBack = accepted {
        return Ok(closed_unaccepted(ctx, events, id, &m, CANCELLED));
    }
    crate::items::notify(ctx.world, ctx.client_id, events);
    tell_balance(ctx, me);
    tell_balance(ctx, m.issuer);
    info!("{me} took {} {id}", if m.kind == KIND_ESCROW { "escrow" } else { "courier mission" });
    Ok(Value::None)
}

/// DeliverCourierPackage(missionID): at the drop-off, the package's contents
/// go to the issuer, and the courier is paid.
async fn deliver(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let (m, events) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let mut txn = ItemTxn::new(tx);
            let m = open_mission(txn.db(), id)?;
            if m.state != ACCEPTED || m.acceptor != Some(me) {
                return Err(Outcome::custom("That is not a mission you are carrying.").into());
            }
            if ctx.session.station_id != Some(m.drop) {
                return Err(Outcome::custom("You have to be docked at the drop-off station.").into());
            }
            let package = txn.item(m.crater).map_err(|e| db_failed("reading the package", e))?;
            if !package.is_some_and(|p| p.location_id == m.drop && p.owner_id == me && p.flag == FLAG_HANGAR) {
                return Err(Outcome::custom("The package has to be in your hangar here.").into());
            }
            unwrap(&mut txn, m.crater, m.issuer, m.drop)?;
            let reason = format!("Courier mission {id}");
            let entry = Entry { ref_type: REF_ESCROW, payer: OWNER_SYSTEM, payee: me, arg_id1: "", reason: &reason };
            if m.reward + m.fee > 0.0 {
                pay(&mut txn, me, m.reward + m.fee, &entry)?;
            }
            let now = crate::bulkdata::filetime_now();
            set_mission(&mut txn, id, &[("state", int(DONE.into())), ("dateCompleted", int(now))], "closing the mission")?;
            Ok((m, txn.events()))
        })
        .map_err(|Undone(o)| o)?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    tell_balance(ctx, me);
    info!("{me} delivered courier mission {id} to {}", m.drop);
    Ok(Value::None)
}

/// CancelMission(missionID): the issuer takes back what nobody has accepted.
async fn cancel(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let (m, events) = ctx
        .store
        .write(|tx| -> Result<_, Undone> {
            let mut txn = ItemTxn::new(tx);
            let m = open_mission(txn.db(), id)?;
            if m.issuer != me {
                return Err(Outcome::custom("That is not your mission.").into());
            }
            if m.state != OUTSTANDING {
                return Err(Outcome::custom("Someone has already accepted that mission.").into());
            }
            close_unaccepted(&mut txn, id, &m, CANCELLED)?;
            Ok((m, txn.events()))
        })
        .map_err(|Undone(o)| o)?;
    Ok(closed_unaccepted(ctx, events, id, &m, CANCELLED))
}

/// An unaccepted mission ends: the items and the reward go back to the issuer.
fn close_unaccepted(txn: &mut ItemTxn<'_, '_>, id: i64, m: &Open, state: i16) -> Refused<()> {
    unwrap(txn, m.crater, m.issuer, m.pickup)?;
    if m.reward > 0.0 {
        let reason = format!("Courier mission {id} reward returned");
        let entry = Entry { ref_type: REF_ESCROW, payer: OWNER_SYSTEM, payee: m.issuer, arg_id1: "", reason: &reason };
        pay(txn, m.issuer, m.reward, &entry)?;
    }
    let closed = [("state", int(state.into())), ("dateCompleted", int(crate::bulkdata::filetime_now()))];
    set_mission(txn, id, &closed, "closing the mission")
}

/// Once [`close_unaccepted`] has committed: the item events and the
/// issuer's balance.
fn closed_unaccepted(ctx: &Ctx<'_>, events: Vec<Value>, id: i64, m: &Open, state: i16) -> Value {
    crate::items::notify(ctx.world, ctx.client_id, events);
    tell_balance(ctx, m.issuer);
    info!("mission {id} closed unaccepted ({})", if state == LAPSED { "lapsed" } else { "taken back" });
    Value::None
}

/// Missions past their date: unaccepted ones go back to their issuers,
/// accepted ones forfeit the collateral to them.
async fn settle_lapsed(ctx: &Ctx<'_>) {
    let now = crate::bulkdata::filetime_now();
    let lapsed: Vec<i64> = ctx.store.read(|db| {
        let mut ids: Vec<i64> = db
            .table("courierMissions")
            .rows()
            .filter(|m| m.int("state").is_some_and(|s| s == i64::from(OUTSTANDING) || s == i64::from(ACCEPTED)))
            .filter(|m| m.int("dateExpires").is_some_and(|d| d <= now))
            .filter_map(|m| m.int("missionID"))
            .collect();
        ids.sort_unstable();
        ids.truncate(50);
        ids
    });
    for id in lapsed {
        let settled = ctx.store.write(|tx| -> Result<_, Undone> {
            let mut txn = ItemTxn::new(tx);
            let m = open_mission(txn.db(), id)?;
            if m.state == OUTSTANDING {
                close_unaccepted(&mut txn, id, &m, LAPSED)?;
                return Ok((m, true, txn.events()));
            }
            let reason = format!("Courier mission {id} failed: collateral");
            let entry = Entry { ref_type: REF_ESCROW, payer: OWNER_SYSTEM, payee: m.issuer, arg_id1: "", reason: &reason };
            if m.fee > 0.0 {
                pay(&mut txn, m.issuer, m.fee, &entry)?;
            }
            set_mission(&mut txn, id, &[("state", int(LAPSED.into())), ("dateCompleted", int(now))], "failing the mission")?;
            Ok((m, false, Vec::new()))
        });
        match settled {
            Ok((m, true, events)) => {
                closed_unaccepted(ctx, events, id, &m, LAPSED);
            }
            Ok((m, false, _)) => info!("courier mission {id} lapsed undelivered; the collateral went to {}", m.issuer),
            Err(_) => error!("courier mission {id} could not be settled"),
        }
    }
}

/// Takes a package apart: its contents to `owner`'s hangar at `station`.
fn unwrap(txn: &mut ItemTxn<'_, '_>, crater: i64, owner: i64, station: i64) -> Refused<()> {
    let inside: Vec<i64> = txn.db().table("entity").find(&["locationID"], &[int(crater)]).filter_map(|r| r.int("itemID")).collect();
    for id in inside {
        txn.item(id).map_err(|e| db_failed("opening the package", e))?;
        txn.move_to(id, station, FLAG_HANGAR).map_err(|e| db_failed("opening the package", e))?;
        txn.set_owner(id, owner).map_err(|e| db_failed("opening the package", e))?;
    }
    if txn.item(crater).map_err(|e| db_failed("opening the package", e))?.is_some() {
        txn.destroy(crater).map_err(|e| db_failed("opening the package", e))?;
    }
    Ok(())
}

/// The volume of what a package holds.
pub fn package_volume(db: &Db, crater: i64) -> Result<f64, StoreError> {
    let types = crate::sde::table("invTypes");
    // Items of an unknown type or without a volume add nothing.
    let volumes = db.table("entity").find(&["locationID"], &[int(crater)]).filter_map(|r| {
        let volume = types.get("typeID", r.int("typeID")?)?.float("volume")?;
        Some(volume * r.int("quantity")? as f64)
    }).collect::<Vec<f64>>();
    Ok(volumes.into_iter().reduce(|a, b| a + b).unwrap_or(0.0))
}

fn pay(txn: &mut ItemTxn<'_, '_>, owner: i64, delta: f64, entry: &Entry<'_>) -> Refused<f64> {
    match wallet::post(txn.tx(), owner, delta, entry) {
        Ok(b) => Ok(b),
        Err(Transfer::NotEnough { need, have }) => Err(Outcome::not_enough_money(need, have)),
        Err(Transfer::NoSuchOwner) => Err(Outcome::custom("That wallet could not be found.")),
        Err(Transfer::Db(e)) => Err(db_failed("moving the money", e)),
    }
}

fn tell_balance(ctx: &Ctx<'_>, owner: i64) {
    let balance = ctx.store.read(|db| db.table("character_").get_id(owner).and_then(|c| c.float("balance")));
    let Some(balance) = balance else { return };
    let w = ctx.world.lock();
    let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(owner), Value::Double(balance)]);
    w.notify(&w.clients_of(owner), "OnAccountChange", cash);
}
