use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{cell, rowset};

pub async fn map(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetSolarSystemPseudoSecurities" => pseudo_securities(ctx).await,
        "GetStuckSystems" => stuck_systems(),
        "GetHistory" => history(),
        "GetSolarSystemVisits" => solar_system_visits(),
        "GetStationCount" => station_count(ctx).await,
        "GetDeadspaceComplexMap" => deadspace_complex_map(ctx).await,
        "GetBeaconCount" => crate::svc::jumps::beacon_count(ctx.world),
        "GetMyExtraMapInfo" => my_extra_map_info(),
        "GetMyExtraMapInfoAgents" => my_extra_map_info_agents(ctx).await,
        "GetSolarSystemLoadRatios" => load_ratios(),

        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn pseudo_securities(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["solarSystemID", "security"];
    let sql = r#"SELECT "solarSystemID", "security"
                   FROM "mapSolarSystems" WHERE "security" IS NOT NULL"#;
    ctx.rowset(HEADER, sql, &[]).await
}

fn stuck_systems() -> Value {
    Value::Dict(Vec::new())
}

fn history() -> Value {
    rowset(&["solarSystemID", "value1", "value2", "value3"], Vec::new())
}

fn solar_system_visits() -> Value {
    rowset(&["solarSystemID", "visits", "lastDateTime"], Vec::new())
}

async fn station_count(ctx: &Ctx<'_>) -> Value {
    let sql = r#"SELECT "solarSystemID", count(*)::int AS "count"
                   FROM "staStations" GROUP BY "solarSystemID""#;
    match ctx.db.query(sql, &[]).await {
        Ok(rows) => Value::List(
            rows.iter()
                .map(|r| {
                    Value::Tuple(vec![
                        cell(r, 0),
                        cell(r, 1),
                    ])
                })
                .collect(),
        ),
        Err(e) => {
            error!("GetStationCount: {err}", err = crate::logging::chain(&e));
            Value::List(Vec::new())
        }
    }
}

async fn deadspace_complex_map(ctx: &Ctx<'_>) -> Value {
    let found = crate::complexes::map(ctx.db).await;
    Value::List(
        found
            .into_iter()
            .map(|(system, id, level, name)| Value::Tuple(vec![Value::Int(system), Value::Int(id), Value::Int(level), Value::text(name)]))
            .collect(),
    )
}

fn my_extra_map_info() -> Value {
    Value::Tuple(vec![Value::None, Value::None])
}

async fn my_extra_map_info_agents(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["fromID", "rank"];
    let Some(char_id) = ctx.session.char_id else { return rowset(HEADER, Vec::new()) };
    let sql = r#"SELECT "fromID", "standing" / 10.0 AS "rank"
                   FROM "chrNPCStandings" WHERE "characterID" = $1"#;
    ctx.rowset(HEADER, sql, &[&(char_id as i32)]).await
}

fn load_ratios() -> Value {
    rowset(&["solarSystemID", "loadRatio"], Vec::new())
}
