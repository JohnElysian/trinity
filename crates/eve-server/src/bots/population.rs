//! Making the bots and reading them back: the map they live on, and the
//! characters themselves.
//!
//! A bot is a real character on the account rmr-bots: named from the NPC
//! characters' first and last names of its race, with the face of an NPC of
//! its bloodline and gender, in its bloodline's school corporation, with a
//! ship of its race and role in its home station's hangar. Its skills live
//! only in the simulation. It is not a static owner: the client learns its
//! name when it needs it, as for any player (config.StaticOwners goes whole
//! to every client at login, and has a 1 MB ceiling). They are made in batches, each in one
//! write.

use std::collections::{HashMap, HashSet};

use tracing::info;

use super::behaviour::{Activity, Atlas, Bot, Role, System};
use super::presence::Identity;
use crate::combat::Rng;
use crate::store::{Db, Row, Store, StoreError, Tx, Value};

/// The share of bots in each role.
pub const ROLES: [(Role, f64); 6] = [
    (Role::Miner, 0.30),
    (Role::Trader, 0.15),
    (Role::Hauler, 0.15),
    (Role::Ratter, 0.20),
    (Role::Explorer, 0.10),
    (Role::Pirate, 0.10),
];

/// Jita 4-4, Amarr VIII, Dodixie IX-20, Rens VI-8, Hek VIII-12.
const HUBS: [i32; 5] = [60003760, 60008494, 60011866, 60004588, 60005686];
const BOT_ACCOUNT: &str = "rmr-bots";
const BATCH: usize = 5_000;
const GROUP_BELT: i32 = 9;
const HIGH_SEC: f32 = 0.45;

// ---- the map -------------------------------------------------------------------------------

pub fn atlas(db: &Db) -> Result<Atlas, String> {
    let graph = crate::gates::graph(db).ok_or("the star gates could not be read")?;
    let mut systems: HashMap<i32, System> = HashMap::new();
    for r in db.table("mapSolarSystems").rows() {
        let id = r.int("solarSystemID").unwrap_or(0) as i32;
        let neighbours = graph.get(&(id as i64)).map(|n| n.iter().map(|s| *s as i32).collect()).unwrap_or_default();
        let system = System {
            security: r.float("security").unwrap_or(0.0) as f32,
            region: r.int("regionID").unwrap_or(0) as i32,
            stations: Vec::new(),
            belts: 0,
            neighbours,
        };
        systems.insert(id, system);
    }
    let mut station_system = HashMap::new();
    let mut stations: Vec<(i32, i32)> =
        db.table("staStations").rows().map(|r| (r.int("stationID").unwrap_or(0) as i32, r.int("solarSystemID").unwrap_or(0) as i32)).collect();
    stations.sort_unstable_by_key(|(station, _)| *station);
    for (station, system) in stations {
        if let Some(s) = systems.get_mut(&system) {
            s.stations.push(station);
            station_system.insert(station, system);
        }
    }
    let mut belts: HashMap<i32, i64> = HashMap::new();
    for r in db.table("mapDenormalize").rows().filter(|r| r.int("groupID") == Some(i64::from(GROUP_BELT))) {
        *belts.entry(r.int("solarSystemID").unwrap_or(0) as i32).or_default() += 1;
    }
    for (system, n) in belts {
        if let Some(s) = systems.get_mut(&system) {
            s.belts = n.min(u16::MAX as i64) as u16;
        }
    }
    let belts_near = systems
        .keys()
        .filter_map(|&id| {
            crate::gates::reach_on(graph, id as i64, 3)
                .into_iter()
                .find(|(s, _)| systems.get(&(*s as i32)).is_some_and(|x| x.belts > 0))
                .map(|(s, _)| (id, s as i32))
        })
        .collect();
    let hubs = HUBS.iter().copied().filter(|h| station_system.contains_key(h)).collect();
    Ok(Atlas { systems, station_system, hubs, belts_near, graph })
}

// ---- making them ---------------------------------------------------------------------------

/// What generating needs from the database, read once.
struct Sources {
    /// First and last names by race.
    names: HashMap<i64, (Vec<String>, Vec<String>)>,
    taken: HashSet<String>,
    /// NPC characters to take faces from, by (bloodline, gender).
    faces: HashMap<(i64, i16), Vec<i32>>,
    /// Each player bloodline's race.
    bloodlines: Vec<(i64, i64)>,
    /// Hulls by (group, race).
    hulls: HashMap<(i32, i64), Vec<i32>>,
}

fn sources(db: &Db) -> Sources {
    let chr_bloodlines = crate::sde::table("chrBloodlines");
    let characters = db.table("character_");
    let mut names: HashMap<i64, (Vec<String>, Vec<String>)> = HashMap::new();
    for r in characters.rows().filter(|c| c.int("accountID").is_none() && c.text("characterName").is_some_and(|n| n.contains(' '))) {
        // An NPC whose bloodline is not in chrBloodlines lends no names.
        let Some(b) = r.int("bloodlineID").and_then(|b| chr_bloodlines.get("bloodlineID", b)) else { continue };
        let (race, name) = (b.int("raceID").unwrap_or(0), r.text("characterName").unwrap_or_default());
        let (Some(first), Some(last)) = (name.split(' ').next(), name.rsplit(' ').next()) else { continue };
        let entry = names.entry(race).or_default();
        entry.0.push(first.to_string());
        entry.1.push(last.to_string());
    }
    for (firsts, lasts) in names.values_mut() {
        firsts.sort();
        firsts.dedup();
        lasts.sort();
        lasts.dedup();
    }
    let taken = characters.rows().filter_map(|r| r.text("characterName").map(str::to_lowercase)).collect();
    let mut faces: HashMap<(i64, i16), Vec<i32>> = HashMap::new();
    for r in characters.rows().filter(|c| c.int("accountID").is_none() && c.int("bloodlineID").is_some_and(|b| (1..=8).contains(&b))) {
        faces.entry((r.int("bloodlineID").unwrap_or(0), r.int("gender").unwrap_or(0) as i16)).or_default().push(r.int("characterID").unwrap_or(0) as i32);
    }
    let mut bloodlines: Vec<(i64, i64)> = chr_bloodlines
        .rows()
        .filter_map(|r| Some((r.int("bloodlineID").filter(|b| (1..=8).contains(b))?, r.int("raceID").unwrap_or(0))))
        .collect();
    bloodlines.sort_by_key(|(b, _)| *b);
    // Marketable hulls of a race in the bots' ship groups, by type.
    let mut hulls: HashMap<(i32, i64), Vec<i32>> = HashMap::new();
    let mut types: Vec<_> = [25i64, 26, 27, 28, 31, 419, 463]
        .into_iter()
        .flat_map(|g| crate::sde::table("invTypes").find("groupID", g))
        .filter(|t| t.int("marketGroupID").is_some() && t.int("raceID").is_some())
        .collect();
    types.sort_by_key(|t| t.int("typeID"));
    for t in types {
        let (Some(group), Some(race), Some(id)) = (t.int("groupID"), t.int("raceID"), t.int("typeID")) else { continue };
        hulls.entry((group as i32, race)).or_default().push(id as i32);
    }
    Sources { names, taken, faces, bloodlines, hulls }
}

/// How likely a role is to live at a station, by where it is.
fn home_weight(role: Role, atlas: &Atlas, station: i32) -> f64 {
    let Some(system) = atlas.station_system.get(&station).and_then(|s| atlas.systems.get(s)) else { return 0.0 };
    let (high, low) = (system.security >= HIGH_SEC, system.security > 0.0 && system.security < HIGH_SEC);
    let hub = atlas.hubs.contains(&station);
    let hub_system = atlas.hubs.iter().any(|h| atlas.station_system.get(h) == atlas.station_system.get(&station));
    match role {
        Role::Miner if system.belts == 0 => 0.05,
        Role::Miner => if high { 1.0 } else if low { 0.25 } else { 0.1 },
        Role::Trader if hub => 400.0,
        Role::Trader if hub_system => 20.0,
        Role::Trader => if high { 0.3 } else { 0.02 },
        Role::Hauler => if hub_system { 10.0 } else if high { 1.0 } else if low { 0.2 } else { 0.05 },
        Role::Ratter => if high { 0.1 } else if low { 1.0 } else { 1.5 },
        Role::Explorer => 1.0,
        Role::Pirate => if low { 1.0 } else if high { 0.0 } else { 0.3 },
    }
}

/// Weighted picking, by cumulative weight.
struct Pick {
    items: Vec<i32>,
    cumulative: Vec<f64>,
}

impl Pick {
    fn new(items: impl IntoIterator<Item = (i32, f64)>) -> Pick {
        let (mut out, mut cumulative, mut total) = (Vec::new(), Vec::new(), 0.0);
        for (item, w) in items.into_iter().filter(|(_, w)| *w > 0.0) {
            total += w;
            out.push(item);
            cumulative.push(total);
        }
        Pick { items: out, cumulative }
    }

    fn pick(&self, rng: &mut Rng) -> i32 {
        let x = rng.unit() * self.cumulative.last().copied().unwrap_or(0.0);
        let i = self.cumulative.partition_point(|c| *c < x).min(self.items.len().saturating_sub(1));
        self.items[i]
    }
}

/// The hull a bot of a role and race flies.
fn hull(role: Role, race: i64, s: &Sources, rng: &mut Rng) -> i32 {
    let groups: &[i32] = match role {
        Role::Miner => &[25, 463],
        Role::Hauler => &[28],
        Role::Trader => &[31, 28],
        Role::Ratter => &[26, 419, 27],
        Role::Explorer => &[25],
        Role::Pirate => &[25, 26],
    };
    let group = groups[rng.below(groups.len())];
    let list = s.hulls.get(&(group, race)).or_else(|| s.hulls.get(&(25, race))).or_else(|| s.hulls.values().next());
    list.map_or(0, |l| l[rng.below(l.len())])
}

fn name(race: i64, s: &mut Sources, rng: &mut Rng) -> Option<String> {
    let races: Vec<i64> = s.names.keys().copied().collect();
    for attempt in 0..40 {
        let r = if attempt < 30 { race } else { races[rng.below(races.len())] };
        let (firsts, lasts) = s.names.get(&r)?;
        let full = format!("{} {}", firsts[rng.below(firsts.len())], lasts[rng.below(lasts.len())]);
        if crate::svc::charcreation::name_shape_ok(&full) && s.taken.insert(full.to_lowercase()) {
            return Some(full);
        }
    }
    None
}

/// Makes bots until there are `target`; returns how many there are.
pub async fn ensure(store: &Store, atlas: &Atlas, target: usize) -> Result<usize, String> {
    let fail = |what: &str, e: StoreError| format!("{what}: {}", crate::logging::chain(&e));
    let have = store.read(|db| db.table("bots").len());
    if have >= target {
        return Ok(have);
    }
    let account = store
        .write(|tx| -> Result<i64, StoreError> {
            let name = Value::Text(BOT_ACCOUNT.into());
            if let Some(found) = tx.table("account").find_one(&["accountName"], std::slice::from_ref(&name)) {
                return Ok(found.int("accountID").unwrap_or(0));
            }
            let row = tx.insert("account", &[("accountName", name), ("password", Value::Text("".into())), ("role", Value::Int(0)), ("banned", Value::Int(1))])?;
            Ok(tx.table("account").row(row).and_then(|r| r.int("accountID")).unwrap_or(0))
        })
        .map_err(|e| fail("the bot account", e))?;
    let mut src = store.read(sources);
    let stations: Vec<i32> = atlas.station_system.keys().copied().collect();
    let homes: Vec<(Role, Pick)> = ROLES.iter().map(|(role, _)| (*role, Pick::new(stations.iter().map(|s| (*s, home_weight(*role, atlas, *s)))))).collect();
    let roles = Pick::new(ROLES.iter().map(|(r, w)| (*r as i32, *w)));
    let mut rng = Rng::seeded(crate::bulkdata::filetime_now() as u64);
    let started = std::time::Instant::now();
    let mut made = 0;
    while have + made < target {
        let n = BATCH.min(target - have - made);
        let mut batch = Vec::with_capacity(n);
        for _ in 0..n {
            let role = Role::from_i16(roles.pick(&mut rng) as i16);
            let (bloodline, race) = src.bloodlines[rng.below(src.bloodlines.len())];
            let gender = rng.below(2) as i16;
            let Some(face) = src.faces.get(&(bloodline, gender)).map(|f| f[rng.below(f.len())]) else { continue };
            let Some(name) = name(race, &mut src, &mut rng) else { continue };
            let home = homes.iter().find(|(r, _)| *r == role).map_or(HUBS[0], |(_, p)| p.pick(&mut rng));
            let ship = hull(role, race, &src, &mut rng);
            let balance = 10f64.powf(rng.range(5.5, 8.5)).round();
            let security = if role == Role::Pirate { -rng.range(2.0, 8.0) } else { rng.range(-0.5, 3.0) };
            let share = rng.range(0.1, 0.5) as f32;
            batch.push(NewBot { name, bloodline: bloodline as i32, gender, face, home, ship, balance, security, role, share });
        }
        store.write(|tx| insert(tx, account, &batch)).map_err(|e| fail("making bots", e))?;
        made += batch.len();
        info!("bots: made {} of {} ({:.0}/s)", have + made, target, made as f64 / started.elapsed().as_secs_f64().max(0.001));
    }
    Ok(have + made)
}

struct NewBot {
    name: String,
    bloodline: i32,
    gender: i16,
    face: i32,
    home: i32,
    ship: i32,
    balance: f64,
    security: f64,
    role: Role,
    share: f32,
}

/// The appearance columns a bot copies from its face.
const LOOKS: [&str; 37] = [
    "accessoryID", "beardID", "costumeID", "decoID", "eyebrowsID", "eyesID", "hairID", "lipstickID", "makeupID", "skinID",
    "backgroundID", "lightID", "headRotation1", "headRotation2", "headRotation3", "eyeRotation1", "eyeRotation2", "eyeRotation3",
    "camPos1", "camPos2", "camPos3", "morph1e", "morph1n", "morph1s", "morph1w", "morph2e", "morph2n", "morph2s", "morph2w",
    "morph3e", "morph3n", "morph3s", "morph3w", "morph4e", "morph4n", "morph4s", "morph4w",
];

/// A batch of bots, in the caller's transaction: their characters (each
/// with the face it copies, its bloodline's school corporation and race,
/// at home), bodies, ships, names, roles and employment, and the bots' rows.
fn insert(tx: &mut Tx<'_>, account: i64, batch: &[NewBot]) -> Result<(), StoreError> {
    let mut ids = Vec::with_capacity(2 * batch.len());
    for _ in 0..2 * batch.len() {
        ids.push(tx.nextval("dyn_id_seq")?);
    }
    let (chars, ships) = ids.split_at(batch.len());
    let now = crate::bulkdata::filetime_now();
    // chrBloodlines: each bloodline's school corporation and race.
    let bloodlines: HashMap<i64, (Value, Value)> = crate::sde::table("chrBloodlines")
        .rows()
        .filter_map(|r| {
            let to = |c: &str| r.int(c).map_or(Value::Null, Value::Int);
            Some((r.int("bloodlineID")?, (to("corporationID"), to("raceID"))))
        })
        .collect();
    let mut joined: HashMap<i64, i64> = HashMap::new();
    for ((b, &id), &ship) in batch.iter().zip(chars).zip(ships) {
        let bloodline = bloodlines.get(&i64::from(b.bloodline));
        let face = tx.table("character_").get_id(i64::from(b.face)).map(|f| (f.value("typeID").clone(), LOOKS.map(|c| f.value(c).clone())));
        let home = tx.table("staStations").get_id(i64::from(b.home)).map(|s| ["stationID", "solarSystemID", "constellationID", "regionID"].map(|c| s.value(c).clone()));
        let name = Value::Text(b.name.as_str().into());
        if let (Some((corp, race)), Some((type_id, looks)), Some(home)) = (bloodline, face, home) {
            let mut row: Vec<(&str, Value)> = vec![
                ("characterID", Value::Int(id)),
                ("characterName", name.clone()),
                ("accountID", Value::Int(account)),
                ("balance", Value::Float(b.balance)),
                ("securityRating", Value::Float(b.security)),
                ("corporationID", corp.clone()),
                ("corporationDateTime", Value::Int(now)),
                ("startDateTime", Value::Int(now)),
                ("createDateTime", Value::Int(now)),
                ("typeID", type_id.clone()),
                ("raceID", race.clone()),
                ("bloodlineID", Value::Int(i64::from(b.bloodline))),
                ("gender", Value::Int(i64::from(b.gender))),
                ("shipID", Value::Int(ship)),
            ];
            row.extend(LOOKS.iter().copied().zip(looks));
            row.extend(["stationID", "solarSystemID", "constellationID", "regionID"].into_iter().zip(home));
            tx.insert("character_", &row)?;
            tx.insert(
                "entity",
                &[
                    ("itemID", Value::Int(id)),
                    ("itemName", name.clone()),
                    ("typeID", type_id.clone()),
                    ("ownerID", Value::Int(id)),
                    ("locationID", Value::Int(i64::from(b.home))),
                    ("flag", Value::Int(0)),
                    ("contraband", Value::Int(0)),
                    ("singleton", Value::Int(1)),
                    ("quantity", Value::Int(1)),
                ],
            )?;
            tx.insert(
                "eveNames",
                &[("itemID", Value::Int(id)), ("itemName", name.clone()), ("categoryID", Value::Int(1)), ("groupID", Value::Int(1)), ("typeID", type_id)],
            )?;
            tx.insert(
                "chrEmployment",
                &[("characterID", Value::Int(id)), ("corporationID", corp.clone()), ("startDate", Value::Int(now)), ("deleted", Value::Int(0))],
            )?;
        }
        // The ship and the roles come whether or not the character could
        // be made, as the set-based inserts made them.
        tx.insert(
            "entity",
            &[
                ("itemID", Value::Int(ship)),
                ("itemName", Value::Text("".into())),
                ("typeID", Value::Int(i64::from(b.ship))),
                ("ownerID", Value::Int(id)),
                ("locationID", Value::Int(i64::from(b.home))),
                ("flag", Value::Int(4)),
                ("contraband", Value::Int(0)),
                ("singleton", Value::Int(1)),
                ("quantity", Value::Int(1)),
            ],
        )?;
        tx.insert(
            "chrCorporationRoles",
            &[
                ("characterID", Value::Int(id)),
                ("corprole", Value::Int(0)),
                ("rolesAtAll", Value::Int(0)),
                ("rolesAtBase", Value::Int(0)),
                ("rolesAtHQ", Value::Int(0)),
                ("rolesAtOther", Value::Int(0)),
            ],
        )?;
        if let Some(corp) = bloodline.and_then(|(corp, _)| corp.int()) {
            *joined.entry(corp).or_default() += 1;
        }
    }
    for (corp, n) in joined {
        let Some(co) = tx.table("corporation").get_id(corp) else { continue };
        let (row, members) = (co.id, co.int("memberCount").unwrap_or(0));
        tx.update("corporation", row, &[("memberCount", Value::Int(members + n))])?;
    }
    for (b, &id) in batch.iter().zip(chars) {
        tx.insert(
            "bots",
            &[
                ("characterID", Value::Int(id)),
                ("role", Value::Int(b.role as i64)),
                ("homeStationID", Value::Int(i64::from(b.home))),
                ("share", Value::Float(f64::from(b.share))),
            ],
        )?;
    }
    Ok(())
}

// ---- reading them back ---------------------------------------------------------------------

/// Up to `limit` bots, each docked where it was (or at home, if it was in
/// space when the server stopped), offline; with who each is.
pub fn load(db: &Db, atlas: &Atlas, limit: usize) -> (Vec<Bot>, Vec<Identity>) {
    let (characters, entity, corps) = (db.table("character_"), db.table("entity"), db.table("corporation"));
    let mut rows: Vec<(Row, Row)> = db.table("bots").rows().filter_map(|b| Some((b, characters.get_id(b.int("characterID")?)?))).collect();
    rows.sort_by_key(|(b, _)| b.int("characterID"));
    rows.truncate(limit);
    let ids = rows
        .iter()
        .map(|(_, c)| Identity {
            name: c.text("characterName").unwrap_or_default().to_string(),
            corp: c.int("corporationID").unwrap_or(0) as i32,
            security: c.float("securityRating").unwrap_or(0.0) as f32,
            ship: c.int("shipID").unwrap_or(0),
            hull: c.int("shipID").and_then(|s| entity.get_id(s)).and_then(|e| e.int("typeID")).unwrap_or(0),
            alliance: c.int("corporationID").and_then(|co| corps.get_id(co)).and_then(|co| co.int("allianceID")).map(|a| a as i32).filter(|a| *a != 0),
        })
        .collect();
    let bots = rows
        .iter()
        .map(|(b, c)| {
            let home = b.int("homeStationID").unwrap_or(0) as i32;
            let station = c.int("stationID").map(|s| s as i32).filter(|s| atlas.station_system.contains_key(s)).unwrap_or(home);
            let hull = c.int("shipID").and_then(|s| entity.get_id(s)).and_then(|e| e.int("typeID")).unwrap_or(0);
            Bot {
                id: b.int("characterID").unwrap_or(0) as i32,
                role: Role::from_i16(b.int("role").unwrap_or(1) as i16),
                home,
                system: atlas.station_system.get(&station).copied().unwrap_or(0),
                station,
                online: false,
                share: b.float("share").unwrap_or(0.3) as f32,
                session_end: 0,
                activity: Activity::Docked,
                route: Vec::new(),
                dock_at: 0,
                hold: Vec::new(),
                isk: c.float("balance").unwrap_or(0.0),
                isk_saved: c.float("balance").unwrap_or(0.0),
                earned: 0.0,
                pod: None,
                turf: b.int("turf").unwrap_or(0) as i32,
                hull: hull as i32,
                tier: b.int("tier").unwrap_or(0) as u8,
                hull_lost: false,
                prime: super::behaviour::prime_for(b.int("characterID").unwrap_or(0) as i32),
            }
        })
        .collect();
    (bots, ids)
}
