use eve_marshal::Value;
use tracing::warn;

use crate::services::{Call, Ctx, Outcome};
use crate::store::{Db, Row};
use crate::svc::rows::{row, rowset};

pub async fn station(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetStationItemBits" => station_item_bits(ctx).await,
        "GetGuests" => guests(ctx).await,
        _ => return None,
    };
    Some(Outcome::Value(value))
}

pub async fn station_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetStation" => ctx.store.read(|db| get_station(db, call.arg_id(0))),
        "GetSolarSystem" => ctx.store.read(|db| get_solar_system(db, call.arg_id(0))),
        "GetConstellation" => get_constellation(call.arg_id(0)),
        "GetRegion" => get_region(call.arg_id(0)),
        "GetAllianceSystems" => ctx.store.read(alliance_systems),
        "GetAllianceConstellations" => alliance_constellations(),
        "GetStationsByRegion" => ctx.store.read(|db| stations_by_region(db, call.arg_id(0))),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// The station-related part of the `map` service.
pub async fn map(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    match call.method.as_str() {
        "GetStationExtraInfo" => Some(Outcome::Value(ctx.store.read(station_extra_info))),
        _ => None,
    }
}

async fn station_item_bits(ctx: &Ctx<'_>) -> Value {
    let Some(station_id) = ctx.session.station_id else {
        return Value::List(vec![Value::None; 5]);
    };
    let line = ctx.store.read(|db| {
        let st = db.table("staStations").get_id(station_id)?;
        let j = joined(db, &st);
        Some(vec![Value::int_or_none(j.hangar), Value::int_or_none(j.owner), st.cell("stationID"), Value::Int(j.mask), st.cell("stationTypeID")])
    });
    match line {
        Some(line) => Value::List(line),
        None => {
            warn!("GetStationItemBits: no staStations row for {station_id}");
            Value::List(vec![Value::None; 5])
        }
    }
}

/// The players in the station, as `(charID, corpID, allianceID)`: Trinity's
/// station/base.py GetGuests unpacks each into three.
async fn guests(ctx: &Ctx<'_>) -> Value {
    let Some(station_id) = ctx.session.station_id else { return Value::List(Vec::new()) };
    ctx.store.read(|db| {
        let corporations = db.table("corporation");
        Value::List(
            db.table("character_")
                .find(&["stationID"], &[crate::store::Value::Int(station_id)])
                .filter(|c| c.int("accountID").is_some())
                .map(|c| {
                    let corp = c.int("corporationID");
                    let alliance = corp
                        .and_then(|corp| corporations.get_id(corp))
                        .and_then(|co| co.int("allianceID"))
                        .filter(|a| *a != 0);
                    Value::Tuple(vec![Value::int_or_none(c.int("characterID")), Value::int_or_none(corp), Value::int_or_none(alliance)])
                })
                .collect(),
        )
    })
}

fn get_station(db: &Db, station_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "stationID", "security", "dockingCostPerVolume", "maxShipVolumeDockable",
        "officeRentalCost", "operationID", "stationTypeID", "corporationID",
        "solarSystemID", "constellationID", "regionID", "stationName", "x", "y", "z",
        "reprocessingEfficiency", "reprocessingStationsTake", "reprocessingHangarFlag",
        "ownerID", "description", "serviceMask", "hangarGraphicID", "dockingBayGraphicID",
    ];
    let Some(st) = db.table("staStations").get_id(station_id) else { return Value::None };
    let j = joined(db, &st);
    let description = j
        .operation
        .and_then(|op| crate::sde::table("staOperations").get("operationID", op))
        .and_then(|op| op.text("description"))
        .unwrap_or("");
    let mut line = st.line(&HEADER[..18]);
    line.extend([
        Value::int_or_none(j.owner),
        Value::text(description),
        Value::Int(j.mask),
        Value::int_or_none(j.hangar),
        Value::int_or_none(j.bay),
    ]);
    row(HEADER, line)
}

fn get_solar_system(db: &Db, solar_system_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "solarSystemID", "solarSystemName", "regionID", "constellationID", "x", "y", "z",
        "luminosity", "border", "fringe", "corridor", "hub", "international", "regional",
        "constellation", "security", "factionID", "radius", "sunTypeID", "securityClass",
        "allianceID", "itemID", "sovereigntyLevel", "constellationSovereignty",
    ];
    let Some(r) = db.table("mapSolarSystems").get_id(solar_system_id) else { return Value::None };
    let mut line = r.line(&HEADER[..21]);
    // "itemID" is the system's ID again; the sovereignty level 0 when unset.
    line.push(r.cell("solarSystemID"));
    line.push(Value::Int(r.int("sovereigntyLevel").unwrap_or(0)));
    line.push(Value::None);
    row(HEADER, line)
}

fn stations_by_region(db: &Db, region_id: i64) -> Value {
    const HEADER: &[&str] = &["stationID", "stationName", "solarSystemID", "constellationID", "regionID", "x", "y", "z"];
    let mut stations: Vec<Row> = db.table("staStations").rows().filter(|r| r.int("regionID") == Some(region_id)).collect();
    // By name (unnamed last), then ID.
    stations.sort_by(|a, b| {
        let (x, y) = (a.text("stationName"), b.text("stationName"));
        (x.is_none(), x).cmp(&(y.is_none(), y)).then(a.int("stationID").cmp(&b.int("stationID")))
    });
    rowset(HEADER, stations.iter().map(|r| Value::List(r.line(HEADER))).collect())
}

fn get_constellation(constellation_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "constellationID", "constellationName", "regionID", "x", "y", "z",
        "xMin", "xMax", "yMin", "yMax", "zMin", "zMax", "factionID", "radius", "itemID",
    ];
    // The last column is "constellationID" again, as "itemID".
    const COLUMNS: &[&str] = &[
        "constellationID", "constellationName", "regionID", "x", "y", "z",
        "xMin", "xMax", "yMin", "yMax", "zMin", "zMax", "factionID", "radius", "constellationID",
    ];
    crate::sde::table("mapConstellations")
        .get("constellationID", constellation_id as i32 as i64)
        .map_or(Value::None, |r| row(HEADER, r.line(COLUMNS)))
}

fn get_region(region_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "regionID", "regionName", "x", "y", "z",
        "xMin", "xMax", "yMin", "yMax", "zMin", "zMax", "factionID", "radius", "itemID",
    ];
    // The last column is "regionID" again, as "itemID".
    const COLUMNS: &[&str] = &[
        "regionID", "regionName", "x", "y", "z",
        "xMin", "xMax", "yMin", "yMax", "zMin", "zMax", "factionID", "radius", "regionID",
    ];
    crate::sde::table("mapRegions")
        .get("regionID", region_id as i32 as i64)
        .map_or(Value::None, |r| row(HEADER, r.line(COLUMNS)))
}

fn alliance_constellations() -> Value {
    const HEADER: &[&str] = &["constellationID", "allianceID"];
    let mut held: Vec<_> = crate::sde::table("mapConstellations")
        .rows()
        .filter(|r| r.int("allianceID").is_some_and(|a| a != 0))
        .collect();
    held.sort_by_key(|r| r.int("constellationID"));
    rowset(HEADER, held.iter().map(|r| Value::List(r.line(HEADER))).collect())
}

fn alliance_systems(db: &Db) -> Value {
    const HEADER: &[&str] = &["solarSystemID", "allianceID"];
    let held = db.table("mapSolarSystems").rows().filter(|r| r.int("allianceID").is_some());
    rowset(HEADER, held.map(|r| Value::List(r.line(HEADER))).collect())
}

fn station_extra_info(db: &Db) -> Value {
    const STATIONS: &[&str] = &[
        "stationID", "operationID", "stationTypeID", "corporationID", "ownerID",
        "solarSystemID", "constellationID", "regionID", "serviceMask", "security",
        "officeRentalCost", "maxShipVolumeDockable", "dockingCostPerVolume",
        "reprocessingEfficiency", "reprocessingStationsTake", "reprocessingHangarFlag",
    ];
    const OPSERVICES: &[&str] = &["operationID", "serviceID"];
    const SERVICES: &[&str] = &["serviceID", "serviceName", "description"];

    let mut stations: Vec<Row> = db.table("staStations").rows().collect();
    stations.sort_by_key(|r| r.int("stationID"));
    let stations = stations
        .iter()
        .map(|st| {
            let j = joined(db, st);
            let mut line = st.line(&STATIONS[..4]);
            line.push(Value::int_or_none(j.owner));
            line.extend(st.line(&STATIONS[5..8]));
            line.push(Value::Int(j.mask));
            line.extend(st.line(&STATIONS[9..]));
            Value::List(line)
        })
        .collect();

    let mut opservices: Vec<_> = crate::sde::table("staOperationServices").rows().collect();
    opservices.sort_by_key(|r| (r.int("operationID"), r.int("serviceID")));
    let mut services: Vec<_> = crate::sde::table("staServices").rows().collect();
    services.sort_by_key(|r| r.int("serviceID"));

    Value::Tuple(vec![
        rowset(STATIONS, stations),
        rowset(OPSERVICES, opservices.iter().map(|r| Value::List(r.line(OPSERVICES))).collect()),
        rowset(SERVICES, services.iter().map(|r| Value::List(r.line(SERVICES))).collect()),
    ])
}

/// What a station's rows were joined with: its type's graphics
/// (staStationTypes), its operation (its own, else its type's) and that
/// operation's service mask (0 without services), and its owner (the
/// station item's, else its corporation).
struct Joined {
    hangar: Option<i64>,
    bay: Option<i64>,
    operation: Option<i64>,
    mask: i64,
    owner: Option<i64>,
}

fn joined(db: &Db, st: &Row) -> Joined {
    let stt = st.int("stationTypeID").and_then(|t| crate::sde::table("staStationTypes").get("stationTypeID", t));
    let operation = st.int("operationID").or_else(|| stt.and_then(|t| t.int("operationID")));
    let (operations, masks) = service_masks();
    let mask = operation
        .and_then(|op| operations.iter().position(|o| i64::from(*o) == op))
        .map_or(0, |i| i64::from(masks[i]));
    let owner = db.table("entity").get_id(st.int("stationID").unwrap_or(0)).and_then(|e| e.int("ownerID")).or(st.int("corporationID"));
    Joined {
        hangar: stt.and_then(|t| t.int("hangarGraphicID")),
        bay: stt.and_then(|t| t.int("dockingBayGraphicID")),
        operation,
        mask,
        owner,
    }
}

/// Each station operation's services as one mask (the `bit_or` of its
/// staOperationServices), as parallel lists of operations and masks. An
/// operation without services has no entry.
pub(crate) fn service_masks() -> (Vec<i32>, Vec<i32>) {
    let mut masks: std::collections::BTreeMap<i32, i32> = std::collections::BTreeMap::new();
    for r in crate::sde::table("staOperationServices").rows() {
        let (Some(operation), Some(service)) = (r.int("operationID"), r.int("serviceID")) else { continue };
        *masks.entry(operation as i32).or_insert(0) |= service as i32;
    }
    masks.into_iter().unzip()
}
