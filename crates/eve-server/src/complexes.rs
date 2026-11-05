//! Deadspace complexes: pockets of pirates off the beaten track, rated 1 to
//! 10, in rooms joined by acceleration gates, the overseer's belongings
//! waiting at the end. There is no dungeon data, so they are generated.
//!
//! - Complexes are kept in `deadspaceComplexes`, a few hundred across New
//!   Eden at a time (`seed`, from the heartbeat): more and harder in low- and
//!   null-sec. Each lasts a few days, or half an hour once cleared, and
//!   another appears somewhere else.
//! - Most show a beacon in their system (warpable, like a celestial); some
//!   are hidden, a cosmic signature only probes find (svc::scan). A pilot
//!   who comes close to a hidden one uncovers it for everyone.
//! - Each room is far from the next. Its rats (the system's pirates, bigger
//!   in each room than the last) appear when a pilot first comes near; its
//!   acceleration gate (keeper.ActivateAccelerationGate) opens once they are
//!   dead, and flings the ship on to the next room. When the last room is
//!   cleared, the Overseer's Belongings (a can of loot worth its rating)
//!   appear there.

#[cfg(test)]
use std::collections::BTreeMap;

use tokio_postgres::Client;
use tracing::{error, info};

use crate::ballpark::{Park, Vec3};
use crate::beacons::{self, Beacon, Kind};
use crate::heartbeat::{Job, Out};
use crate::svc::rows::{float, int};

pub const TYPE_COMPLEX_BEACON: i64 = 10124;
pub const TYPE_ACCELERATION_GATE: i64 = 17831;
pub const TYPE_DEADSPACE_SIGNATURE: i64 = 19728;
/// How many complexes there are at once, at most.
const TARGET: i64 = 600;
const NEW_PER_RUN: i64 = 40;
const LIFETIME_DAYS: (i64, i64) = (2, 5);
const CLEARED_LINGER_FT: i64 = 30 * 60 * 10_000_000;
const DAY_FT: i64 = 86_400 * 10_000_000;
/// A room wakes when a pilot comes this close.
const WAKE_RANGE: f64 = 150_000.0;
/// A hidden complex shows itself to a pilot this close.
const UNCOVER_RANGE: f64 = 100_000.0;
/// How far apart the rooms are: too far to see one from another.
const ROOM_GAP: f64 = 2.0e10;
const GATE_OFFSET: f64 = 15_000.0;
/// How close a ship must be to use a gate (const.maxStargateJumpingDistance).
pub const GATE_RANGE: f64 = 2_500.0;

const NAMES: [&str; 6] = ["Hideout", "Lookout", "Refuge", "Den", "Outpost", "Stronghold"];

/// A complex in a park, as it is being played.
#[derive(Debug, Clone)]
pub struct Complex {
    pub id: i64,
    pub level: u8,
    pub name: String,
    pub rooms: Vec<Room>,
    /// Hidden until a pilot comes close (or it is scanned down).
    pub hidden: bool,
    pub cleared: bool,
}

#[derive(Debug, Clone)]
pub struct Room {
    pub pos: Vec3,
    /// What the room's rats belong to (their "belt"), and its gate, if any.
    pub anchor: i64,
    pub gate: Option<i64>,
    pub rats: Vec<i64>,
    pub woke: bool,
}

impl Complex {
    fn room_of_anchor(&self, anchor: i64) -> Option<usize> {
        self.rooms.iter().position(|r| r.anchor == anchor)
    }
}

/// Keeps New Eden supplied with complexes: the expired go, new ones appear.
pub async fn seed(db: &Client) {
    let result = async {
        let now = crate::bulkdata::filetime_now();
        let gone = db.execute(r#"DELETE FROM "deadspaceComplexes" WHERE "expires" < $1"#, &[&now]).await?;
        let have: i64 = db.query_one(r#"SELECT count(*) FROM "deadspaceComplexes""#, &[]).await?.get(0);
        let want = (TARGET - have).clamp(0, NEW_PER_RUN);
        if want > 0 {
            // Systems with planets to hide behind, low- and null-sec three times as likely.
            db.execute(
                r#"INSERT INTO "deadspaceComplexes" ("solarSystemID","level","seed","x","y","z","rooms","cleared","spawnedAt","expires")
                   SELECT s."solarSystemID", 0, (random() * 2147483647)::bigint,
                          p."x" + (random() - 0.5) * 4e11, p."y" + (random() - 0.5) * 4e10, p."z" + (random() - 0.5) * 4e11,
                          0, 0, $2, $2 + ((random() * $4 + $3) * $5)::bigint
                     FROM "mapSolarSystems" s
                     JOIN LATERAL (SELECT d."x", d."y", d."z" FROM "mapDenormalize" d WHERE d."solarSystemID" = s."solarSystemID" AND d."groupID" = 7
                                    ORDER BY random() LIMIT 1) p ON true
                    ORDER BY random() * CASE WHEN s."security" >= 0.45 THEN 3 ELSE 1 END
                    LIMIT $1"#,
                &[&want, &now, &(LIFETIME_DAYS.0 as f64), &((LIFETIME_DAYS.1 - LIFETIME_DAYS.0) as f64), &(DAY_FT as f64)],
            )
            .await?;
            // Their rating and rooms, from the system's security and the seed.
            db.execute(
                r#"UPDATE "deadspaceComplexes" c SET
                       "level" = LEAST(10, GREATEST(1, round(CASE WHEN s."security" >= 0.45 THEN 1 WHEN s."security" > 0 THEN 3 ELSE 5 END + (c."seed" % 1000) / 1000.0 * 5)))::smallint,
                       "rooms" = (2 + c."seed" % 3)::smallint
                  FROM "mapSolarSystems" s WHERE s."solarSystemID" = c."solarSystemID" AND c."level" = 0"#,
                &[],
            )
            .await?;
        }
        Ok::<_, tokio_postgres::Error>((gone, want))
    }
    .await;
    match result {
        Ok((gone, made)) if gone > 0 || made > 0 => info!("deadspace complexes: {gone} gone, {made} new"),
        Ok(_) => {}
        Err(e) => error!("deadspace complexes: {}", crate::logging::chain(&e)),
    }
}

/// The complexes of a system, for its park when it is made.
pub async fn load(db: &Client, system: i64) -> Vec<(i64, u8, i64, Vec3, u8, bool)> {
    let sql = r#"SELECT "complexID", "level", "seed", "x", "y", "z", "rooms", "cleared" FROM "deadspaceComplexes"
                  WHERE "solarSystemID" = $1 AND "expires" > $2"#;
    match db.query(sql, &[&(system as i32), &crate::bulkdata::filetime_now()]).await {
        Ok(rows) => rows
            .iter()
            .map(|r| {
                (
                    int(r, 0).unwrap_or(0),
                    int(r, 1).unwrap_or(1) as u8,
                    int(r, 2).unwrap_or(0),
                    Vec3::new(float(r, 3).unwrap_or(0.0), float(r, 4).unwrap_or(0.0), float(r, 5).unwrap_or(0.0)),
                    int(r, 6).unwrap_or(2) as u8,
                    int(r, 7).unwrap_or(0) != 0,
                )
            })
            .collect(),
        Err(e) => {
            error!("loading the complexes of {system}: {}", crate::logging::chain(&e));
            Vec::new()
        }
    }
}

/// Lays a system's complexes out in its park: beacons for the visible,
/// signatures for the hidden, and a gate in every room but the last.
pub fn place(park: &mut Park, found: Vec<(i64, u8, i64, Vec3, u8, bool)>, faction: &str) {
    for (id, level, seed, at, rooms, cleared) in found {
        if cleared {
            continue;
        }
        let mut rng = crate::combat::Rng::seeded(seed as u64);
        let hidden = seed % 3 == 0;
        let name = format!("{faction} {} ({level}/10)", NAMES[rng.below(NAMES.len())]);
        let dir = rng.in_sphere(1.0);
        let dir = if dir.len() > 1e-6 { dir.unit() } else { Vec3::new(1.0, 0.0, 0.0) };
        let mut list: Vec<Room> = (0..rooms.max(1))
            .map(|k| Room { pos: at + dir * (ROOM_GAP * k as f64), anchor: crate::npc::next_id(), gate: None, rats: Vec::new(), woke: false })
            .collect();
        let entrance = if hidden {
            Beacon::new(id, TYPE_DEADSPACE_SIGNATURE, None, "Cosmic Signature", at, 500.0, Kind::Complex { complex_id: id }).hidden()
        } else {
            Beacon::new(id, TYPE_COMPLEX_BEACON, None, name.clone(), at, 1_000.0, Kind::Complex { complex_id: id })
        };
        park.beacons.insert(entrance.id, entrance);
        let count = list.len();
        for k in 0..count.saturating_sub(1) {
            let gate_id = crate::npc::next_id();
            let landing = list[k + 1].pos + Vec3::new(5_000.0, 0.0, 0.0);
            let gate = Beacon::new(
                gate_id,
                TYPE_ACCELERATION_GATE,
                None,
                format!("Acceleration Gate (room {} of {count})", k + 1),
                list[k].pos + Vec3::new(GATE_OFFSET, 0.0, 0.0),
                2_500.0,
                Kind::Gate { complex_id: id, from: k as u8, to: (k + 1) as u8, landing },
            );
            list[k].gate = Some(gate_id);
            park.beacons.insert(gate_id, gate);
        }
        park.complexes.insert(id, Complex { id, level, name, rooms: list, hidden, cleared: false });
    }
}

/// Each pulse: rooms wake as pilots come near, hidden complexes show
/// themselves to those who find them.
pub fn step(park: &mut Park, now: i64, out: &mut Out) {
    let players: Vec<Vec3> = park.players().map(|s| s.pos).collect();
    if players.is_empty() || park.complexes.is_empty() {
        return;
    }
    let near = |p: Vec3, r: f64| players.iter().any(|x| (*x - p).len() <= r);
    // Uncovering.
    let uncovered: Vec<i64> = park.complexes.values().filter(|c| c.hidden && near(c.rooms[0].pos, UNCOVER_RANGE)).map(|c| c.id).collect();
    for id in uncovered {
        let Some(c) = park.complexes.get_mut(&id) else { continue };
        c.hidden = false;
        let name = c.name.clone();
        if let Some(b) = park.beacons.remove(&id) {
            let shown = Beacon::new(b.id, TYPE_COMPLEX_BEACON, None, name, b.pos, 1_000.0, b.kind);
            for e in beacons::add(park, shown) {
                out.destiny(park.solar_system_id, e);
            }
        }
        info!("complex {id} uncovered");
    }
    // Waking rooms.
    let security = park.wildlife.security;
    let mut wake: Vec<(i64, usize)> = Vec::new();
    for c in park.complexes.values() {
        for (k, r) in c.rooms.iter().enumerate() {
            if !r.woke && near(r.pos, WAKE_RANGE) {
                wake.push((c.id, k));
            }
        }
    }
    for (id, k) in wake {
        let Some(c) = park.complexes.get(&id) else { continue };
        let (level, pos, anchor) = (c.level, c.rooms[k].pos, c.rooms[k].anchor);
        // Each room meaner than the last, the rating meaner still.
        let fierce = (security - 0.15 * level as f64 - 0.2 * k as f64).max(-1.0);
        let groups = 1 + level as usize / 3 + k / 2;
        let mut rats = Vec::new();
        for _ in 0..groups {
            rats.extend(crate::npc::spawn_group(park, anchor, pos, fierce, now));
        }
        for rat in &rats {
            for e in crate::svc::space::add_ball_entries(park, *rat) {
                out.destiny(park.solar_system_id, e);
            }
        }
        if let Some(c) = park.complexes.get_mut(&id) {
            c.rooms[k].rats = rats;
            c.rooms[k].woke = true;
            info!("complex {id} room {} woke with {} rat(s)", k + 1, c.rooms[k].rats.len());
        }
    }
}

/// A rat died: if it was a complex's, its room may be clear, and the last
/// room's clearing leaves the overseer's belongings. True if it was one.
pub fn rat_killed(park: &mut Park, anchor: i64, rat: i64, killer: Option<i64>, out: &mut Out) -> bool {
    let Some((id, k)) = park.complexes.values().find_map(|c| c.room_of_anchor(anchor).map(|k| (c.id, k))) else { return false };
    let Some(c) = park.complexes.get_mut(&id) else { return true };
    c.rooms[k].rats.retain(|r| *r != rat);
    if !c.rooms[k].rats.is_empty() {
        return true;
    }
    info!("complex {id} room {} cleared", k + 1);
    if k + 1 == c.rooms.len() && !c.cleared {
        c.cleared = true;
        let (level, pos) = (c.level, c.rooms[k].pos);
        let worth = 2_000_000.0 * (level as f64).powf(1.6);
        let stash = crate::loot::roll(worth, (3, 6), &mut park.rng);
        out.jobs.push(Job::Loot { system: park.solar_system_id, pos: pos + Vec3::new(0.0, 3_000.0, 0.0), owner_id: killer.unwrap_or(0), stash });
        out.jobs.push(Job::ComplexCleared { complex_id: id });
        info!("complex {id} cleared: the overseer's belongings are out");
    }
    true
}

/// keeper.ActivateAccelerationGate(gateID): where the gate sends the ship,
/// if it may go.
pub fn gate_landing(park: &Park, ship_id: i64, gate: i64) -> Result<Vec3, String> {
    let Some(b) = park.beacons.get(&gate) else { return Err("That is not an acceleration gate.".into()) };
    let Kind::Gate { complex_id, from, landing, .. } = b.kind else { return Err("That is not an acceleration gate.".into()) };
    let distance = park.surface_distance(ship_id, gate).unwrap_or(f64::MAX);
    if distance > GATE_RANGE * 1.25 {
        return Err(format!("You must be within {:.1} km of the gate to activate it.", GATE_RANGE / 1_000.0));
    }
    let room = park.complexes.get(&complex_id).and_then(|c| c.rooms.get(from as usize));
    if room.is_some_and(|r| !r.rats.is_empty()) {
        return Err("The gate will not activate while its guardians live.".into());
    }
    Ok(landing)
}

/// Marks a complex cleared, so it goes after a while.
pub async fn cleared(db: &Client, complex_id: i64) {
    let now = crate::bulkdata::filetime_now();
    let result = db
        .execute(r#"UPDATE "deadspaceComplexes" SET "cleared" = 1, "expires" = LEAST("expires", $2) WHERE "complexID" = $1"#, &[&(complex_id as i32), &(now + CLEARED_LINGER_FT)])
        .await;
    if let Err(e) = result {
        error!("clearing complex {complex_id}: {}", crate::logging::chain(&e));
    }
}

/// map.GetDeadspaceComplexMap: (solarSystemID, dungeonID, difficulty, name)
/// of the complexes not hidden.
pub async fn map(db: &Client) -> Vec<(i64, i64, i64, String)> {
    let sql = r#"SELECT "solarSystemID", "complexID", "level", "seed" FROM "deadspaceComplexes" WHERE "expires" > $1 AND "seed" % 3 <> 0 AND "cleared" = 0"#;
    match db.query(sql, &[&crate::bulkdata::filetime_now()]).await {
        Ok(rows) => rows
            .iter()
            .map(|r| {
                let seed = int(r, 3).unwrap_or(0);
                let mut rng = crate::combat::Rng::seeded(seed as u64);
                let name = format!("Deadspace {}", NAMES[rng.below(NAMES.len())]);
                (int(r, 0).unwrap_or(0), int(r, 1).unwrap_or(0), int(r, 2).unwrap_or(1), name)
            })
            .collect(),
        Err(e) => {
            error!("the deadspace map: {}", crate::logging::chain(&e));
            Vec::new()
        }
    }
}

/// The rooms of a park's complexes, for tests and inspection.
#[cfg(test)]
pub fn rooms(park: &Park) -> BTreeMap<i64, usize> {
    park.complexes.iter().map(|(id, c)| (*id, c.rooms.len())).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_complex_has_a_gate_in_every_room_but_the_last_and_locks_it_until_cleared() {
        let mut park = Park::new(30_000_001, Vec::new(), 0);
        place(&mut park, vec![(77, 4, 1, Vec3::ZERO, 3, false)], "Serpentis");
        let c = &park.complexes[&77];
        assert_eq!(c.rooms.len(), 3);
        assert!(c.rooms[0].gate.is_some() && c.rooms[1].gate.is_some() && c.rooms[2].gate.is_none());
        assert_eq!(rooms(&park)[&77], 3);
        let gate = c.rooms[0].gate.unwrap();
        // A ship at the gate, with a rat still alive in the room.
        let pilot = crate::ballpark::Pilot { type_id: 587, group_id: 25, name: "P".into(), char_id: 9, corp_id: 1, alliance_id: None, security: 0.0, bounty: 0.0 };
        let at = park.beacons[&gate].pos + Vec3::new(2_600.0, 0.0, 0.0);
        park.ships.insert(5, crate::ballpark::Ship::new(5, 1, crate::ballpark::ShipStats::new(30.0, 1e6, 1.0, 300.0, 1.0), pilot, at, Vec3::ZERO));
        park.complexes.get_mut(&77).unwrap().rooms[0].rats = vec![1234];
        assert!(gate_landing(&park, 5, gate).is_err());
        let anchor = park.complexes[&77].rooms[0].anchor;
        assert!(rat_killed(&mut park, anchor, 1234, Some(9), &mut Out::default()));
        let landing = gate_landing(&park, 5, gate).unwrap();
        assert!((landing - park.complexes[&77].rooms[1].pos).len() < 10_000.0);
    }

    #[test]
    fn clearing_the_last_room_leaves_the_overseers_belongings() {
        let mut park = Park::new(30_000_001, Vec::new(), 0);
        place(&mut park, vec![(78, 6, 2, Vec3::ZERO, 2, false)], "Guristas");
        let anchor = park.complexes[&78].rooms[1].anchor;
        park.complexes.get_mut(&78).unwrap().rooms[1].rats = vec![1];
        let mut out = Out::default();
        rat_killed(&mut park, anchor, 1, Some(9), &mut out);
        assert!(park.complexes[&78].cleared);
        assert!(out.jobs.iter().any(|j| matches!(j, Job::ComplexCleared { complex_id: 78 })));
    }
}
