//! Mission sites: a beacon where an agent's encounter waits, and the rats
//! sent to it. A site opens when its pilot first heads there and lives in
//! the system's park; when the last of its rats dies the mission's
//! objective is done (`Job::SiteCleared`).
//!
//! A new pilot's personal tutorial site is one too, belonging to no mission
//! ([`TUTORIAL`]): the client's tutorial warps a pilot there on first
//! undocking in a rookie system (`WarpToStuff("tutorial")`).

use eve_marshal::Value;

use crate::ballpark::{Park, Vec3};
use crate::destiny::{self, Ball};
use crate::svc::space::{self, entry};

/// Celestial Agent Site Beacon.
pub const TYPE_BEACON: i64 = 25354;
const BEACON_RADIUS: f64 = 1_000.0;
const OWNER_SYSTEM: i64 = 1;

/// The mission of a personal tutorial site: none.
pub const TUTORIAL: i64 = 0;
/// How far from the pilot a tutorial site opens: a short warp.
const TUTORIAL_DISTANCE: f64 = 5_000_000_000.0;
/// The strength of its rats: what the safest space holds (a few frigates).
const TUTORIAL_SECURITY: f64 = 1.0;

#[derive(Debug, Clone)]
pub struct Site {
    pub id: i64,
    pub mission_id: i64,
    pub char_id: i64,
    pub name: String,
    pub pos: Vec3,
    /// The rats still alive.
    pub rats: Vec<i64>,
}

impl Site {
    pub fn ball(&self) -> Ball {
        Ball::celestial(self.id, BEACON_RADIUS, self.pos.tuple(), &self.name, false)
    }

    pub fn slim(&self) -> Value {
        space::slim(vec![
            ("itemID", Value::Int(self.id)),
            ("typeID", Value::Int(TYPE_BEACON)),
            ("ownerID", Value::Int(OWNER_SYSTEM)),
            ("name", Value::text(&self.name)),
        ])
    }

    pub fn radius(&self) -> f64 {
        BEACON_RADIUS
    }
}

/// Opens a site with rats of a strength for `security` around it; returns
/// the destiny entries that show it all.
pub fn open(park: &mut Park, mut site: Site, security: f64, now: i64) -> Vec<Value> {
    let blob = destiny::encode(destiny::stamp(park.tick), &[site.ball()]);
    let chunk = Value::Tuple(vec![Value::Bytes(blob), Value::List(vec![site.slim()]), Value::Dict(Vec::new())]);
    let mut entries = vec![entry(park.tick, "AddBalls", vec![chunk])];
    site.rats = crate::npc::spawn_group(park, site.id, site.pos, security, now);
    for id in &site.rats {
        entries.extend(space::add_ball_entries(park, *id));
    }
    if site.mission_id == TUTORIAL {
        tracing::info!("tutorial site {} of {} opened with {} rat(s)", site.id, site.char_id, site.rats.len());
    } else {
        tracing::info!("mission {} site {} opened with {} rat(s)", site.mission_id, site.id, site.rats.len());
    }
    park.sites.insert(site.id, site);
    entries
}

/// Closes a mission's site, and sends off any rats left there.
pub fn close(park: &mut Park, mission_id: i64) -> Vec<Value> {
    let Some(id) = park.sites.values().find(|s| s.mission_id == mission_id).map(|s| s.id) else { return Vec::new() };
    let site = park.sites.remove(&id).expect("the site was just found");
    let mut gone = vec![Value::Int(id)];
    for rat in site.rats {
        if park.ships.remove(&rat).is_some() {
            gone.push(Value::Int(rat));
        }
    }
    vec![entry(park.tick, "RemoveBalls", vec![Value::List(gone)])]
}

/// `char_id`'s tutorial site in this park, opened a short warp from `near`
/// if there is none (or if its rats are all dead, so there is practice
/// again): where it is and its radius, and the destiny entries that show
/// a new one.
pub fn tutorial(park: &mut Park, char_id: i64, near: Vec3, now: i64) -> (Vec3, f64, Vec<Value>) {
    let mine = park.sites.values().find(|s| s.mission_id == TUTORIAL && s.char_id == char_id).map(|s| (s.id, s.pos, s.rats.is_empty()));
    let mut entries = Vec::new();
    match mine {
        Some((_, pos, false)) => return (pos, BEACON_RADIUS, entries),
        Some((id, _, true)) => {
            park.sites.remove(&id);
            entries.push(entry(park.tick, "RemoveBalls", vec![Value::List(vec![Value::Int(id)])]));
        }
        None => {}
    }
    let mut direction = park.rng.in_sphere(1.0);
    direction.y *= 0.1;
    let pos = near + direction.unit() * TUTORIAL_DISTANCE;
    let site = Site { id: crate::npc::next_id(), mission_id: TUTORIAL, char_id, name: "Tutorial Site".into(), pos, rats: Vec::new() };
    entries.extend(open(park, site, TUTORIAL_SECURITY, now));
    (pos, BEACON_RADIUS, entries)
}

/// A rat died: if it was a site's, and the last, the site's mission is done.
pub fn rat_killed(park: &mut Park, site_id: i64, rat: i64) -> Option<(i64, i64)> {
    let site = park.sites.get_mut(&site_id)?;
    site.rats.retain(|r| *r != rat);
    site.rats.is_empty().then_some((site.mission_id, site.char_id))
}
