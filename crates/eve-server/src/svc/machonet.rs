//! `machoNet` as a remote service.

use std::collections::HashMap;

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};

/// The star map keys its pilot counts by solarSystemID minus this.
const SOLAR_SYSTEM_BASE: i64 = 30_000_000;

pub async fn macho_net(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    match call.method.as_str() {
        // ConnectToServer (machoNet.py): `(self.serviceInfo, initvals) = ...`,
        // then GetCachedObject() on every handle in initvals, then
        // cfg.GotInitData(initvals). serviceInfo maps a service name to how
        // its calls are routed (`service in self.machoNet.serviceInfo`); one
        // node serves them all here, so it is empty.
        "GetInitVals" => Some(Outcome::Value(Value::Tuple(vec![
            crate::bulkdata::service_info(),
            ctx.bulk.init_vals(),
        ]))),
        // eve.SynchronizeClock, the client's first call once it is connected.
        "GetTime" => Some(Outcome::Value(Value::Long(crate::bulkdata::filetime_now()))),
        // map.py: (in space, docked, divisor), each {solarSystemID - 30000000: pilots}.
        // It adds the two for its "pilots in space" view.
        "GetClusterSessionStatistics" => {
            let (space, docked) = ctx.world.lock().counts();
            let dict = |m: HashMap<i64, i64>| {
                Value::Dict(
                    m.into_iter()
                        .map(|(system, n)| (Value::Int(system - SOLAR_SYSTEM_BASE), Value::Int(n)))
                        .collect(),
                )
            };
            Some(Outcome::Value(Value::Tuple(vec![dict(space), dict(docked), Value::Int(1)])))
        }
        _ => None,
    }
}
