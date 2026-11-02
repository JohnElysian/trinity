//! Scanning (scanner.py, scansvc.py, moonscan.py, surveyscan.py, shipui.py).
//!
//! - **Directional scan** (beyonce.ConeScan): what lies in a cone from the
//!   ship, within a range: ships (not cloaked ones), structures, celestials,
//!   beacons.
//! - **Scanner probes**: a Scan Probe Launcher fires one into space where the
//!   ship is, for its lifetime (explosionDelay), covering a sphere of its scan
//!   range (in AU). beyonce.GetScans lists the pilot's probes and the last
//!   analysis; beyonce.CombineScans(probes, groupID) analyses with as many
//!   probes as the type works in (probesInGroup), for one group: ships of a
//!   group, stations, or cosmic signatures (hidden deadspace complexes).
//!   What lies within reach of every probe is found, its position known to
//!   within the probes' deviation; the analysis runs as the launcher's
//!   systemScan effect and the results come when it ends. A result can be
//!   warped to (WarpToStuff 'scan'). GiveProbe hands a probe to another pilot.
//! - **Survey probes** (Quest, Discovery, Gaze) fly for their lifetime and
//!   then survey every moon within their range of where they were launched:
//!   OnMoonScanComplete with what each moon holds (the same materials moon
//!   harvesters mine), kept in `moonScans`.
//! - **Ship, cargo and survey scanners**: at the end of a cycle,
//!   OnShipScanComplete (the target's capacitor and modules),
//!   OnCargoScanComplete (its hold) and OnSurveyScanComplete (asteroids in
//!   range and the ore in each).

use std::collections::HashMap;
use std::time::Duration;

use eve_marshal::Value;
use tracing::info;

use crate::ballpark::{Park, Vec3};
use crate::beacons::{self, Beacon, Kind};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{float, int, rowset};

const AU: f64 = 149_597_870_700.0;
const GROUP_SCANNER_PROBE: i64 = 479;
const GROUP_SURVEY_PROBE: i64 = 492;
const GROUP_PROBE_LAUNCHER: i64 = 481;
const GROUP_CARGO_SCANNER: i64 = 47;
const GROUP_SHIP_SCANNER: i64 = 48;
const GROUP_SURVEY_SCANNER: i64 = 49;
const GROUP_STATION: i64 = 15;
pub const GROUP_COSMIC_SIGNATURE: i64 = 502;
const TYPE_DEADSPACE_SIGNATURE: i64 = 19728;
const CATEGORY_SHIP: i64 = 6;
const ATTR_LIFETIME: i32 = 281;
const ATTR_SCAN_RANGE: i32 = 765;
const ATTR_PROBES_IN_GROUP: i32 = 794;
const ATTR_MIN_DEVIATION: i32 = 787;
const ATTR_MAX_DEVIATION: i32 = 788;
const ATTR_SURVEY_RANGE: i32 = 197;
const EFFECT_SYSTEM_SCAN: i64 = 1150;
/// How long an analysis takes.
const ANALYSIS_MS: i64 = 10_000;
const FLAG_CARGO: i32 = 5;

/// A pilot's probe out in space.
#[derive(Debug, Clone)]
pub struct Probe {
    pub owner: i64,
    pub type_id: i64,
    pub system: i64,
    pub pos: Vec3,
    pub until_ms: i64,
    /// AU.
    pub range: f64,
    pub group_count: i64,
    pub deviation: (f64, f64),
}

/// Something an analysis found.
#[derive(Debug, Clone)]
pub struct Found {
    pub id: i64,
    pub signal: f64,
    pub pos: Vec3,
    pub accuracy: f64,
    pub system: i64,
}

pub async fn beyonce(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let me = ctx.session.char_id?;
    Some(match call.method.as_str() {
        "ConeScan" => cone_scan(ctx, call),
        "GetScans" => Outcome::Value(get_scans(ctx, me)),
        "CombineScans" => match combine(ctx, me, call.args.first(), call.arg_id(1)).await {
            Ok(()) => Outcome::Value(Value::None),
            Err(why) => Outcome::custom(why),
        },
        "GiveProbe" => match give(ctx, me, call.arg_id(0), call.arg_id(1)) {
            Ok(()) => Outcome::Value(Value::None),
            Err(why) => Outcome::custom(why),
        },
        _ => return None,
    })
}

/// ConeScan(angle, range, x, y, z): rows of (id, typeID, groupID).
fn cone_scan(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    let f = |i: usize| call.args.get(i).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let (angle, range) = (f(0).clamp(0.01, std::f64::consts::TAU), f(1).max(0.0));
    let dir = Vec3::new(f(2), f(3), f(4));
    let dir = if dir.len() > 1e-9 { dir.unit() } else { Vec3::new(0.0, 0.0, 1.0) };
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else { return Outcome::custom("You are not in space.") };
    let w = ctx.world.lock();
    let Some(park) = w.parks.get(&system) else { return Outcome::Value(rowset(&["id", "typeID", "groupID"], Vec::new())) };
    let Some(me) = park.ships.get(&ship_id).map(|s| s.pos) else { return Outcome::custom("You are not in space.") };
    let half = angle / 2.0;
    let seen = |pos: Vec3| {
        let d = pos - me;
        let len = d.len();
        len <= range && (len < 1.0 || angle >= std::f64::consts::TAU - 1e-6 || (d.unit().dot(dir)).clamp(-1.0, 1.0).acos() <= half)
    };
    let mut lines: Vec<Value> = Vec::new();
    let mut add = |id: i64, type_id: i64, group_id: i64| lines.push(Value::List(vec![Value::Int(id), Value::Int(type_id), Value::Int(group_id)]));
    for s in park.ships.values().filter(|s| s.id != ship_id && !s.cloaked && seen(s.pos)) {
        add(s.id, s.pilot.type_id, s.pilot.group_id);
    }
    for s in park.structures.values().filter(|s| seen(s.pos)) {
        add(s.id, s.type_id, s.group_id);
    }
    for c in park.celestials.iter().filter(|c| seen(c.pos)) {
        add(c.id, c.type_id, c.group_id);
    }
    for b in park.beacons.values().filter(|b| seen(b.pos) && !matches!(b.kind, Kind::Probe { .. })) {
        add(b.id, b.type_id, beacon_group(b));
    }
    Outcome::Value(rowset(&["id", "typeID", "groupID"], lines))
}

fn beacon_group(b: &Beacon) -> i64 {
    match b.kind {
        Kind::Gate { .. } => 366,
        _ => 310,
    }
}

/// GetScans: {'probes': {probeID: probe}, 'results': [(id, signal, pos, accuracy)]}.
fn get_scans(ctx: &Ctx<'_>, me: i64) -> Value {
    let system = ctx.session.solar_system_id.unwrap_or(0);
    let now = crate::combat::now_ms();
    let w = ctx.world.lock();
    let probes: Vec<(Value, Value)> = w
        .probes
        .iter()
        .filter(|(_, p)| p.owner == me && p.system == system && p.until_ms > now)
        .map(|(id, p)| {
            let kv = key_val(vec![
                ("probeID", Value::Int(*id)),
                ("typeID", Value::Int(p.type_id)),
                ("expiry", Value::Long(crate::combat::filetime(p.until_ms))),
                ("pos", point(p.pos)),
                ("scanRange", Value::Double(p.range)),
            ]);
            (Value::Int(*id), kv)
        })
        .collect();
    let results: Vec<Value> = w
        .scan_results
        .get(&me)
        .map(|list| {
            list.iter()
                .filter(|r| r.system == system)
                .map(|r| Value::Tuple(vec![Value::Int(r.id), Value::Double((r.signal * 10.0).round() / 10.0), point(r.pos), Value::Double(r.accuracy)]))
                .collect()
        })
        .unwrap_or_default();
    Value::Dict(vec![(Value::bytes("probes"), Value::Dict(probes)), (Value::bytes("results"), Value::List(results))])
}

fn point(p: Vec3) -> Value {
    Value::Tuple(vec![Value::Double(p.x), Value::Double(p.y), Value::Double(p.z)])
}

fn key_val(fields: Vec<(&str, Value)>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.KeyVal")),
        state: Box::new(Value::Dict(fields.into_iter().map(|(k, v)| (Value::bytes(k), v)).collect())),
    }
}

/// GiveProbe(charID, probeID).
fn give(ctx: &Ctx<'_>, me: i64, to: i64, probe: i64) -> Result<(), String> {
    let mut w = ctx.world.lock();
    let Some(p) = w.probes.get_mut(&probe).filter(|p| p.owner == me) else { return Err("That is not your probe.".into()) };
    p.owner = to;
    info!("{me} gave probe {probe} to {to}");
    Ok(())
}

/// The launcher fitted to a ship, for the analysis's effect.
async fn launcher(ctx: &Ctx<'_>, ship_id: i64) -> Option<i64> {
    ctx.query_one(
        r#"SELECT e."itemID" FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID" WHERE e."locationID" = $1 AND t."groupID" = $2 AND e."flag" BETWEEN 11 AND 34 LIMIT 1"#,
        &[&(ship_id as i32), &(GROUP_PROBE_LAUNCHER as i32)],
    )
    .await
    .and_then(|r| int(&r, 0))
}

fn scan_event(char_id: i64, ship_id: i64, module: i64, start: bool, started: i64, now: i64) -> Value {
    let environment = Value::List(vec![Value::Int(module), Value::Int(char_id), Value::Int(ship_id), Value::None, Value::None, Value::List(Vec::new()), Value::Int(EFFECT_SYSTEM_SCAN)]);
    Value::Tuple(vec![
        Value::Int(module),
        Value::Int(EFFECT_SYSTEM_SCAN),
        Value::Long(crate::combat::filetime(now)),
        Value::Int(i64::from(start)),
        Value::Int(i64::from(start)),
        environment,
        Value::Long(crate::combat::filetime(started)),
        Value::Int(ANALYSIS_MS),
        Value::Int(0),
        Value::None,
        Value::None,
    ])
}

/// CombineScans(probeIDs, groupID): an analysis, its results due when it ends.
async fn combine(ctx: &Ctx<'_>, me: i64, ids: Option<&Value>, group: i64) -> Result<(), String> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else { return Err("You are not in space.".into()) };
    let ids: Vec<i64> = match ids {
        Some(Value::List(v)) | Some(Value::Tuple(v)) => v.iter().filter_map(|v| v.as_i64()).collect(),
        _ => Vec::new(),
    };
    let now = crate::combat::now_ms();
    let probes: Vec<Probe> = {
        let w = ctx.world.lock();
        ids.iter().filter_map(|id| w.probes.get(id)).filter(|p| p.owner == me && p.system == system && p.until_ms > now).cloned().collect()
    };
    if probes.is_empty() || probes.len() != ids.len() {
        return Err("Those probes are not yours, or no longer out.".into());
    }
    let need = probes[0].group_count.max(1) as usize;
    if probes.len() != need || probes.iter().any(|p| p.type_id != probes[0].type_id) {
        return Err(format!("This analysis takes {need} probes of the same type."));
    }
    let module = launcher(ctx, ship_id).await.ok_or("You have no probe launcher fitted.")?;
    let category_ship = ctx
        .query_one(r#"SELECT "categoryID" FROM "invGroups" WHERE "groupID" = $1"#, &[&(group as i32)])
        .await
        .and_then(|r| int(&r, 0))
        == Some(CATEGORY_SHIP);
    let found = {
        let w = ctx.world.lock();
        match w.parks.get(&system) {
            Some(park) => analyse(park, &probes, group, category_ship, ship_id, now),
            None => Vec::new(),
        }
    };
    {
        let w = ctx.world.lock();
        w.notify(&[ctx.client_id], "OnGodmaShipEffect", scan_event(me, ship_id, module, true, now, now));
    }
    let (world, client) = (ctx.world.clone(), ctx.client_id);
    info!("{me} analyses {} probes for group {group}: {} found", probes.len(), found.len());
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(ANALYSIS_MS as u64)).await;
        let mut w = world.lock();
        w.scan_results.insert(me, found);
        w.notify(&[client], "OnGodmaShipEffect", scan_event(me, ship_id, module, false, now, crate::combat::now_ms()));
    });
    Ok(())
}

/// What lies within every probe's reach, of the group asked for.
fn analyse(park: &Park, probes: &[Probe], group: i64, ships: bool, me: i64, now: i64) -> Vec<Found> {
    let mut candidates: Vec<(i64, Vec3, f64)> = Vec::new();
    if ships {
        for s in park.ships.values().filter(|s| s.id != me && s.pilot.group_id == group && !s.cloaked) {
            candidates.push((s.id, s.pos, s.fight.as_ref().map_or(100.0, |f| f.sig())));
        }
    } else if group == GROUP_STATION {
        for c in park.celestials.iter().filter(|c| c.group_id == GROUP_STATION) {
            candidates.push((c.id, c.pos, 5_000.0));
        }
    } else if group == GROUP_COSMIC_SIGNATURE {
        for b in park.beacons.values().filter(|b| b.type_id == TYPE_DEADSPACE_SIGNATURE) {
            candidates.push((b.id, b.pos, 300.0));
        }
    }
    let mut rng = crate::combat::Rng::seeded(now as u64 ^ me as u64);
    let (min_dev, max_dev) = probes[0].deviation;
    candidates
        .into_iter()
        .filter(|(_, pos, _)| probes.iter().all(|p| (*pos - p.pos).len() <= p.range * AU))
        .map(|(id, pos, sig)| {
            // Further from the probes, weaker and vaguer.
            let nearest = probes.iter().map(|p| (pos - p.pos).len() / (p.range * AU).max(1.0)).fold(0.0, f64::max);
            let accuracy = min_dev + (max_dev - min_dev) * nearest.clamp(0.0, 1.0) * rng.unit();
            let off = rng.in_sphere(accuracy);
            let signal = (100.0 * (1.0 - nearest) * (sig / 400.0).sqrt().min(1.0)).clamp(1.0, 100.0);
            Found { id, signal, pos: pos + off, accuracy, system: park.solar_system_id }
        })
        .collect()
}

/// Where a scan result lies, for WarpToStuff('scan', resultID).
pub fn result_at(world: &crate::world::World, char_id: i64, id: i64) -> Option<(Vec3, f64)> {
    let w = world.lock();
    w.scan_results.get(&char_id)?.iter().find(|r| r.id == id).map(|r| (r.pos, 0.0))
}

/// Before a scanning module starts: a probe launcher needs probes loaded;
/// scanners need nothing more.
pub async fn may_scan(_ctx: &Ctx<'_>, _ship_id: i64) -> Result<(), String> {
    Ok(())
}

/// A scanning module started: probes go out, scanners report at the end of
/// their cycle.
pub async fn scan_started(ctx: &Ctx<'_>, system: i64, ship_id: i64, module: i64, duration_ms: i64) {
    let Some(me) = ctx.session.char_id else { return };
    let Some(row) = ctx
        .query_one(
            r#"SELECT t."groupID", e."flag" FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID" WHERE e."itemID" = $1"#,
            &[&(module as i32)],
        )
        .await
    else {
        return;
    };
    let (group, flag) = (int(&row, 0).unwrap_or(0), int(&row, 1).unwrap_or(0));
    match group {
        GROUP_PROBE_LAUNCHER => launch_probe(ctx, me, system, ship_id, flag).await,
        GROUP_SHIP_SCANNER | GROUP_CARGO_SCANNER | GROUP_SURVEY_SCANNER => {
            let target = {
                let w = ctx.world.lock();
                w.parks.get(&system).and_then(|p| p.ships.get(&ship_id)).and_then(|s| s.fight.as_ref()).and_then(|f| f.module(module)).and_then(|m| m.target)
            };
            let (world, db_url, client) = (ctx.world.clone(), ctx.db_url.to_string(), ctx.client_id);
            let range = survey_range(ctx, me, ship_id, module).await;
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(duration_ms.max(1) as u64)).await;
                if let Err(e) = report(&world, &db_url, client, system, ship_id, group, target, range).await {
                    tracing::error!("scan results: {}", crate::logging::chain(&e));
                }
            });
        }
        _ => {}
    }
}

async fn survey_range(ctx: &Ctx<'_>, me: i64, ship_id: i64, module: i64) -> f64 {
    let active = std::collections::HashSet::new();
    let Ok(l) = crate::dogma::Loadout::load(ctx.db, ctx.dogma, me, Some(ship_id), &active).await else { return 15_000.0 };
    l.find(module).map_or(15_000.0, |i| l.value(i, ATTR_SURVEY_RANGE).max(1_000.0))
}

/// A scanner's cycle ended: what it saw.
#[allow(clippy::too_many_arguments)]
async fn report(world: &crate::world::World, db_url: &str, client: i64, system: i64, ship_id: i64, group: i64, target: Option<i64>, range: f64) -> Result<(), tokio_postgres::Error> {
    match group {
        GROUP_SURVEY_SCANNER => {
            let w = world.lock();
            let Some(park) = w.parks.get(&system) else { return Ok(()) };
            let Some(me) = park.ships.get(&ship_id).map(|s| s.pos) else { return Ok(()) };
            let rocks: Vec<Value> = park
                .rocks
                .values()
                .filter(|r| (r.pos - me).len() <= range)
                .map(|r| Value::Tuple(vec![Value::Int(r.id), Value::Int(r.type_id), Value::Int(r.quantity)]))
                .collect();
            w.notify(&[client], "OnSurveyScanComplete", Value::Tuple(vec![Value::List(rocks)]));
        }
        GROUP_SHIP_SCANNER | GROUP_CARGO_SCANNER => {
            let Some(target) = target else { return Ok(()) };
            let (bot_loot, cap) = {
                let w = world.lock();
                let s = w.parks.get(&system).and_then(|p| p.ships.get(&target));
                let cap = s.and_then(|s| s.fight.as_ref()).map_or(0.0, |f| {
                    let now = crate::combat::now_ms();
                    100.0 * f.vitals.capacitor.at(now) / f.vitals.capacitor.max.max(1.0)
                });
                (s.filter(|s| s.bot || s.is_npc()).map(|s| s.bot_loot.clone()), cap)
            };
            let items: Vec<(i64, i64)> = match bot_loot {
                Some(loot) => loot,
                None => {
                    let db = crate::db::connect(db_url).await?;
                    let sql = if group == GROUP_CARGO_SCANNER {
                        r#"SELECT "typeID", sum("quantity")::bigint FROM entity WHERE "locationID" = $1 AND "flag" = $2 GROUP BY 1"#
                    } else {
                        r#"SELECT "typeID", count(*)::bigint FROM entity WHERE "locationID" = $1 AND "flag" BETWEEN 11 AND 34 AND $2 >= 0 GROUP BY 1"#
                    };
                    db.query(sql, &[&(target as i32), &FLAG_CARGO]).await?.iter().map(|r| (int(r, 0).unwrap_or(0), int(r, 1).unwrap_or(0))).collect()
                }
            };
            let list = Value::List(items.iter().map(|(t, n)| Value::Tuple(vec![Value::Int(*t), Value::Int(*n)])).collect());
            let w = world.lock();
            if group == GROUP_CARGO_SCANNER {
                w.notify(&[client], "OnCargoScanComplete", Value::Tuple(vec![Value::Int(target), list]));
            } else {
                w.notify(&[client], "OnShipScanComplete", Value::Tuple(vec![Value::Int(target), Value::Double(cap), list]));
            }
        }
        _ => {}
    }
    Ok(())
}

/// A probe out of the launcher (the charge was used by the cycle).
async fn launch_probe(ctx: &Ctx<'_>, me: i64, system: i64, ship_id: i64, flag: i64) {
    let Some(row) = ctx
        .query_one(
            r#"SELECT e."typeID", t."groupID" FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                WHERE e."locationID" = $1 AND e."flag" = $2 AND t."groupID" IN ($3, $4) LIMIT 1"#,
            &[&(ship_id as i32), &(flag as i32), &(GROUP_SCANNER_PROBE as i32), &(GROUP_SURVEY_PROBE as i32)],
        )
        .await
    else {
        return;
    };
    let (type_id, group) = (int(&row, 0).unwrap_or(0), int(&row, 1).unwrap_or(0));
    let attrs: HashMap<i32, f64> = ctx
        .query(
            r#"SELECT "attributeID", COALESCE("valueFloat", "valueInt")::float8 FROM "dgmTypeAttributes" WHERE "typeID" = $1"#,
            &[&(type_id as i32)],
        )
        .await
        .iter()
        .filter_map(|r| Some((int(r, 0)? as i32, float(r, 1)?)))
        .collect();
    let a = |id: i32| attrs.get(&id).copied().unwrap_or(0.0);
    let now = crate::combat::now_ms();
    let lifetime = a(ATTR_LIFETIME).max(60_000.0) as i64;
    let Some(pos) = ctx.world.lock().parks.get(&system).and_then(|p| p.ships.get(&ship_id)).map(|s| s.pos) else { return };
    let id = crate::npc::next_id();
    let probe = Probe {
        owner: me,
        type_id,
        system,
        pos,
        until_ms: now + lifetime,
        range: a(ATTR_SCAN_RANGE).max(0.1),
        group_count: a(ATTR_PROBES_IN_GROUP) as i64,
        deviation: (a(ATTR_MIN_DEVIATION), a(ATTR_MAX_DEVIATION).max(a(ATTR_MIN_DEVIATION))),
    };
    {
        let mut w = ctx.world.lock();
        let beacon = Beacon::new(id, type_id, Some(me), "Probe", pos + Vec3::new(0.0, 2_000.0, 0.0), 10.0, Kind::Probe { char_id: me, until_ms: probe.until_ms });
        if let Some(park) = w.parks.get_mut(&system) {
            let entries = beacons::add(park, beacon);
            crate::svc::space::broadcast(&w, system, None, entries);
        }
        w.probes.retain(|_, p| p.until_ms > now);
        w.probes.insert(id, probe.clone());
    }
    info!("{me} launched probe {id} (type {type_id}) in {system}, {} AU, for {} s", probe.range, lifetime / 1_000);
    if group == GROUP_SURVEY_PROBE {
        let (world, db_url, client) = (ctx.world.clone(), ctx.db_url.to_string(), ctx.client_id);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(lifetime as u64)).await;
            if let Err(e) = survey_moons(&world, &db_url, client, me, system, pos, probe.range * AU).await {
                tracing::error!("moon survey: {}", crate::logging::chain(&e));
            }
        });
    }
}

/// A survey probe's flight is over: every moon within its range, and what
/// each holds.
async fn survey_moons(world: &crate::world::World, db_url: &str, client: i64, me: i64, system: i64, at: Vec3, range: f64) -> Result<(), tokio_postgres::Error> {
    let db = crate::db::connect(db_url).await?;
    let security: f64 = db
        .query_opt(r#"SELECT COALESCE("security", 0)::float8 FROM "mapSolarSystems" WHERE "solarSystemID" = $1"#, &[&(system as i32)])
        .await?
        .map_or(0.0, |r| r.get(0));
    let moons = db
        .query(r#"SELECT "itemID", "x", "y", "z" FROM "mapDenormalize" WHERE "solarSystemID" = $1 AND "groupID" = 8"#, &[&(system as i32)])
        .await?;
    let materials = crate::svc::moonmining::materials(&db).await;
    let now = crate::bulkdata::filetime_now();
    let mut surveyed = Vec::new();
    for m in moons {
        let pos = Vec3::new(float(&m, 1).unwrap_or(0.0), float(&m, 2).unwrap_or(0.0), float(&m, 3).unwrap_or(0.0));
        if (pos - at).len() > range {
            continue;
        }
        let moon = int(&m, 0).unwrap_or(0);
        let held = crate::svc::moonmining::moon_materials(moon, security, materials);
        let products: Vec<Value> = held
            .iter()
            .map(|(t, q)| key_val(vec![("celestialID", Value::Int(moon)), ("typeID", Value::Int(*t)), ("quantity", Value::Int(*q * 100))]))
            .collect();
        surveyed.push(moon as i32);
        if products.is_empty() {
            continue;
        }
        let w = world.lock();
        w.notify(&[client], "OnMoonScanComplete", Value::Tuple(vec![Value::List(products)]));
    }
    if !surveyed.is_empty() {
        db.execute(
            r#"INSERT INTO "moonScans" ("charID","moonID","scannedAt") SELECT $1, u, $3 FROM unnest($2::int[]) u
               ON CONFLICT ("charID","moonID") DO UPDATE SET "scannedAt" = EXCLUDED."scannedAt""#,
            &[&(me as i32), &surveyed, &now],
        )
        .await?;
    }
    info!("{me}'s survey probe in {system} surveyed {} moon(s)", surveyed.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ballpark::{Pilot, Ship, ShipStats};

    #[test]
    fn three_probes_find_what_lies_within_reach_of_all_of_them() {
        let mut park = Park::new(30_000_001, Vec::new(), 0);
        let pilot = |id| Pilot { type_id: 587, group_id: 25, name: String::new(), char_id: id, corp_id: 1, alliance_id: None, security: 0.0, bounty: 0.0 };
        let stats = || ShipStats::new(30.0, 1e6, 1.0, 300.0, 1.0);
        park.ships.insert(10, Ship::new(10, 0, stats(), pilot(10), Vec3::new(0.5 * AU, 0.0, 0.0), Vec3::ZERO));
        park.ships.insert(11, Ship::new(11, 0, stats(), pilot(11), Vec3::new(40.0 * AU, 0.0, 0.0), Vec3::ZERO));
        let probe = |x: f64| Probe { owner: 1, type_id: 9994, system: 30_000_001, pos: Vec3::new(x, 0.0, 0.0), until_ms: i64::MAX, range: 3.0, group_count: 3, deviation: (1.0e9, 5.0e9) };
        let probes = vec![probe(0.0), probe(AU), probe(-AU)];
        let found = analyse(&park, &probes, 25, true, 99, 1);
        assert_eq!(found.iter().map(|f| f.id).collect::<Vec<_>>(), vec![10]);
        assert!((found[0].pos - Vec3::new(0.5 * AU, 0.0, 0.0)).len() <= 5.0e9 + 1.0);
        assert!(found[0].signal > 0.0);
    }
}
