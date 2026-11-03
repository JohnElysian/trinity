//! Capacitor used by a warp, separate from Destiny's kinematic curve.
//!
//! Trinity attribute 153 describes a cost per kilogram per AU. The supplied
//! `WarpingWithAvailablePower` message confirms that insufficient charge makes
//! a shorter warp. The coefficient passed here is the fitted ship's effective
//! dogma value, including skill, implant and rig modifiers.

pub const WARP_CAPACITOR_NEED: i32 = 153;
pub const AU_METRES: f64 = 149_597_870_700.0;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quote {
    pub distance_m: f64,
    pub cost: f64,
    pub limited: bool,
}

/// Quote the actual travel distance at warp entry. This does not debit charge:
/// an alignment that is cancelled must not consume capacitor. Invalid state
/// must fail closed instead of injecting NaN into the ship or its capacitor.
pub fn quote(distance_m: f64, mass_kg: f64, need_per_kg_au: f64, available: f64) -> Option<Quote> {
    if ![distance_m, mass_kg, need_per_kg_au, available].iter().all(|v| v.is_finite())
        || distance_m < 0.0 || mass_kg <= 0.0 || need_per_kg_au < 0.0 || available < 0.0
    {
        return None;
    }
    let cost = (distance_m / AU_METRES) * mass_kg * need_per_kg_au;
    if !cost.is_finite() {
        return None;
    }
    if cost <= available {
        Some(Quote { distance_m, cost, limited: false })
    } else {
        Some(Quote { distance_m: distance_m * (available / cost), cost: available, limited: true })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(actual: f64, expected: f64) {
        assert!((actual - expected).abs() <= expected.abs().max(1.0) * 1e-12, "{actual} != {expected}");
    }

    #[test]
    fn warp_cost_uses_effective_mass_distance_and_dogma_coefficient() {
        let q = quote(20.0 * AU_METRES, 1_000_000.0, 8e-7, 500.0).unwrap();
        close(q.cost, 16.0);
        assert!(!q.limited);
        close(quote(40.0 * AU_METRES, 1_000_000.0, 8e-7, 500.0).unwrap().cost, 32.0);
        close(quote(20.0 * AU_METRES, 2_000_000.0, 8e-7, 500.0).unwrap().cost, 32.0);
        close(quote(20.0 * AU_METRES, 1_000_000.0, 4e-7, 500.0).unwrap().cost, 8.0);
    }

    #[test]
    fn insufficient_charge_only_buys_the_affordable_distance() {
        let q = quote(20.0 * AU_METRES, 1_000_000.0, 8e-7, 4.0).unwrap();
        assert!(q.limited);
        close(q.distance_m, 5.0 * AU_METRES);
        close(q.cost, 4.0);
        let full = quote(q.distance_m, 1_000_000.0, 8e-7, 4.0).unwrap();
        close(full.cost, q.cost);
        assert!(!full.limited);
    }

    #[test]
    fn empty_charge_and_zero_cost_have_finite_results() {
        assert_eq!(quote(AU_METRES, 1e6, 1e-6, 0.0), Some(Quote { distance_m: 0.0, cost: 0.0, limited: true }));
        assert_eq!(quote(AU_METRES, 1e6, 0.0, 0.0), Some(Quote { distance_m: AU_METRES, cost: 0.0, limited: false }));
        assert_eq!(quote(0.0, 1e6, 1e-6, 0.0), Some(Quote { distance_m: 0.0, cost: 0.0, limited: false }));
    }

    #[test]
    fn invalid_or_overflowing_state_does_not_quote_a_warp() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            assert!(quote(bad, 1e6, 1e-6, 100.0).is_none());
            assert!(quote(AU_METRES, bad, 1e-6, 100.0).is_none());
            assert!(quote(AU_METRES, 1e6, bad, 100.0).is_none());
            assert!(quote(AU_METRES, 1e6, 1e-6, bad).is_none());
        }
        assert!(quote(AU_METRES, 0.0, 1e-6, 100.0).is_none());
        assert!(quote(f64::MAX, f64::MAX, 1.0, 100.0).is_none());
    }
}
