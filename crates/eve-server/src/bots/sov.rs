//! Null-sec sovereignty for bot alliances: they hold space as players do,
//! with control towers that claim it (svc::starbase::sovereignty decides).
//!
//! - Settling (`establish`, once): each null-sec alliance whose home region
//!   no NPC faction holds anchors a large control tower of its race at a moon
//!   in each of its systems, a block spreading out from its home, one system
//!   for about every 25 pilots. Their claims are dated back past the claim
//!   delay, so the map shows the space held from the start.
//! - Upkeep (`upkeep`, every politics tick): towers are kept in 30 days of
//!   fuel and some strontium. Neighbours at war plan sieges of each other's
//!   border systems, announced in the news `SIEGE_NOTICE` ahead
//!   (`battle` fights them). A siege that succeeds (`conclude`) destroys the
//!   defender's towers there and anchors the attacker's, whose claim then
//!   stands its delay (scaled by the bots' clock) before the system changes
//!   hands. An alliance short of systems at home anchors in unclaimed ones.
//!
//! Bot towers are real: players can see them, shoot them and reinforce them.

use std::collections::{HashMap, HashSet, VecDeque};

use tracing::info;

use super::behaviour::Atlas;
use super::politics::{Politics, KIND_NULL_ALLIANCE};
use crate::combat::Rng;
use crate::store::{Db, Row, RowId, Store, StoreError, Tx, Value};

/// One system for this many pilots, at least `MIN_SYSTEMS`.
const PILOTS_PER_SYSTEM: u32 = 25;
const MIN_SYSTEMS: usize = 3;
/// At most this share of a region's systems goes to its alliance.
const MAX_SHARE: f64 = 0.6;
/// Large control towers by race (Caldari, Minmatar, Amarr, Gallente).
const TOWERS: [(i64, i32); 4] = [(1, 16213), (2, 16214), (4, 12235), (8, 12236)];
const GROUP_MOON: i32 = 8;
/// A tower stands this far off its moon's surface (the moon rule is 100 km).
const OFF_MOON: f64 = 30_000.0;
/// Fuel held: 30 days of hourly periods, topped up below a quarter of that.
const FUEL_PERIODS: i64 = 30 * 24;
const STRONTIUM: i64 = 3_000;
const PURPOSE_ONLINE: i32 = 1;
const PURPOSE_REINFORCE: i32 = 4;
/// Each war's chance, per politics tick, of a siege.
const SIEGE_CHANCE: f64 = 0.03;
/// How long before a siege it is announced, in seconds of the bots' clock.
pub const SIEGE_NOTICE: u32 = 15 * 60;
/// An alliance's chance, per tick, to anchor in an unclaimed home system.
const EXPAND_CHANCE: f64 = 0.05;
const SECOND_FT: i64 = 10_000_000;
/// What an alliance pays its executor's wallet: to anchor a tower, and to
/// keep each in fuel (each politics tick, about 5M ISK a day). An alliance
/// that cannot pay lets its towers run dry.
pub const TOWER_COST: f64 = 150_000_000.0;
const UPKEEP_PER_TICK: f64 = 35_000.0;
/// Treasury at which an alliance fields twice the ships its pilots alone would.
const RICH: f64 = 5_000_000_000.0;

/// What happened, for the news.
pub type News = (i64, String);

/// A siege planned or under way: who, where, and when it begins.
pub struct Siege {
    pub war: i32,
    pub attacker: i32,
    pub defender: i32,
    /// The attacker's executor corporation and its race, for its tower.
    pub attacker_exec: i32,
    pub attacker_race: i64,
    /// Pilots in each alliance, for the odds when nobody watches.
    pub attackers: u32,
    pub defenders: u32,
    pub system: i32,
    pub names: SiegeNames,
    /// When it begins: on the bots' clock, and as FILETIME for the dashboard.
    pub starts: u32,
    pub starts_at: i64,
    /// The fleets, once it is fought where someone watches.
    pub fight: Option<super::battle::Fight>,
}

#[derive(Debug, Clone)]
pub struct SiegeNames {
    pub attacker: String,
    pub defender: String,
    pub system: String,
    pub region: String,
    pub security: f64,
}

struct Place {
    system: i32,
    moon: i32,
    pos: (f64, f64, f64),
}

/// The bot alliances' null-sec alliances, with their home region (only those
/// no NPC faction holds), executor corporation, race and pilots.
fn claimants(db: &Db, p: &Politics) -> Vec<(i32, i32, i32, i64, u32)> {
    let mut out = Vec::new();
    for (id, a) in p.alliances.iter().filter(|(_, a)| a.kind == KIND_NULL_ALLIANCE) {
        let Some(region) = a.region else { continue };
        if !crate::sde::table("mapRegions").get("regionID", i64::from(region)).is_some_and(|r| r.int("factionID").is_none()) {
            continue;
        }
        let Some(executor) = db.table("alliances").get_id(i64::from(*id)).and_then(|al| al.int("executorCorpID")) else { continue };
        let Some(co) = db.table("corporation").get_id(executor) else { continue };
        let pilots: u32 = p.corps.values().filter(|c| c.alliance == Some(*id)).map(|c| c.members).sum();
        out.push((*id, region, executor as i32, co.int("raceID").unwrap_or(4), pilots));
    }
    out.sort_unstable();
    out
}

/// A moon in the system to put a tower beside, and where.
fn spot(db: &Db, system: i32, rng: &mut Rng) -> Option<Place> {
    let mut moons: Vec<Row> = db
        .table("mapDenormalize")
        .find(&["solarSystemID"], &[Value::Int(i64::from(system))])
        .filter(|r| r.int("groupID") == Some(i64::from(GROUP_MOON)))
        .collect();
    moons.sort_by_key(|r| r.int("itemID"));
    let m = moons.get(rng.below(moons.len().max(1)))?;
    let dir = rng.in_sphere(1.0);
    let dir = if dir.len() > 1e-6 { dir.unit() } else { crate::ballpark::Vec3::new(1.0, 0.0, 0.0) };
    let reach = m.float("radius").unwrap_or(1_000.0) + OFF_MOON;
    let at = (m.float("x").unwrap_or(0.0) + dir.x * reach, m.float("y").unwrap_or(0.0) + dir.y * reach, m.float("z").unwrap_or(0.0) + dir.z * reach);
    Some(Place { system, moon: m.int("itemID").unwrap_or(0) as i32, pos: at })
}

/// Anchors and onlines a claiming tower; its claim counts from `claim_since`.
fn anchor(tx: &mut Tx<'_>, owner: i32, tower_type: i32, place: &Place, claim_since: i64) -> Result<i64, StoreError> {
    let now = crate::bulkdata::filetime_now();
    let id = tx.nextval("dyn_id_seq")?;
    tx.insert(
        "entity",
        &[
            ("itemID", Value::Int(id)),
            ("itemName", Value::Text("".into())),
            ("typeID", Value::Int(i64::from(tower_type))),
            ("ownerID", Value::Int(i64::from(owner))),
            ("locationID", Value::Int(i64::from(place.system))),
            ("flag", Value::Int(0)),
            ("contraband", Value::Int(0)),
            ("singleton", Value::Int(1)),
            ("quantity", Value::Int(1)),
            ("x", Value::Float(place.pos.0)),
            ("y", Value::Float(place.pos.1)),
            ("z", Value::Float(place.pos.2)),
        ],
    )?;
    tx.insert(
        "posStructures",
        &[
            ("itemID", Value::Int(id)),
            ("state", Value::Int(4)),
            ("stateTimestamp", Value::Int(now)),
            ("moonID", Value::Int(i64::from(place.moon))),
            ("lastFuel", Value::Int(now)),
            ("claim", Value::Int(1)),
            ("claimSince", Value::Int(claim_since)),
            ("allowCorp", Value::Int(1)),
            ("allowAlliance", Value::Int(1)),
        ],
    )?;
    Ok(id)
}

/// Takes `amount` out of a corporation's wallet if it holds that much;
/// whether it did.
fn pay(tx: &mut Tx<'_>, corp: i32, amount: f64) -> Result<bool, StoreError> {
    let Some(co) = tx.table("corporation").get_id(i64::from(corp)) else { return Ok(false) };
    let (row, balance) = (co.id, co.float("balance").unwrap_or(0.0));
    if balance < amount {
        return Ok(false);
    }
    tx.update("corporation", row, &[("balance", Value::Float(balance - amount))])?;
    Ok(true)
}

/// Settles null-sec: towers for every null-sec bot alliance with none yet.
pub async fn establish(store: &Store, atlas: &Atlas, p: &Politics, rng: &mut Rng, claim_delay_secs: i64) -> Result<Vec<News>, StoreError> {
    let (have, taken, claimants) = store.read(|db| {
        let (entity, corps, groups) = (db.table("entity"), db.table("corporation"), db.table("botGroups"));
        let have: HashSet<i64> = db
            .table("posStructures")
            .rows()
            .filter_map(|ps| entity.get_id(ps.int("itemID")?))
            .filter_map(|e| corps.get_id(e.int("ownerID")?))
            .filter(|co| co.int("corporationID").is_some_and(|c| groups.get_id(c).is_some()))
            .filter_map(|co| co.int("allianceID"))
            .collect();
        let taken: HashSet<i32> =
            db.table("mapSolarSystems").rows().filter(|r| r.int("allianceID").is_some()).filter_map(|r| r.int("solarSystemID").map(|v| v as i32)).collect();
        (have, taken, claimants(db, p))
    });
    let now = crate::bulkdata::filetime_now();
    let settled = now - (claim_delay_secs + 60) * SECOND_FT;
    let mut claimed: HashSet<i32> = taken;
    let mut towers = 0;
    let mut news = Vec::new();
    for (alliance, region, executor, race, pilots) in claimants {
        if have.contains(&(alliance as i64)) {
            continue;
        }
        let in_region: Vec<i32> = atlas.systems.iter().filter(|(_, s)| s.region == region && s.security <= 0.0).map(|(id, _)| *id).collect();
        let want = ((pilots / PILOTS_PER_SYSTEM) as usize).max(MIN_SYSTEMS).min((in_region.len() as f64 * MAX_SHARE) as usize).max(1);
        let tower_type = TOWERS.iter().find(|(r, _)| *r == race).map_or(TOWERS[2].1, |(_, t)| *t);
        // Outward from home: the region's station systems first, then by gates.
        let home = in_region.iter().copied().find(|s| atlas.systems.get(s).is_some_and(|x| !x.stations.is_empty())).or_else(|| in_region.first().copied());
        let Some(home) = home else { continue };
        let mut order: Vec<i32> = Vec::new();
        let mut seen = HashSet::from([home]);
        let mut queue = VecDeque::from([home]);
        while let Some(s) = queue.pop_front() {
            order.push(s);
            for n in atlas.systems.get(&s).map(|x| x.neighbours.clone()).unwrap_or_default() {
                if atlas.systems.get(&n).is_some_and(|x| x.region == region && x.security <= 0.0) && seen.insert(n) {
                    queue.push_back(n);
                }
            }
        }
        let mut held = 0;
        for system in order {
            if held >= want {
                break;
            }
            if claimed.contains(&system) {
                continue;
            }
            let Some(place) = store.read(|db| spot(db, system, rng)) else { continue };
            store.write(|tx| anchor(tx, executor, tower_type, &place, settled))?;
            claimed.insert(system);
            held += 1;
            towers += 1;
        }
        let name = store.read(|db| alliance_name(db, alliance));
        news.push((now, format!("{name} claims {held} systems of {}", region_name(region))));
    }
    if towers > 0 {
        refuel(store).await?;
        info!("bots: {towers} control towers anchored across null-sec");
    }
    Ok(news)
}

/// Keeps every bot tower in fuel: a top-up whenever one runs below a quarter
/// of 30 days' worth.
pub async fn refuel(store: &Store) -> Result<u64, StoreError> {
    // What each tower type burns, from the static data: (fuel, how much to
    // hold).
    let mut burns: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    for r in crate::sde::table("invControlTowerResources").rows() {
        let purpose = r.int("purpose");
        if !(purpose == Some(i64::from(PURPOSE_ONLINE)) || purpose == Some(i64::from(PURPOSE_REINFORCE)))
            || r.float("minSecurityLevel").is_some()
            || r.int("factionID").is_some()
        {
            continue;
        }
        let (Some(tower), Some(kind)) = (r.int("controlTowerTypeID"), r.int("resourceTypeID")) else { continue };
        let target = if purpose == Some(i64::from(PURPOSE_REINFORCE)) {
            STRONTIUM
        } else {
            let Some(per_hour) = r.int("quantity") else { continue };
            per_hour * FUEL_PERIODS
        };
        burns.entry(tower).or_default().push((kind, target));
    }
    store.write(|tx| {
        // (tower, its owner, fuel, how much to add)
        let needs: Vec<(i64, i64, i64, i64)> = {
            let db = tx.db();
            let (entity, corps, groups) = (db.table("entity"), db.table("corporation"), db.table("botGroups"));
            let mut needs = Vec::new();
            for ps in db.table("posStructures").rows() {
                if ps.int("controlTowerID").is_some() || !matches!(ps.int("state"), Some(2..=5)) {
                    continue;
                }
                let Some(e) = ps.int("itemID").and_then(|id| entity.get_id(id)) else { continue };
                let (Some(tower), Some(type_id), Some(owner)) = (e.int("itemID"), e.int("typeID"), e.int("ownerID")) else { continue };
                if groups.get_id(owner).is_none() || !corps.get_id(owner).is_some_and(|co| co.float("balance").unwrap_or(0.0) > 0.0) {
                    continue;
                }
                let Some(list) = burns.get(&type_id) else { continue };
                let mut held: HashMap<i64, i64> = HashMap::new();
                for item in entity.find(&["locationID"], &[Value::Int(tower)]) {
                    *held.entry(item.int("typeID").unwrap_or(0)).or_default() += item.int("quantity").unwrap_or(0);
                }
                for (kind, target) in list {
                    let have = held.get(kind).copied().unwrap_or(0);
                    if have * 4 < *target {
                        needs.push((tower, owner, *kind, target - have));
                    }
                }
            }
            needs
        };
        for (tower, owner, kind, quantity) in &needs {
            let id = tx.nextval("dyn_id_seq")?;
            tx.insert(
                "entity",
                &[
                    ("itemID", Value::Int(id)),
                    ("itemName", Value::Text("".into())),
                    ("typeID", Value::Int(*kind)),
                    ("ownerID", Value::Int(*owner)),
                    ("locationID", Value::Int(*tower)),
                    ("flag", Value::Int(0)),
                    ("contraband", Value::Int(0)),
                    ("singleton", Value::Int(0)),
                    ("quantity", Value::Int(*quantity)),
                ],
            )?;
        }
        Ok(needs.len() as u64)
    })
}

/// Every politics tick: fuel, sieges planned between warring neighbours
/// (added to `sieges`), and expansion into empty home systems. Returns the
/// news.
#[allow(clippy::too_many_arguments)]
pub async fn upkeep(
    store: &Store,
    atlas: &Atlas,
    p: &mut Politics,
    watched: &HashSet<i32>,
    sieges: &mut Vec<Siege>,
    rng: &mut Rng,
    clock: u32,
    claim_delay_secs: i64,
    speed: f64,
) -> Result<Vec<News>, StoreError> {
    // Fuel costs money: each bot corporation pays for its towers.
    store.write(|tx| -> Result<(), StoreError> {
        let mut owned: HashMap<i64, f64> = HashMap::new();
        {
            let db = tx.db();
            let (entity, groups) = (db.table("entity"), db.table("botGroups"));
            for ps in db.table("posStructures").rows().filter(|ps| ps.int("controlTowerID").is_none()) {
                let Some(owner) = ps.int("itemID").and_then(|id| entity.get_id(id)).and_then(|e| e.int("ownerID")) else { continue };
                if groups.get_id(owner).is_some() {
                    *owned.entry(owner).or_default() += 1.0;
                }
            }
        }
        for (owner, n) in owned {
            let Some(co) = tx.table("corporation").get_id(owner) else { continue };
            let (row, balance) = (co.id, co.float("balance").unwrap_or(0.0));
            tx.update("corporation", row, &[("balance", Value::Float(balance - n * UPKEEP_PER_TICK))])?;
        }
        Ok(())
    })?;
    refuel(store).await?;
    let now = crate::bulkdata::filetime_now();
    // A new claim waits out the delay, run at the bots' speed.
    let delay = claim_delay_secs as f64;
    let claim_since = now - ((delay - delay / speed.max(1.0)) as i64) * SECOND_FT;
    let mut news = Vec::new();
    // Which alliances have claiming towers where: what sieges aim at, rather
    // than the map's sovereignty, which follows the towers only later.
    let (claimants, mut towers) = store.read(|db| {
        let (entity, corps) = (db.table("entity"), db.table("corporation"));
        let mut towers: HashMap<i32, HashSet<i32>> = HashMap::new();
        for ps in db.table("posStructures").rows().filter(|ps| ps.int("claim") == Some(1)) {
            let Some(e) = ps.int("itemID").and_then(|id| entity.get_id(id)) else { continue };
            let alliance = e.int("ownerID").and_then(|o| corps.get_id(o)).and_then(|co| co.int("allianceID")).unwrap_or(0);
            if let Some(system) = e.int("locationID") {
                towers.entry(system as i32).or_default().insert(alliance as i32);
            }
        }
        (claimants(db, p), towers)
    });
    let info: HashMap<i32, (i32, i32, i64, u32)> = claimants.iter().map(|(a, r, e, race, n)| (*a, (*r, *e, *race, *n))).collect();
    let has = |towers: &HashMap<i32, HashSet<i32>>, system: &i32, alliance: i32| towers.get(system).is_some_and(|t| t.contains(&alliance));

    // Sieges: one at a time in each war, announced ahead.
    let wars: Vec<(i32, i32, i32)> = p.wars.iter().map(|w| (w.a, w.b, w.id)).collect();
    for (a, b, war) in wars {
        let (Some(&(_, attacker_exec, attacker_race, attackers)), Some(&(_, _, _, defenders))) = (info.get(&a), info.get(&b)) else { continue };
        if sieges.iter().any(|s| s.war == war) || rng.unit() >= SIEGE_CHANCE || !p.in_prime(a) {
            continue;
        }
        // Rich alliances field more ships than their numbers alone would.
        let wealth = |side: i32| 1.0 + (p.treasury.get(&side).copied().unwrap_or(0.0) / RICH).clamp(0.0, 1.0);
        let (attackers, defenders) = ((attackers as f64 * wealth(a)) as u32, (defenders as f64 * wealth(b)) as u32);
        // A defender's system next to the attacker's space, else any of theirs.
        let mut theirs: Vec<i32> = towers.keys().copied().filter(|s| has(&towers, s, b) && !sieges.iter().any(|x| x.system == *s)).collect();
        theirs.sort_unstable();
        let border: Vec<i32> = theirs
            .iter()
            .copied()
            .filter(|s| atlas.systems.get(s).is_some_and(|x| x.neighbours.iter().any(|nb| has(&towers, nb, a))))
            .collect();
        let pool = if border.is_empty() { &theirs } else { &border };
        let Some(&target) = pool.get(rng.below(pool.len().max(1))) else { continue };
        let names = store.read(|db| SiegeNames {
            attacker: alliance_name(db, a),
            defender: alliance_name(db, b),
            system: system_name(db, target),
            region: region_name(atlas.systems.get(&target).map_or(0, |x| x.region)),
            security: atlas.systems.get(&target).map_or(0.0, |x| x.security as f64),
        });
        let starts_at = now + (SIEGE_NOTICE as f64 / speed.max(0.001)) as i64 * SECOND_FT;
        info!("bots: {} will besiege {} ({}) of {}", names.attacker, names.system, names.region, names.defender);
        news.push((now, format!("{} are massing a fleet to besiege {} in {}, held by {}", names.attacker, names.system, names.region, names.defender)));
        sieges.push(Siege {
            war,
            attacker: a,
            defender: b,
            attacker_exec,
            attacker_race,
            attackers,
            defenders,
            system: target,
            names,
            starts: clock + SIEGE_NOTICE,
            starts_at,
            fight: None,
        });
    }

    // Expansion into empty home systems.
    for (alliance, region, executor, race, pilots) in &claimants {
        if rng.unit() >= EXPAND_CHANCE {
            continue;
        }
        let size = atlas.systems.values().filter(|s| s.region == *region && s.security <= 0.0).count();
        let want = ((*pilots / PILOTS_PER_SYSTEM) as usize).max(MIN_SYSTEMS).min((size as f64 * MAX_SHARE) as usize);
        let have = towers.values().filter(|t| t.contains(alliance)).count();
        if have >= want {
            continue;
        }
        let empty: Vec<i32> = atlas
            .systems
            .iter()
            .filter(|(id, s)| s.region == *region && s.security <= 0.0 && towers.get(id).is_none_or(|t| t.is_empty()) && !watched.contains(id))
            .map(|(id, _)| *id)
            .collect();
        let Some(&system) = empty.get(rng.below(empty.len().max(1))) else { continue };
        let tower_type = TOWERS.iter().find(|(r, _)| r == race).map_or(TOWERS[2].1, |(_, t)| *t);
        // A tower is bought out of the executor's wallet, or not at all.
        if !store.write(|tx| pay(tx, *executor, TOWER_COST))? {
            continue;
        }
        if let Some(place) = store.read(|db| spot(db, system, rng)) {
            store.write(|tx| anchor(tx, *executor, tower_type, &place, claim_since))?;
            towers.entry(system).or_default().insert(*alliance);
            let (who, place) = store.read(|db| (alliance_name(db, *alliance), system_name(db, system)));
            news.push((now, format!("{who} anchored a control tower in {place}")));
        }
    }
    Ok(news)
}

/// How a siege ended: `won` by the attacker or not, with `lost` ships on
/// each side. A won siege destroys the defender's towers in the system and
/// anchors the attacker's, its claim to stand the delay (scaled by the bots'
/// clock). Returns the news.
#[allow(clippy::too_many_arguments)]
pub async fn conclude(
    store: &Store,
    p: &mut Politics,
    s: &Siege,
    won: bool,
    lost: (u32, u32),
    rng: &mut Rng,
    claim_delay_secs: i64,
    speed: f64,
) -> Result<News, StoreError> {
    let now = crate::bulkdata::filetime_now();
    if let Some(w) = p.wars.iter_mut().find(|w| w.id == s.war) {
        w.kills += lost.0 + lost.1;
    }
    p.lost += (lost.0 + lost.1) as u64;
    let n = &s.names;
    if !won {
        info!("bots: {} driven back from {} by {} ({} and {} ships lost)", n.attacker, n.system, n.defender, lost.0, lost.1);
        return Ok((now, format!("{} were driven back from {} by {}", n.attacker, n.system, n.defender)));
    }
    let gone = destroy_towers(store, s.system, s.defender).await?;
    // The victors buy their tower with what is left in their executor's wallet.
    store.write(|tx| -> Result<(), StoreError> {
        if let Some(co) = tx.table("corporation").get_id(i64::from(s.attacker_exec)) {
            let (row, balance) = (co.id, co.float("balance").unwrap_or(0.0));
            tx.update("corporation", row, &[("balance", Value::Float(balance - balance.max(0.0).min(TOWER_COST)))])?;
        }
        Ok(())
    })?;
    let delay = claim_delay_secs as f64;
    let claim_since = now - ((delay - delay / speed.max(1.0)) as i64) * SECOND_FT;
    let tower_type = TOWERS.iter().find(|(r, _)| *r == s.attacker_race).map_or(TOWERS[2].1, |(_, t)| *t);
    if let Some(place) = store.read(|db| spot(db, s.system, rng)) {
        store.write(|tx| anchor(tx, s.attacker_exec, tower_type, &place, claim_since))?;
        refuel(store).await?;
    }
    info!("bots: {} besieged {}: {gone} tower(s) of {} destroyed", n.attacker, n.system, n.defender);
    Ok((now, format!("{} destroyed {}'s towers in {} and anchored their own", n.attacker, n.defender, n.system)))
}

/// Brings a system's park up to date with its towers after a siege: the
/// destroyed ones explode, the new one appears.
pub async fn show_towers(store: &Store, world: &crate::world::World, system: i32) {
    let loaded = match store.read(|db| crate::pos::load(db, system as i64)) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("bots: reading the towers of {system}: {}", crate::logging::chain(&e));
            return;
        }
    };
    let mut w = world.lock();
    let Some(park) = w.parks.get_mut(&(system as i64)) else { return };
    park.advance(crate::destiny::tick_now());
    let tick = park.tick;
    let mut entries = Vec::new();
    let gone: Vec<i64> = park.structures.keys().copied().filter(|id| !loaded.iter().any(|s| s.id == *id)).collect();
    for id in gone {
        let Some(s) = park.structures.remove(&id) else { continue };
        entries.push(crate::svc::space::entry(tick, "TerminalExplosion", vec![eve_marshal::Value::Int(id)]));
        let mut balls = vec![eve_marshal::Value::Int(id)];
        if let Some(f) = &s.field {
            balls.push(eve_marshal::Value::Int(f.id));
        }
        entries.push(crate::svc::space::entry(tick, "RemoveBalls", vec![eve_marshal::Value::List(balls)]));
    }
    for s in loaded {
        if !park.structures.contains_key(&s.id) {
            // Its force field goes up with the next pulse (`pos::step`).
            entries.push(crate::pos::add_entry(tick, &s));
            park.structures.insert(s.id, s);
        }
    }
    crate::svc::space::broadcast(&w, system as i64, None, entries);
}

/// Destroys an alliance's towers (and what they hold) in a system.
async fn destroy_towers(store: &Store, system: i32, alliance: i32) -> Result<u64, StoreError> {
    store.write(|tx| {
        let towers: Vec<i64> = {
            let db = tx.db();
            let (structures, corps) = (db.table("posStructures"), db.table("corporation"));
            db.table("entity")
                .find(&["locationID"], &[Value::Int(i64::from(system))])
                .filter(|e| e.int("itemID").is_some_and(|id| structures.get_id(id).is_some()))
                .filter(|e| e.int("ownerID").and_then(|o| corps.get_id(o)).is_some_and(|co| co.int("allianceID") == Some(i64::from(alliance))))
                .filter_map(|e| e.int("itemID"))
                .collect()
        };
        if towers.is_empty() {
            return Ok(0);
        }
        for tower in &towers {
            tx.delete_where("entity", &["locationID"], &[Value::Int(*tower)])?;
        }
        let structures: Vec<RowId> = tx
            .table("posStructures")
            .rows()
            .filter(|ps| [ps.int("itemID"), ps.int("controlTowerID")].iter().any(|id| id.is_some_and(|id| towers.contains(&id))))
            .map(|ps| ps.id)
            .collect();
        for row in structures {
            tx.delete("posStructures", row)?;
        }
        let mut gone = 0;
        for tower in &towers {
            gone += tx.delete_where("entity", &["itemID"], &[Value::Int(*tower)])? as u64;
        }
        Ok(gone)
    })
}

pub fn alliance_name(db: &Db, id: i32) -> String {
    db.table("alliances").get_id(i64::from(id)).and_then(|r| r.text("allianceName").map(String::from)).unwrap_or_else(|| id.to_string())
}

pub fn system_name(db: &Db, id: i32) -> String {
    db.table("mapSolarSystems").get_id(i64::from(id)).and_then(|r| r.text("solarSystemName").map(String::from)).unwrap_or_else(|| id.to_string())
}

fn region_name(id: i32) -> String {
    crate::sde::table("mapRegions").get("regionID", i64::from(id)).and_then(|r| r.text("regionName")).map_or_else(|| id.to_string(), String::from)
}
