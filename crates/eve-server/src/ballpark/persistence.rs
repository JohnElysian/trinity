//! Durable motion only: identity, combat targets and active modules are rebuilt
//! from their authoritative owners, never deserialized with a ball.
use super::*;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Motion {
    version: u8,
    pub tick: i64,
    pos: Vec3,
    vel: Vec3,
    prev_vel: Vec3,
    goal: Vec3,
    fraction: f64,
    command: Command,
    warp: Option<WarpState>,
    factor: i64,
}

impl Ship {
    pub fn motion(&self, tick: i64) -> Motion {
        Motion {
            version: 1,
            tick,
            pos: self.pos,
            vel: self.vel,
            prev_vel: self.prev_vel,
            goal: self.goal,
            fraction: self.speed_fraction,
            command: self.command.clone(),
            warp: self.warp,
            factor: self.stats.warp_factor,
        }
    }

    /// Resume an already paid warp using its native curve. Other orders resume
    /// on the live park, where targets and collisions can be checked. This does
    /// not attempt to invent the original server's emergency logout warp.
    pub fn restore_motion(&mut self, saved: &Motion, now: i64) -> bool {
        if saved.version != 1
            || saved.tick > now
            || saved.tick < 0
            || ![saved.pos, saved.vel, saved.prev_vel, saved.goal]
                .into_iter()
                .all(Vec3::is_finite)
            || !saved.fraction.is_finite()
            || !(0.0..=1.0).contains(&saved.fraction)
            || !(1..=i32::MAX as i64).contains(&saved.factor)
            || !self.stats.max_velocity.is_finite()
            || self.stats.max_velocity < 0.0
            || !valid_command(self.id, saved.pos, &saved.command)
        {
            return false;
        }
        let age = match saved.warp {
            None if !matches!(saved.command, Command::Warp { .. }) => None,
            Some(WarpState::Aligning) if matches!(saved.command, Command::Warp { .. }) => None,
            Some(WarpState::InWarp {
                exit,
                distance,
                start,
            }) if matches!(saved.command, Command::Warp { .. }) => {
                let age = (crate::destiny::stamp(saved.tick.saturating_sub(1)) as i32)
                    .wrapping_sub(start);
                if age < 0
                    || !exit.is_finite()
                    || !(saved.pos - exit).is_finite()
                    || !distance.is_finite()
                    || distance <= 0.0
                {
                    return false;
                }
                Some(age as f64)
            }
            _ => return false,
        };
        let mut ship = self.clone();
        ship.pos = saved.pos;
        ship.vel = saved.vel;
        ship.prev_vel = saved.prev_vel;
        ship.goal = saved.goal;
        ship.speed_fraction = saved.fraction;
        ship.command = saved.command.clone();
        ship.warp = saved.warp;
        if let (Some(age), Some(WarpState::InWarp { exit, distance, .. })) = (age, saved.warp) {
            ship.stats.warp_factor = saved.factor;
            let elapsed = now.saturating_sub(saved.tick) as f64;
            if elapsed > 0.0 {
                let top =
                    (saved.factor as f64 * WARP_FACTOR_SPEED).min(distance * WARP_DISTANCE_SPEED);
                let slowing = (3.0 * distance + top * ((top / 3.0).ln() - 2.0)) / (3.0 * top);
                let threshold = slowing + (top / (3.0 * ship.stats.max_velocity)).ln();
                let end = slowing.ceil().max(threshold.floor() + 1.0);
                if end.is_nan() || !slowing.is_finite() {
                    return false;
                }
                let target_age = age + elapsed;
                let t = target_age.min(end.max(age));
                if t > age + 1.0 {
                    warp_along(&mut ship, exit, distance, t - 1.0);
                }
                ship.prev_vel = ship.vel;
                warp_along(&mut ship, exit, distance, t);
                if !ship.in_warp() && target_age > t {
                    coast(&mut ship, target_age - t);
                }
            }
        }
        if !ship.pos.is_finite() || !ship.vel.is_finite() || !ship.prev_vel.is_finite() {
            return false;
        }
        *self = ship;
        true
    }
}

/// Closed sum of STOP's discrete levelling and exponential drift. Long offline
/// intervals do not loop once per second or alias the 32-bit Destiny clock.
fn coast(ship: &mut Ship, seconds: f64) {
    let tau = ship.stats.inertia() / FRICTION;
    let k = (-1.0 / tau).exp();
    let q = (1.0 - STOP_LEVELLING) / STOP_LEVELLING_DIV;
    let before = ship.vel;
    let decay = (-seconds / tau).exp();
    let vertical = (q * k).powf(seconds);
    ship.pos.x += before.x * tau * (1.0 - decay);
    ship.pos.z += before.z * tau * (1.0 - decay);
    ship.pos.y += before.y * q * tau * (1.0 - k) * (1.0 - vertical) / (1.0 - q * k);
    ship.vel = Vec3::new(before.x * decay, before.y * vertical, before.z * decay);
    let previous = (-(seconds - 1.0) / tau).exp();
    ship.prev_vel = Vec3::new(
        before.x * previous,
        before.y * q * (q * k).powf(seconds - 1.0),
        before.z * previous,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    fn park(tick: i64) -> Park {
        let mut park = Park::new(1, vec![], tick);
        let pilot = Pilot {
            type_id: 587,
            group_id: 25,
            name: "resume".into(),
            char_id: 7,
            corp_id: 8,
            alliance_id: None,
            security: 0.0,
            bounty: 0.0,
        };
        park.ships.insert(
            1,
            Ship::new(
                1,
                1,
                ShipStats::new(10.0, 1.1e6, 3.0, 320.0, 1.0),
                pilot,
                Vec3::ZERO,
                Vec3::ZERO,
            ),
        );
        park.command(
            1,
            Command::Warp {
                target: Vec3::new(5.0e12, 2.0e11, -3.0e11),
                range: 15_000.0,
            },
        );
        park.advance(tick + 10);
        assert!(park.ships[&1].in_warp());
        park
    }
    #[test]
    fn durable_warp_matches_uninterrupted_native_motion_and_landing() {
        for base in [0, i32::MAX as i64 - 15, u32::MAX as i64 - 15] {
            for downtime in [0, 1, 10, 30, 55, 500] {
                let mut live = park(base);
                let json = serde_json::to_string(&live.ships[&1].motion(live.tick)).unwrap();
                let saved: Motion = serde_json::from_str(&json).unwrap();
                let mut restored = live.ships[&1].clone();
                restored.arrive(Vec3::ZERO);
                assert!(restored.restore_motion(&saved, live.tick + downtime));
                live.advance(live.tick + downtime);
                let expected = &live.ships[&1];
                assert!(
                    (restored.pos - expected.pos).len() < 0.02,
                    "base {base} delta {downtime}: {:?} {:?}",
                    restored.pos,
                    expected.pos
                );
                assert!((restored.vel - expected.vel).len() < 0.002);
                assert_eq!(restored.in_warp(), expected.in_warp());
                assert_eq!(restored.command, expected.command);
                assert_eq!(restored.warp_snapshot(), expected.warp_snapshot());
            }
        }
    }
    #[test]
    fn long_offline_warp_finishes_without_clock_alias_or_double_charge() {
        let live = park(100);
        let mut restored = live.ships[&1].clone();
        assert!(restored.restore_motion(
            &restored.motion(live.tick),
            live.tick + u32::MAX as i64 + 1000
        ));
        assert!(!restored.warping());
        assert!(restored.pos.is_finite());
        assert_eq!(restored.vel, Vec3::ZERO);
    }
    #[test]
    fn invalid_or_future_snapshot_is_rejected_without_mutation() {
        let live = park(100);
        let ship = &live.ships[&1];
        for broken in 0..4 {
            let mut saved = ship.motion(live.tick);
            match broken {
                0 => saved.pos.x = f64::NAN,
                1 => saved.tick += 1,
                2 => saved.factor = 0,
                _ => saved.command = Command::Stop,
            }
            let mut restored = ship.clone();
            assert!(!restored.restore_motion(&saved, live.tick));
            assert_eq!(restored.pos, ship.pos);
            assert_eq!(restored.warp_snapshot(), ship.warp_snapshot());
        }
    }

    #[test]
    fn immobilized_ship_still_has_a_valid_durable_checkpoint() {
        let mut live = park(100);
        let ship = live.ships.get_mut(&1).unwrap();
        ship.arrive(Vec3::new(17.0, 19.0, 23.0));
        ship.stats.max_velocity = 0.0;
        let saved = ship.motion(live.tick);
        assert!(ship.restore_motion(&saved, live.tick));
        assert_eq!(ship.pos, Vec3::new(17.0, 19.0, 23.0));
    }
}
