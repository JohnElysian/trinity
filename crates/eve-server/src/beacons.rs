//! Things put in space for a while that are neither ships nor celestials:
//! cynosural fields (svc::jumps), the beacons and acceleration gates of
//! deadspace complexes (complexes), and scanner probes out on a scan
//! (svc::scan). They are balls like a can: seen by everyone in the system,
//! warpable to, gone when their time is up.

use eve_marshal::Value;

use crate::ballpark::{Park, Vec3};
use crate::destiny::{self, Ball};
use crate::svc::space::{self, entry};

pub const TYPE_CYNOSURAL_FIELD: i64 = 21094;
pub const TYPE_COVERT_CYNOSURAL_FIELD: i64 = 28650;
const OWNER_SYSTEM: i64 = 1;

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    /// A pilot's cynosural field, lit by their ship's generator.
    Cyno { char_id: i64, ship_id: i64 },
    /// The way into a deadspace complex.
    Complex { complex_id: i64 },
    /// An acceleration gate in a complex: room `from` to room `to`, landing
    /// at `landing`.
    Gate { complex_id: i64, from: u8, to: u8, landing: Vec3 },
    /// A scanner probe launched by `char_id`, gone at `until_ms`.
    Probe { char_id: i64, until_ms: i64 },
}

#[derive(Debug, Clone)]
pub struct Beacon {
    pub id: i64,
    pub type_id: i64,
    pub owner_id: i64,
    pub name: String,
    pub pos: Vec3,
    pub radius: f64,
    pub kind: Kind,
    /// Not shown to anyone (a cosmic signature, found only by probes).
    pub hidden: bool,
}

impl Beacon {
    pub fn new(id: i64, type_id: i64, owner_id: Option<i64>, name: impl Into<String>, pos: Vec3, radius: f64, kind: Kind) -> Beacon {
        Beacon { id, type_id, owner_id: owner_id.unwrap_or(OWNER_SYSTEM), name: name.into(), pos, radius, kind, hidden: false }
    }

    pub fn hidden(mut self) -> Beacon {
        self.hidden = true;
        self
    }

    pub fn ball(&self) -> Ball {
        Ball::celestial(self.id, self.radius, self.pos.tuple(), &self.name, false)
    }

    pub fn slim(&self) -> Value {
        space::slim(vec![
            ("itemID", Value::Int(self.id)),
            ("typeID", Value::Int(self.type_id)),
            ("ownerID", Value::Int(self.owner_id)),
            ("name", Value::text(&self.name)),
        ])
    }
}

/// Puts a beacon in a park; returns what shows it.
pub fn add(park: &mut Park, b: Beacon) -> Vec<Value> {
    if b.hidden {
        park.beacons.insert(b.id, b);
        return Vec::new();
    }
    let blob = destiny::encode(destiny::stamp(park.tick), &[b.ball()]);
    let chunk = Value::Tuple(vec![Value::Bytes(blob), Value::List(vec![b.slim()]), Value::Dict(Vec::new())]);
    park.beacons.insert(b.id, b);
    vec![entry(park.tick, "AddBalls", vec![chunk])]
}

/// Takes beacons out of a park; returns what removes them.
pub fn remove(park: &mut Park, ids: &[i64]) -> Vec<Value> {
    let gone: Vec<Value> = ids.iter().filter(|id| park.beacons.remove(id).is_some()).map(|id| Value::Int(*id)).collect();
    if gone.is_empty() {
        return Vec::new();
    }
    vec![entry(park.tick, "RemoveBalls", vec![Value::List(gone)])]
}

/// Probes whose time is up, gone; returns what removes them.
pub fn expire(park: &mut Park, now_ms: i64) -> Vec<Value> {
    let old: Vec<i64> = park.beacons.values().filter(|b| matches!(b.kind, Kind::Probe { until_ms, .. } if until_ms <= now_ms)).map(|b| b.id).collect();
    remove(park, &old)
}
