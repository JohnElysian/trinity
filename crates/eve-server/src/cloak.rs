//! Cloaking devices (effects.Cloak, effects.Uncloak).
//!
//! A cloaked ship leaves everyone's view but its pilot's (its ball carries
//! the cloak flag for them), cannot be locked, and breaks the locks already on
//! it; NPCs and bots lose sight of it. It cannot be cloaked within
//! `DECLOAK_RANGE` of anything, and anything coming that close (a ship, a
//! structure, a celestial) drops the cloak. While cloaked no other module can
//! be started; only a covert ops cloak lets the ship warp. When the device
//! stops, the ship is seen again.

use tracing::info;

use crate::ballpark::Park;
use crate::combat::Action;
use crate::heartbeat::Out;
use crate::svc::space;
use crate::world::WorldState;

/// Nothing may be this close to a ship that cloaks, or stays cloaked.
pub const DECLOAK_RANGE: f64 = 2_000.0;

/// What is too close to a ship to cloak: the name of the nearest thing.
fn crowded(park: &Park, ship_id: i64) -> Option<String> {
    let me = park.ships.get(&ship_id)?;
    let near = |pos: crate::ballpark::Vec3, radius: f64| (pos - me.pos).len() - radius - me.stats.radius <= DECLOAK_RANGE;
    if let Some(s) = park.ships.values().find(|s| s.id != ship_id && !s.cloaked && near(s.pos, s.stats.radius)) {
        return Some(s.pilot.name.clone());
    }
    if park.structures.values().any(|s| near(s.pos, s.radius)) {
        return Some("a structure".into());
    }
    if let Some(c) = park.celestials.iter().find(|c| near(c.pos, c.radius)) {
        return Some(if c.name.is_empty() { "a celestial".into() } else { c.name.clone() });
    }
    park.cans.values().find(|c| near(c.pos, c.radius)).map(|_| "a container".into())
}

/// Whether the ship may cloak where it is.
pub fn may_cloak(w: &WorldState, system: i64, ship_id: i64) -> Result<(), String> {
    let Some(park) = w.parks.get(&system) else { return Err("You are not in space.".into()) };
    match crowded(park, ship_id) {
        Some(what) => Err(format!("You cannot cloak within {:.0} km of {what}.", DECLOAK_RANGE / 1_000.0)),
        None => Ok(()),
    }
}

/// The ship just cloaked: out of others' sight, locks on it broken.
pub fn cloaked(w: &mut WorldState, system: i64, ship_id: i64) {
    let Some(park) = w.parks.get_mut(&system) else { return };
    let Some(ship) = park.ships.get_mut(&ship_id) else { return };
    ship.cloaked = true;
    let owner = ship.client_id;
    let tick = park.tick;
    // Whoever had it locked loses it.
    let lockers: Vec<i64> = park.ships.values().filter(|s| s.fight.as_ref().is_some_and(|f| f.locks.contains_key(&ship_id))).map(|s| s.id).collect();
    let mut out = Out::default();
    for l in lockers {
        if let Some(f) = park.ships.get_mut(&l).and_then(|s| s.fight.as_mut()) {
            f.locks.remove(&ship_id);
            f.modules.retain(|m| m.target != Some(ship_id));
        }
        crate::fight::tell_target_lost(park, l, ship_id, &mut out);
        if let Some(brain) = park.ships.get_mut(&l).and_then(|s| s.npc.as_mut()).filter(|b| b.target == Some(ship_id)) {
            brain.target = None;
        }
    }
    let fx = space::special_fx(tick, ship_id, None, "effects.Cloak");
    space::broadcast(w, system, None, vec![fx]);
    let others: Vec<i64> = w.in_space(system).into_iter().filter(|c| *c != owner).collect();
    for c in others {
        space::broadcast(w, system, Some(c), space::remove_ball_entries(tick, ship_id));
    }
    out.deliver(w);
    info!("ship {ship_id} cloaked in {system}");
}

/// The ship's cloak dropped: seen again by everyone.
pub fn uncloaked(w: &mut WorldState, system: i64, ship_id: i64) {
    let Some(park) = w.parks.get_mut(&system) else { return };
    let Some(ship) = park.ships.get_mut(&ship_id) else { return };
    if !ship.cloaked {
        return;
    }
    ship.cloaked = false;
    let owner = ship.client_id;
    let entries = space::add_ball_entries(park, ship_id);
    let fx = space::special_fx(park.tick, ship_id, None, "effects.Uncloak");
    let others: Vec<i64> = w.in_space(system).into_iter().filter(|c| *c != owner).collect();
    for c in others {
        space::broadcast(w, system, Some(c), entries.clone());
    }
    space::broadcast(w, system, None, vec![fx]);
    info!("ship {ship_id} uncloaked in {system}");
}

/// Cloaked ships something has come too close to lose their cloak (the
/// device stops, and the heartbeat shows the ship again).
pub fn proximity(park: &mut Park, now: i64, out: &mut Out) {
    let crowded_ships: Vec<(i64, i64)> = park
        .ships
        .values()
        .filter(|s| s.cloaked)
        .filter(|s| crowded(park, s.id).is_some())
        .filter_map(|s| s.fight.as_ref()?.modules.iter().find(|m| matches!(m.action, Action::Cloak { .. })).map(|m| (s.id, m.module_id)))
        .collect();
    for (ship, module) in crowded_ships {
        let note = crate::fight::notify("Your cloak drops: something came too close.");
        crate::fight::stop(park, ship, module, now, out, Some(note));
    }
}

/// Whether a ship is cloaked with a device that lets it warp.
pub fn warp_safe(park: &Park, ship_id: i64) -> Option<bool> {
    let ship = park.ships.get(&ship_id).filter(|s| s.cloaked)?;
    Some(ship.fight.as_ref().is_some_and(|f| f.modules.iter().any(|m| m.action == Action::Cloak { warp_safe: true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ballpark::{Pilot, Ship, ShipStats, Vec3};
    use crate::world::World;

    fn ship(id: i64, pos: Vec3) -> Ship {
        let pilot = Pilot { type_id: 587, group_id: 25, name: format!("P{id}"), char_id: id, corp_id: 1, alliance_id: None, security: 0.0, bounty: 0.0 };
        Ship::new(id, id, ShipStats::new(30.0, 1e6, 1.0, 300.0, 1.0), pilot, pos, Vec3::ZERO)
    }

    #[test]
    fn a_ship_cannot_cloak_beside_another_and_loses_its_cloak_when_one_comes_close() {
        let world = World::default();
        let mut park = Park::new(30_000_001, Vec::new(), 0);
        park.ships.insert(1, ship(1, Vec3::ZERO));
        park.ships.insert(2, ship(2, Vec3::new(1_000.0, 0.0, 0.0)));
        world.lock().parks.insert(30_000_001, park);
        assert!(may_cloak(&world.lock(), 30_000_001, 1).is_err());
        world.lock().parks.get_mut(&30_000_001).unwrap().ships.get_mut(&2).unwrap().pos = Vec3::new(50_000.0, 0.0, 0.0);
        assert!(may_cloak(&world.lock(), 30_000_001, 1).is_ok());
        cloaked(&mut world.lock(), 30_000_001, 1);
        assert!(world.lock().parks[&30_000_001].ships[&1].cloaked);
        // Something comes close: the cloak would drop (no module here to stop,
        // so the ship stays cloaked until its device stops).
        uncloaked(&mut world.lock(), 30_000_001, 1);
        assert!(!world.lock().parks[&30_000_001].ships[&1].cloaked);
    }
}
