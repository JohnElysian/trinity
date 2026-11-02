//! Capital ships' jumps: cynosural fields, jump drives and titans' bridges
//! (gang.py, menusvc.py "Jump to member", "Bridge to member", "Jump through").
//!
//! - A pilot lights a cynosural field with a Cynosural Field Generator: it
//!   burns Liquid Ozone from the hold, holds the ship where it is for its
//!   ten-minute cycle, cannot be lit in high-security space, and puts a field
//!   (a beacon) beside the ship. Everyone in the pilot's gang hears of it
//!   (OnJumpBeaconChange) and so may jump to it.
//! - A ship with a jump drive (canJump) in space jumps to a gang member's
//!   field (beyonce.BeaconJump) if it is within the drive's range in light
//!   years, burning isotopes from its hold for each light year and most of its
//!   capacitor, and comes out beside the field.
//! - A titan with a Jump Portal Generator opens a bridge to a gang member's
//!   field (beyonce.BridgeToMember): Strontium Clathrates from its hold, a
//!   minute's portal (OnBridgeModeChange to the gang); gang members near it
//!   jump through (beyonce.JumpThrough), the titan paying fuel by their mass.

use std::collections::HashSet;

use eve_marshal::Value;
use tracing::info;

use crate::ballpark::Vec3;
use crate::beacons::{self, Beacon, Kind};
use crate::dogma::Loadout;
use crate::items::ItemTxn;
use crate::services::Ctx;
use crate::svc::rows::{float, int};
use crate::world::{World, WorldState};

pub const ATTR_CONSUMPTION_TYPE: i32 = 713;
pub const ATTR_CONSUMPTION_QUANTITY: i32 = 714;
const ATTR_CAN_JUMP: i32 = 861;
const ATTR_JUMP_FUEL_TYPE: i32 = 866;
const ATTR_JUMP_RANGE: i32 = 867;
const ATTR_JUMP_FUEL_PER_LY: i32 = 868;
const ATTR_JUMP_CAPACITOR_NEED: i32 = 898;
const ATTR_PORTAL_MASS_FACTOR: i32 = 1001;
const ATTR_CAPACITOR_CAPACITY: i32 = 482;
const FLAG_CARGO: i32 = 5;
pub const GROUP_TITAN: i64 = 30;
const GROUP_JUMP_PORTAL_GENERATOR: i64 = 590;
const METERS_PER_LIGHT_YEAR: f64 = 9.460_730_472_580_8e15;
/// High-security space takes no cynosural field.
const HIGH_SEC: f64 = 0.45;
/// How close to a titan a ship must be to jump through its bridge.
pub const BRIDGE_RANGE: f64 = 5_000.0;

pub type Refused<T> = Result<T, String>;

/// The clients of everyone in a pilot's gang (the pilot included).
pub fn gang_clients(w: &WorldState, char_id: i64) -> Vec<i64> {
    match w.gangs.of(char_id).and_then(|g| w.gangs.get(g)) {
        Some(g) => g.members.iter().flat_map(|m| w.clients_of(*m)).collect(),
        None => w.clients_of(char_id),
    }
}

/// Burns `quantity` of `type_id` from a ship's hold, or refuses if there is
/// not that much.
pub async fn burn_fuel(ctx: &Ctx<'_>, ship_id: i64, type_id: i64, quantity: i64, tell: &[i64]) -> Refused<()> {
    if quantity <= 0 || type_id <= 0 {
        return Ok(());
    }
    let result = async {
        let mut client = crate::db::connect(ctx.db_url).await?;
        let mut txn = ItemTxn::begin(&mut client).await?;
        let stacks: Vec<(i64, i64)> = txn
            .tx()
            .query(
                r#"SELECT "itemID", "quantity" FROM entity WHERE "locationID" = $1 AND "flag" = $2 AND "typeID" = $3 ORDER BY "quantity""#,
                &[&(ship_id as i32), &FLAG_CARGO, &(type_id as i32)],
            )
            .await?
            .iter()
            .map(|r| (r.get::<_, i32>(0) as i64, r.get::<_, i32>(1) as i64))
            .collect();
        let have: i64 = stacks.iter().map(|(_, q)| q).sum();
        if have < quantity {
            return Ok(Err(have));
        }
        let mut left = quantity;
        for (id, q) in stacks {
            if left == 0 {
                break;
            }
            let take = left.min(q);
            if take == q {
                txn.destroy(id).await?;
            } else {
                txn.set_quantity(id, q - take).await?;
            }
            left -= take;
        }
        Ok::<_, tokio_postgres::Error>(Ok(txn.commit().await?))
    }
    .await;
    match result {
        Ok(Ok(events)) => {
            let w = ctx.world.lock();
            for event in events {
                w.notify(tell, "OnItemChange", event);
            }
            Ok(())
        }
        Ok(Err(have)) => {
            let name = ctx
                .query_one(r#"SELECT "typeName" FROM "invTypes" WHERE "typeID" = $1"#, &[&(type_id as i32)])
                .await
                .and_then(|r| r.get::<_, Option<String>>(0))
                .unwrap_or_else(|| "fuel".into());
            Err(format!("That needs {quantity} units of {name} in your cargo hold; you have {have}."))
        }
        Err(e) => {
            tracing::error!("burning fuel in {ship_id}: {}", crate::logging::chain(&e));
            Err("Your ship's fuel could not be read.".into())
        }
    }
}

/// A cynosural field lit: the generator just started on `ship_id`. Refuses
/// in high-security space.
pub fn may_light_cyno(security: f64) -> Refused<()> {
    if security >= HIGH_SEC {
        return Err("Cynosural fields cannot be lit in high-security space.".into());
    }
    Ok(())
}

/// Puts a pilot's cynosural field beside their ship and tells their gang.
pub fn cyno_lit(w: &mut WorldState, system: i64, char_id: i64, ship_id: i64) {
    let Some(park) = w.parks.get_mut(&system) else { return };
    let Some(pos) = park.ships.get(&ship_id).map(|s| s.pos) else { return };
    let id = crate::npc::next_id();
    let field = Beacon::new(id, beacons::TYPE_CYNOSURAL_FIELD, Some(char_id), "Cynosural Field", pos + Vec3::new(0.0, 0.0, 1_500.0), 1_000.0, Kind::Cyno { char_id, ship_id });
    let entries = beacons::add(park, field);
    crate::svc::space::broadcast(w, system, None, entries);
    let clients = gang_clients(w, char_id);
    w.notify(&clients, "OnJumpBeaconChange", Value::Tuple(vec![Value::Int(char_id), Value::Int(system), Value::Int(id), Value::Bool(true)]));
    info!("{char_id} lit a cynosural field ({id}) in {system}");
}

/// A pilot's cynosural field went out (its generator stopped, or the ship
/// left or died): gone from space, and their gang told.
pub fn cyno_out(w: &mut WorldState, system: i64, char_id: i64) {
    let Some(park) = w.parks.get_mut(&system) else { return };
    let ids: Vec<i64> = park.beacons.values().filter(|b| matches!(b.kind, Kind::Cyno { char_id: c, .. } if c == char_id)).map(|b| b.id).collect();
    if ids.is_empty() {
        return;
    }
    let entries = beacons::remove(park, &ids);
    crate::svc::space::broadcast(w, system, None, entries);
    let clients = gang_clients(w, char_id);
    for id in ids {
        w.notify(&clients, "OnJumpBeaconChange", Value::Tuple(vec![Value::Int(char_id), Value::Int(system), Value::Int(id), Value::Bool(false)]));
    }
    info!("{char_id}'s cynosural field in {system} went out");
}

/// Cynosural fields whose ships are gone (destroyed, docked, jumped) go out.
pub fn orphaned_cynos(w: &mut WorldState) {
    let gone: Vec<(i64, i64)> = w
        .parks
        .iter()
        .flat_map(|(system, park)| {
            park.beacons.values().filter_map(move |b| match b.kind {
                Kind::Cyno { char_id, ship_id } => {
                    let burning = park.ships.get(&ship_id).and_then(|s| s.fight.as_ref()).is_some_and(|f| f.modules.iter().any(|m| m.action == crate::combat::Action::Cyno));
                    (!burning).then_some((*system, char_id))
                }
                _ => None,
            })
        })
        .collect();
    for (system, char_id) in gone {
        cyno_out(w, system, char_id);
    }
}

/// map.GetBeaconCount: the cynosural fields burning, by system.
pub fn beacon_count(world: &World) -> Value {
    let w = world.lock();
    let counts: Vec<(Value, Value)> = w
        .parks
        .iter()
        .map(|(id, p)| (*id, p.beacons.values().filter(|b| matches!(b.kind, Kind::Cyno { .. })).count()))
        .filter(|(_, n)| *n > 0)
        .map(|(id, n)| (Value::Int(id), Value::Int(n as i64)))
        .collect();
    Value::Dict(counts)
}

/// Light years between two systems.
pub async fn light_years(ctx: &Ctx<'_>, a: i64, b: i64) -> Option<f64> {
    let rows = ctx
        .query(r#"SELECT "solarSystemID", "x", "y", "z" FROM "mapSolarSystems" WHERE "solarSystemID" = ANY($1)"#, &[&vec![a as i32, b as i32]])
        .await;
    let at = |id: i64| rows.iter().find(|r| int(r, 0) == Some(id)).map(|r| Vec3::new(float(r, 1).unwrap_or(0.0), float(r, 2).unwrap_or(0.0), float(r, 3).unwrap_or(0.0)));
    Some((at(a)? - at(b)?).len() / METERS_PER_LIGHT_YEAR)
}

/// Where a field is: its system's park must hold it, lit by `char_id`.
pub fn field_at(w: &WorldState, system: i64, beacon: i64, char_id: i64) -> Option<Vec3> {
    let b = w.parks.get(&system)?.beacons.get(&beacon)?;
    matches!(b.kind, Kind::Cyno { char_id: c, .. } if c == char_id).then_some(b.pos)
}

/// What a jump drive offers: (range in light years, fuel type, fuel a light
/// year, capacitor needed as a share of capacity, capacitor capacity).
pub struct Drive {
    pub range: f64,
    pub fuel: i64,
    pub fuel_per_ly: f64,
    pub capacitor_share: f64,
    pub capacitor: f64,
    pub portal_mass_factor: f64,
    pub has_portal: bool,
}

/// The ship's jump drive, with its pilot's skills applied; None if it has none.
pub async fn drive(ctx: &Ctx<'_>, char_id: i64, ship_id: i64) -> Option<Drive> {
    let loadout = Loadout::load(ctx.db, ctx.dogma, char_id, Some(ship_id), &HashSet::new()).await.ok()?;
    let i = loadout.find(ship_id)?;
    let v = |a: i32| loadout.value(i, a);
    if v(ATTR_CAN_JUMP) <= 0.0 && v(ATTR_JUMP_RANGE) <= 0.0 {
        return None;
    }
    let portal = loadout.pieces.iter().position(|p| p.group_id == GROUP_JUMP_PORTAL_GENERATOR);
    Some(Drive {
        range: v(ATTR_JUMP_RANGE),
        fuel: v(ATTR_JUMP_FUEL_TYPE) as i64,
        fuel_per_ly: v(ATTR_JUMP_FUEL_PER_LY),
        capacitor_share: v(ATTR_JUMP_CAPACITOR_NEED),
        capacitor: v(ATTR_CAPACITOR_CAPACITY),
        portal_mass_factor: portal.map_or(v(ATTR_PORTAL_MASS_FACTOR), |p| loadout.value(p, ATTR_PORTAL_MASS_FACTOR)),
        has_portal: portal.is_some() || loadout.pieces.get(i).is_some_and(|p| p.group_id == GROUP_TITAN),
    })
}

/// The portal generator fitted to a titan, with its fuel per activation.
pub async fn portal_generator(ctx: &Ctx<'_>, char_id: i64, ship_id: i64) -> Option<(i64, i64, i64)> {
    let loadout = Loadout::load(ctx.db, ctx.dogma, char_id, Some(ship_id), &HashSet::new()).await.ok()?;
    let p = loadout.pieces.iter().position(|p| p.group_id == GROUP_JUMP_PORTAL_GENERATOR)?;
    Some((loadout.pieces[p].item_id, loadout.value(p, ATTR_CONSUMPTION_TYPE) as i64, loadout.value(p, ATTR_CONSUMPTION_QUANTITY) as i64))
}

/// Checks a jump to a gang member's field and pays for it: in the gang,
/// the field lit, within range, fuel in the hold and capacitor to spare.
/// Returns where the jump lands.
pub async fn prepare_beacon_jump(ctx: &Ctx<'_>, char_id: i64, ship_id: i64, from: i64, member: i64, beacon: i64, to: i64) -> Refused<Vec3> {
    let together = {
        let w = ctx.world.lock();
        let g = w.gangs.of(char_id);
        g.is_some() && g == w.gangs.of(member)
    };
    if !together {
        return Err("You can only jump to a cynosural field lit by a member of your gang.".into());
    }
    if from == to {
        return Err("The field is in this system.".into());
    }
    let Some(at) = field_at(&ctx.world.lock(), to, beacon, member) else { return Err("That cynosural field has gone out.".into()) };
    let Some(d) = drive(ctx, char_id, ship_id).await else { return Err("Your ship has no jump drive.".into()) };
    let ly = light_years(ctx, from, to).await.ok_or("Those systems could not be found.")?;
    if ly > d.range {
        return Err(format!("The field is {ly:.2} light years away; your jump drive reaches {:.2}.", d.range));
    }
    // Capacitor first (it comes back), then fuel.
    let need = d.capacitor * d.capacitor_share;
    let now = crate::combat::now_ms();
    let enough = {
        let mut w = ctx.world.lock();
        let fight = w.parks.get_mut(&from).and_then(|p| p.ships.get_mut(&ship_id)).and_then(|s| s.fight.as_mut());
        match fight {
            Some(f) if f.vitals.capacitor.at(now) >= need => {
                f.vitals.capacitor.spend(now, need);
                true
            }
            _ => false,
        }
    };
    if !enough {
        return Err(format!("A jump needs {:.0}% of your capacitor.", d.capacitor_share * 100.0));
    }
    burn_fuel(ctx, ship_id, d.fuel, (d.fuel_per_ly * ly).ceil() as i64, &[ctx.client_id]).await?;
    info!("{char_id} jumps {ly:.2} ly from {from} to {member}'s field in {to}");
    Ok(landing(at))
}

/// Somewhere near a field.
pub fn landing(at: Vec3) -> Vec3 {
    let dir = crate::combat::Rng::seeded(crate::combat::now_ms() as u64).in_sphere(1.0);
    let dir = if dir.len() > 1e-6 { dir.unit() } else { Vec3::new(1.0, 0.0, 0.0) };
    at + dir * 4_000.0
}

/// BridgeToMember(charID, beaconID, solarsystemID): a titan opens a portal
/// to a gang member's field.
pub async fn open_bridge(ctx: &Ctx<'_>, member: i64, beacon: i64, to: i64) -> Refused<()> {
    let (Some(me), Some(ship_id), Some(from)) = (ctx.session.char_id, ctx.session.ship_id, ctx.session.solar_system_id) else {
        return Err("You must be in space to open a bridge.".into());
    };
    let together = {
        let w = ctx.world.lock();
        let g = w.gangs.of(me);
        g.is_some() && g == w.gangs.of(member)
    };
    if !together {
        return Err("You can only bridge to a field lit by a member of your gang.".into());
    }
    if field_at(&ctx.world.lock(), to, beacon, member).is_none() {
        return Err("That cynosural field has gone out.".into());
    }
    let Some(d) = drive(ctx, me, ship_id).await.filter(|d| d.has_portal) else { return Err("Your ship cannot open a jump portal.".into()) };
    let Some((_, fuel, quantity)) = portal_generator(ctx, me, ship_id).await else { return Err("You have no Jump Portal Generator fitted.".into()) };
    let ly = light_years(ctx, from, to).await.ok_or("Those systems could not be found.")?;
    if ly > d.range {
        return Err(format!("The field is {ly:.2} light years away; your portal reaches {:.2}.", d.range));
    }
    burn_fuel(ctx, ship_id, fuel, quantity, &[ctx.client_id]).await?;
    let mut w = ctx.world.lock();
    w.bridges.insert(ship_id, crate::world::Bridge { titan_pilot: me, system: from, to_system: to, beacon, member, until_ms: crate::combat::now_ms() + 60_000, mass_factor: d.portal_mass_factor });
    let clients = gang_clients(&w, me);
    w.notify(&clients, "OnBridgeModeChange", Value::Tuple(vec![Value::Int(ship_id), Value::Int(to), Value::Int(beacon), Value::Bool(true)]));
    let tick = w.parks.get(&from).map_or(0, |p| p.tick);
    crate::svc::space::broadcast(&w, from, None, vec![crate::svc::space::special_fx(tick, ship_id, None, "effects.JumpPortal")]);
    info!("{me} opened a bridge from {from} to {member}'s field in {to}");
    Ok(())
}

/// Bridges whose minute is up close.
pub fn close_bridges(w: &mut WorldState, now_ms: i64) {
    let over: Vec<i64> = w.bridges.iter().filter(|(_, b)| b.until_ms <= now_ms).map(|(id, _)| *id).collect();
    for ship in over {
        let Some(b) = w.bridges.remove(&ship) else { continue };
        let clients = gang_clients(w, b.titan_pilot);
        w.notify(&clients, "OnBridgeModeChange", Value::Tuple(vec![Value::Int(ship), Value::Int(b.to_system), Value::Int(b.beacon), Value::Bool(false)]));
    }
}

/// Checks a jump through a titan's bridge and has the titan pay for it.
/// Returns where it lands.
pub async fn prepare_jump_through(ctx: &Ctx<'_>, char_id: i64, ship_id: i64, from: i64, titan_pilot: i64, titan: i64) -> Refused<(i64, Vec3)> {
    let (bridge, mass, gap, titan_cargo) = {
        let w = ctx.world.lock();
        let Some(b) = w.bridges.get(&titan).cloned() else { return Err("That titan has no bridge open.".into()) };
        let g = w.gangs.of(char_id);
        if g.is_none() || g != w.gangs.of(titan_pilot) || b.titan_pilot != titan_pilot {
            return Err("Only the titan's gang may use its bridge.".into());
        }
        let park = w.parks.get(&from);
        let gap = park.and_then(|p| p.surface_distance(ship_id, titan)).unwrap_or(f64::MAX);
        let mass = park.and_then(|p| p.ships.get(&ship_id)).map_or(0.0, |s| s.stats.mass);
        (b, mass, gap, titan)
    };
    if bridge.system != from {
        return Err("That titan is not in this system.".into());
    }
    if gap > BRIDGE_RANGE {
        return Err(format!("You must be within {:.0} km of the titan to jump through its bridge.", BRIDGE_RANGE / 1_000.0));
    }
    let Some(at) = field_at(&ctx.world.lock(), bridge.to_system, bridge.beacon, bridge.member) else { return Err("The field at the far end has gone out.".into()) };
    let ly = light_years(ctx, from, bridge.to_system).await.unwrap_or(1.0);
    let fuel = (mass * bridge.mass_factor * ly).ceil().max(1.0) as i64;
    let strontium = 16275;
    let titan_clients = ctx.world.lock().clients_of(titan_pilot);
    burn_fuel(ctx, titan_cargo, strontium, fuel, &titan_clients).await.map_err(|why| format!("The titan cannot power your jump: {why}"))?;
    info!("{char_id} jumps through {titan_pilot}'s bridge to {}", bridge.to_system);
    Ok((bridge.to_system, landing(at)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ballpark::{Park, Pilot, Ship, ShipStats};
    use crate::world::World;

    #[test]
    fn a_cyno_field_burns_beside_its_ship_and_goes_out_with_it() {
        let world = World::default();
        let mut park = Park::new(30_000_001, Vec::new(), 0);
        let pilot = Pilot { type_id: 23757, group_id: 547, name: "C".into(), char_id: 7, corp_id: 1, alliance_id: None, security: 0.0, bounty: 0.0 };
        park.ships.insert(70, Ship::new(70, 0, ShipStats::new(300.0, 1e9, 1.0, 100.0, 1.0), pilot, Vec3::ZERO, Vec3::ZERO));
        world.lock().parks.insert(30_000_001, park);
        assert!(may_light_cyno(0.9).is_err() && may_light_cyno(0.2).is_ok());
        cyno_lit(&mut world.lock(), 30_000_001, 7, 70);
        let beacon = { let w = world.lock(); w.parks[&30_000_001].beacons.values().next().map(|b| b.id).unwrap() };
        assert!(field_at(&world.lock(), 30_000_001, beacon, 7).is_some());
        assert_eq!(beacon_count(&world), Value::Dict(vec![(Value::Int(30_000_001), Value::Int(1))]));
        // No generator running on the ship: the field goes out.
        orphaned_cynos(&mut world.lock());
        assert!(field_at(&world.lock(), 30_000_001, beacon, 7).is_none());
    }
}
