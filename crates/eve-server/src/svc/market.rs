use std::collections::BTreeMap;

use eve_marshal::Value;
use tokio_postgres::Row;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{cell, row, rowset_with_columns};

pub async fn market_proxy(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if matches!(call.method.as_str(), "StartupCheck" | "GetOrders" | "GetCharOrders" | "GetCorporationOrders"
        | "GetStationAsks" | "GetSystemAsks" | "GetRegionBest" | "PlaceCharOrder" | "CancelCharOrder" | "ModifyCharOrder") {
        let result = crate::svc::trade::expire(ctx).await;
        if !matches!(result, Outcome::Value(_)) { return Some(result); }
    }
    let arg0 = || call.arg_id(0);

    let out = match call.method.as_str() {
        "StartupCheck" => {
            if ctx.session.char_id.is_none() || ctx.session.region_id.is_none() {
                Outcome::custom("Select a character in a region before opening the market.")
            } else {
                match ctx.db.query_one("SELECT EXISTS(SELECT 1 FROM market_orders)", &[]).await {
                    Ok(_) => Outcome::Value(Value::None),
                    Err(error) => {
                        error!("market StartupCheck: {}", crate::logging::chain(&error));
                        Outcome::custom("The market is temporarily unavailable. Please try again.")
                    }
                }
            }
        }
        "GetMarketGroups" => Outcome::Value(market_groups(ctx).await),

        "GetOrders" => cached_orders(orders_for_type(ctx, arg0()).await),

        "GetOldPriceHistory" => {
            Outcome::Value(history_rows(Vec::new()))
        }
        "GetNewPriceHistory" => Outcome::Value(price_history(ctx, arg0()).await),

        "GetStationAsks" => Outcome::Value(best_asks(ctx, Scope::Station).await),
        "GetSystemAsks" => Outcome::Value(best_asks(ctx, Scope::SolarSystem).await),
        "GetRegionBest" => Outcome::Value(best_asks(ctx, Scope::Region).await),

        "GetCharOrders" => Outcome::Value(char_orders(ctx).await),
        "GetCorporationOrders" => Outcome::Value(corporation_orders(ctx).await),

        "PlaceCharOrder" | "CancelCharOrder" | "ModifyCharOrder" => {
            return crate::svc::trade::market_write(call, ctx).await;
        }
        "GetCharSkillLimits" => {
            Outcome::Unknown("marketProxy::GetCharSkillLimits has no client call site".into())
        }

        _ => return None,
    };
    Some(out)
}

fn cached_orders(value: Value) -> Outcome {
    let table=eve_marshal::StringTable::default();
    let bytes=match eve_marshal::save(&value,&table) {
        Ok(bytes)=>bytes,
        Err(error)=>{error!("serializing market orders: {error}");return Outcome::custom("Market orders could not be loaded.");}
    };
    // The original marketQuote compares the cached result's checksum before
    // replacing its local order list. Raw RPC results leave that version at 0.
    let (mut a,mut b)=(1u32,0u32);
    for byte in &bytes {a=(a+u32::from(*byte))%65521;b=(b+a)%65521;}
    Outcome::Value(Value::Instance {
        class:Box::new(Value::bytes("objectCaching.CachedMethodCallResult")),
        state:Box::new(Value::Tuple(vec![
            Value::Dict(vec![(Value::bytes("versionCheck"),Value::bytes("always")),
                (Value::bytes("sessionInfo"),Value::bytes("regionid"))]),
            Value::Bytes(bytes),
            Value::List(vec![Value::Long(crate::bulkdata::filetime_now()),Value::Int(((b<<16)|a) as i32 as i64)]),
        ])),
    })
}



const GROUP_HEADER: &[&str] = &[
    "marketGroupID",
    "parentGroupID",
    "marketGroupName",
    "description",
    "graphicID",
    "hasTypes",
    "types",
];

async fn market_groups(ctx: &Ctx<'_>) -> Value {
    let types_sql = r#"WITH RECURSIVE "sub"("rootID","nodeID","depth") AS (
                           SELECT "marketGroupID","marketGroupID",0 FROM "invMarketGroups"
                         UNION ALL
                           SELECT s."rootID", g."marketGroupID", s."depth" + 1
                             FROM "sub" s
                             JOIN "invMarketGroups" g ON g."parentGroupID" = s."nodeID"
                            WHERE s."depth" < 12
                       )
                       SELECT s."rootID", t."typeID"
                         FROM "sub" s
                         JOIN "invTypes" t ON t."marketGroupID" = s."nodeID"
                        ORDER BY s."rootID", t."typeID""#;

    let mut types_by_group: BTreeMap<i32, Vec<Value>> = BTreeMap::new();
    match ctx.db.query(types_sql, &[]).await {
        Ok(rows) => {
            for r in &rows {
                let group: i32 = r.get(0);
                let type_id: i32 = r.get(1);
                types_by_group.entry(group).or_default().push(Value::Int(type_id as i64));
            }
        }
        Err(e) => error!("GetMarketGroups types: {err}", err = crate::logging::chain(&e)),
    }

    let groups_sql = r#"SELECT "marketGroupID","parentGroupID","marketGroupName","description",
                               "graphicID",COALESCE("hasTypes",0)
                          FROM "invMarketGroups" ORDER BY "marketGroupID""#;
    let rows = match ctx.db.query(groups_sql, &[]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("GetMarketGroups: {err}", err = crate::logging::chain(&e));
            return Value::Dict(vec![(Value::None, rowset_with_columns(GROUP_HEADER, Vec::new()))]);
        }
    };

    let mut buckets: BTreeMap<Option<i32>, Vec<Value>> = BTreeMap::new();
    buckets.entry(None).or_default();
    for r in &rows {
        let group_id: i32 = r.get(0);
        let parent: Option<i32> = r.get(1);
        let line = Value::List(vec![
            Value::Int(group_id as i64),
            parent.map(|p| Value::Int(p as i64)).unwrap_or(Value::None),
            cell(r, 2),
            r.get::<_, Option<String>>(3).map(|d| Value::Bytes(d.into_bytes())).unwrap_or(Value::None),
            cell(r, 4),
            cell(r, 5),
            Value::List(types_by_group.remove(&group_id).unwrap_or_default()),
        ]);
        buckets.entry(parent).or_default().push(line);
        buckets.entry(Some(group_id)).or_default();
    }

    Value::Dict(
        buckets
            .into_iter()
            .map(|(parent, lines)| {
                let key = parent.map(|p| Value::Int(p as i64)).unwrap_or(Value::None);
                (key, rowset_with_columns(GROUP_HEADER, lines))
            })
            .collect(),
    )
}

pub const ORDER_HEADER: &[&str] = &[
    "orderID",
    "typeID",
    "charID",
    "regionID",
    "stationID",
    "range",
    "bid",
    "price",
    "volEntered",
    "volRemaining",
    "issued",
    "orderState",
    "minVolume",
    "contraband",
    "accountKey",
    "duration",
    "isCorp",
    "solarSystemID",
    "escrow",
    "jumps",
    "constellationID",
];

pub const ORDER_SELECT: &str = r#"SELECT o."orderID",o."typeID",o."charID",o."regionID",o."stationID",
                                     o."range",o."bid",o."price",o."volEntered",o."volRemaining",
                                     o."issued",o."orderState",o."minVolume",o."contraband",
                                     o."accountID",o."duration",o."isCorp",o."solarSystemID",
                                     o."escrow",o."jumps",s."constellationID"
                                FROM "market_orders" o
                                LEFT JOIN "mapSolarSystems" s
                                       ON s."solarSystemID" = o."solarSystemID""#;

pub fn order_line(r: &Row) -> Vec<Value> {
    vec![
        cell(r, 0),
        cell(r, 1),
        cell(r, 2),
        cell(r, 3),
        cell(r, 4),
        cell(r, 5),
        cell(r, 6),
        cell(r, 7),
        cell(r, 8),
        cell(r, 9),
        cell(r, 10),
        cell(r, 11),
        cell(r, 12),
        cell(r, 13),
        cell(r, 14),
        cell(r, 15),
        cell(r, 16),
        cell(r, 17),
        cell(r, 18),
        cell(r, 19),
        cell(r, 20),
    ]
}

fn order_rows(lines: Vec<Value>) -> Value {
    use eve_marshal::dbrow::{dbtype,DbRow,DbRowDescriptor};
    let descriptor=std::sync::Arc::new(DbRowDescriptor::new(ORDER_HEADER.iter().map(|name| {
        let kind=match *name {"price"|"escrow"=>dbtype::R8,"issued"=>dbtype::FILETIME,_=>dbtype::I4};
        (name.as_bytes().to_vec(),kind)
    }).collect()));
    let rows=lines.into_iter().map(|line| {
        let Value::List(values)=line else {unreachable!("order line")};
        Value::DbRow(Box::new(DbRow{descriptor:descriptor.clone(),values}))
    }).collect();
    // GetBestMatchableBid appends these rows to dbutil.RowList, which rejects
    // util.Row. Native rows also provide __keys__, mutable cells and columns.
    Value::reduce("dbutil.RowList",vec![Value::Tuple(vec![descriptor.to_value(),Value::List(rows)])])
}

async fn orders_for_type(ctx: &Ctx<'_>, type_id: i64) -> Value {
    let empty =
        || Value::Tuple(vec![order_rows(Vec::new()), order_rows(Vec::new())]);
    let Some(region_id) = ctx.session.region_id else { return empty() };

    let sql = format!(
        r#"{sel} WHERE o."typeID" = $1 AND o."regionID" = $2 AND o."orderState" = 0
                   AND o."volRemaining" > 0
                 ORDER BY o."price", o."issued""#,
        sel = ORDER_SELECT
    );
    let rows = match ctx.db.query(sql.as_str(), &[&(type_id as i32), &(region_id as i32)]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("GetOrders({type_id}): {err}", err = crate::logging::chain(&e));
            return empty();
        }
    };

    let (mut asks, mut bids) = (Vec::new(), Vec::new());
    for r in &rows {
        let bid: i16 = r.get(6);
        let line = Value::List(order_line(r));
        if bid == 0 { asks.push(line) } else { bids.push(line) }
    }
    Value::Tuple(vec![order_rows(asks), order_rows(bids)])
}

async fn char_orders(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return order_rows(Vec::new()) };
    let sql = format!(
        r#"{sel} WHERE o."charID" = $1 AND o."orderState" = 0 ORDER BY o."issued" DESC"#,
        sel = ORDER_SELECT
    );
    match ctx.db.query(sql.as_str(), &[&(char_id as i32)]).await {
        Ok(rows) => {
            order_rows(rows.iter().map(|r| Value::List(order_line(r))).collect())
        }
        Err(e) => {
            error!("GetCharOrders: {err}", err = crate::logging::chain(&e));
            order_rows(Vec::new())
        }
    }
}

async fn corporation_orders(ctx: &Ctx<'_>) -> Value {
    let Some(corp_id) = ctx.session.corp_id else { return order_rows(Vec::new()) };
    let sql = format!(
        r#"{sel}
                 WHERE o."isCorp" = 1 AND o."orderState" = 0 AND o."walletOwnerID" = $1 AND o."charID" >= 100000000
                 ORDER BY o."issued" DESC"#,
        sel = ORDER_SELECT
    );
    match ctx.db.query(sql.as_str(), &[&(corp_id as i32)]).await {
        Ok(rows) => {
            order_rows(rows.iter().map(|r| Value::List(order_line(r))).collect())
        }
        Err(e) => {
            error!("GetCorporationOrders: {err}", err = crate::logging::chain(&e));
            order_rows(Vec::new())
        }
    }
}

enum Scope {
    Station,
    SolarSystem,
    Region,
}

async fn best_asks(ctx: &Ctx<'_>, scope: Scope) -> Value {
    let (column, scope_id) = match scope {
        Scope::Station => ("stationID", ctx.session.station_id),
        Scope::SolarSystem => ("solarSystemID", ctx.session.solar_system_id2),
        Scope::Region => ("regionID", ctx.session.region_id),
    };
    let Some(scope_id) = scope_id else { return Value::Dict(Vec::new()) };

    let sql = format!(
        r#"SELECT DISTINCT ON (q."typeID") q.* FROM (
               {sel} WHERE o."bid" = 0 AND o."orderState" = 0 AND o."volRemaining" > 0
                       AND o."{col}" = $1
           ) q ORDER BY q."typeID", q."price", q."issued""#,
        sel = ORDER_SELECT,
        col = column
    );
    match ctx.db.query(sql.as_str(), &[&(scope_id as i32)]).await {
        Ok(rows) => Value::Dict(
            rows.iter()
                .map(|r| {
                    let type_id: i32 = r.get(1);
                    (Value::Int(type_id as i64), row(ORDER_HEADER, order_line(r)))
                })
                .collect(),
        ),
        Err(e) => {
            error!("best asks ({column}): {err}", err = crate::logging::chain(&e));
            Value::Dict(Vec::new())
        }
    }
}


fn history_rows(lines: Vec<Vec<Value>>) -> Value {
    use eve_marshal::dbrow::{dbtype, DbRow, DbRowDescriptor};
    let descriptor = std::sync::Arc::new(DbRowDescriptor::of(&[
        ("historyDate", dbtype::FILETIME), ("lowPrice", dbtype::R8),
        ("highPrice", dbtype::R8), ("avgPrice", dbtype::R8),
        ("volume", dbtype::I8), ("orders", dbtype::I4),
    ]));
    let rows = lines.into_iter().map(|values| Value::DbRow(Box::new(DbRow {
        descriptor: descriptor.clone(), values,
    }))).collect();
    // marketsvc.GetPriceHistory constructs a RowList from new.header and
    // appends blue.DBRows; a util.Rowset or bare tuple is not compatible.
    Value::reduce("dbutil.RowList", vec![Value::Tuple(vec![descriptor.to_value(), Value::List(rows)])])
}

async fn price_history(ctx: &Ctx<'_>, type_id: i64) -> Value {
    let Some(region) = ctx.session.region_id else { return history_rows(vec![]) };
    match ctx.db.query(r#"SELECT "historyDate","lowPrice","highPrice","avgPrice","volume","orders"
        FROM market_history WHERE "regionID"=$1 AND "typeID"=$2 ORDER BY "historyDate""#,
        &[&(region as i32), &(type_id as i32)]).await {
        Ok(rows) => history_rows(rows.iter().map(crate::svc::rows::line).collect()),
        Err(error) => { error!("market history: {}", crate::logging::chain(&error)); history_rows(vec![]) }
    }
}
