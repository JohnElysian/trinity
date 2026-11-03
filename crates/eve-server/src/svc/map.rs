use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{cell, rowset};

pub async fn map(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetSolarSystemPseudoSecurities" => pseudo_securities(ctx).await,
        "GetStuckSystems" => stuck_systems(),
        "GetHistory" => history(ctx, call.arg_id(0), call.arg_i64(1).unwrap_or(1)).await,
        "GetSolarSystemVisits" => solar_system_visits(ctx).await,
        "GetStationCount" => station_count(ctx).await,
        "GetDeadspaceComplexMap" => deadspace_complex_map(ctx).await,
        "GetBeaconCount" => crate::svc::jumps::beacon_count(ctx).await,
        "GetAllianceBeacons" => alliance_beacons(ctx).await,
        "GetLinkableJumpArrays" => super::bridges::linkable(ctx).await,
        "GetMyExtraMapInfo" => my_extra_map_info(),
        "GetMyExtraMapInfoAgents" => my_extra_map_info_agents(ctx).await,
        "GetSolarSystemLoadRatios" => load_ratios(),

        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn alliance_beacons(ctx: &Ctx<'_>) -> Value {
    let Some(alliance) = ctx.session.alliance_id.filter(|id| *id > 0) else {
        return Value::List(vec![]);
    };
    // Capital navigation unpacks (solarSystemID, structureID, structureTypeID).
    let sql = r#"SELECT e."locationID",e."itemID",e."typeID" FROM entity e
        JOIN "invTypes" t ON t."typeID"=e."typeID"
        JOIN "posStructures" p ON p."itemID"=e."itemID"
        JOIN corporation c ON c."corporationID"=e."ownerID"
        WHERE c."allianceID"=$1 AND t."groupID"=838 AND p.state IN (4,5)
        ORDER BY e."locationID",e."itemID""#;
    Value::List(
        ctx.query(sql, &[&(alliance as i32)])
            .await
            .iter()
            .map(|r| Value::Tuple(crate::svc::rows::line(r)))
            .collect(),
    )
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

pub async fn ensure(db: &tokio_postgres::Client) -> Result<(), tokio_postgres::Error> {
    db.batch_execute(
        r#"CREATE TABLE IF NOT EXISTS "chrSystemVisits" (
        "characterID" integer NOT NULL,"solarSystemID" integer NOT NULL,
        visits integer NOT NULL DEFAULT 1,"lastDateTime" bigint NOT NULL,
        PRIMARY KEY ("characterID","solarSystemID"));
        CREATE TABLE IF NOT EXISTS "mapActivity" (
        "solarSystemID" integer NOT NULL,kind smallint NOT NULL,"eventTime" bigint NOT NULL);
        CREATE INDEX IF NOT EXISTS "mapActivity_time" ON "mapActivity" ("eventTime");"#,
    )
    .await
}

pub async fn record_visit(
    db: &tokio_postgres::Client,
    char_id: i64,
    system: i64,
    arrived: bool,
) -> Result<(), tokio_postgres::Error> {
    db.execute(r#"INSERT INTO "chrSystemVisits" ("characterID","solarSystemID","lastDateTime") VALUES ($1,$2,$3)
        ON CONFLICT ("characterID","solarSystemID") DO UPDATE SET
        visits="chrSystemVisits".visits + CASE WHEN $4 THEN 1 ELSE 0 END,"lastDateTime"=$3"#,
        &[&(char_id as i32),&(system as i32),&crate::bulkdata::filetime_now(),&arrived]).await?;
    Ok(())
}

/// kind 1 is a jump; kind 2 an NPC ship destroyed.
pub async fn record_activity(
    db: &tokio_postgres::Client,
    system: i64,
    kind: i16,
) -> Result<(), tokio_postgres::Error> {
    db.execute(
        r#"INSERT INTO "mapActivity" ("solarSystemID",kind,"eventTime") VALUES ($1,$2,$3)"#,
        &[&(system as i32), &kind, &crate::bulkdata::filetime_now()],
    )
    .await?;
    Ok(())
}

async fn history(ctx: &Ctx<'_>, kind: i64, hours: i64) -> Value {
    let header = &["solarSystemID", "value1", "value2", "value3"];
    let since = crate::bulkdata::filetime_now() - hours.clamp(1, 24) * 36_000_000_000;
    match kind {
        1 => ctx.rowset(header,r#"SELECT "solarSystemID",count(*)::integer,0,0 FROM "mapActivity"
            WHERE kind=1 AND "eventTime">=$1 GROUP BY "solarSystemID""#,&[&since]).await,
        3 => ctx.rowset(header,r#"SELECT "solarSystemID",sum(ships)::integer,sum(npcs)::integer,sum(pods)::integer FROM (
            SELECT "solarSystemID",count(*) FILTER (WHERE "isPodKill"=0) AS ships,0::bigint AS npcs,
                count(*) FILTER (WHERE "isPodKill"<>0) AS pods FROM "killLog" WHERE "killTime">=$1 GROUP BY "solarSystemID"
            UNION ALL SELECT "solarSystemID",count(*),count(*),0::bigint FROM "mapActivity"
                WHERE kind=2 AND "eventTime">=$1 GROUP BY "solarSystemID") h GROUP BY "solarSystemID""#,&[&since]).await,
        _ => rowset(header,vec![]),
    }
}

async fn solar_system_visits(ctx: &Ctx<'_>) -> Value {
    ctx.rowset(&["solarSystemID", "visits", "lastDateTime"],
        r#"SELECT "solarSystemID",visits,"lastDateTime" FROM "chrSystemVisits" WHERE "characterID"=$1 ORDER BY "solarSystemID""#,
        &[&(ctx.session.char_id.unwrap_or(0) as i32)]).await
}

async fn station_count(ctx: &Ctx<'_>) -> Value {
    let sql = r#"SELECT "solarSystemID", count(*)::int AS "count"
                   FROM "staStations" GROUP BY "solarSystemID""#;
    match ctx.db.query(sql, &[]).await {
        Ok(rows) => Value::List(
            rows.iter()
                .map(|r| Value::Tuple(vec![cell(r, 0), cell(r, 1)]))
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
            .map(|(system, id, level, name)| {
                Value::Tuple(vec![
                    Value::Int(system),
                    Value::Int(id),
                    Value::Int(level),
                    Value::text(name),
                ])
            })
            .collect(),
    )
}

fn my_extra_map_info() -> Value {
    Value::Tuple(vec![Value::None, Value::None])
}

async fn my_extra_map_info_agents(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["fromID", "rank"];
    let Some(char_id) = ctx.session.char_id else {
        return rowset(HEADER, Vec::new());
    };
    let sql = r#"SELECT "fromID", "standing" / 10.0 AS "rank"
                   FROM "chrNPCStandings" WHERE "characterID" = $1"#;
    ctx.rowset(HEADER, sql, &[&(char_id as i32)]).await
}

fn load_ratios() -> Value {
    rowset(&["solarSystemID", "loadRatio"], Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lines(value: &Value) -> &[Value] {
        let Value::Instance { state, .. } = value else {
            panic!("rowset")
        };
        let Value::Dict(fields) = &**state else {
            panic!("state")
        };
        fields
            .iter()
            .find(|(key, _)| key.as_string().as_deref() == Some("lines"))
            .unwrap()
            .1
            .seq()
            .unwrap()
    }
    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE pointing at a disposable imported database"]
    async fn visits_persist_and_map_history_separates_npcs_ships_and_pods() {
        let fixture = crate::test_support::Database::load().await;
        let pilot = fixture.id().await as i64;
        let system = fixture.id().await as i64;
        let db = &fixture.db;
        record_visit(db, pilot, system, false).await.unwrap();
        record_visit(db, pilot, system, false).await.unwrap();
        record_visit(db, pilot, system, true).await.unwrap();
        let session = crate::session::Session {
            char_id: Some(pilot),
            ..Default::default()
        };
        let ctx = fixture.ctx(&session);
        let visited = solar_system_visits(&ctx).await;
        assert_eq!(
            lines(&visited)[0].seq().unwrap()[1].as_i64(),
            Some(2),
            "relogin does not count as arrival"
        );
        record_activity(db, system, 1).await.unwrap();
        record_activity(db, system, 2).await.unwrap();
        for pod in [0i16, 1] {
            db.execute(r#"INSERT INTO "killLog" ("killTime","victimCharID","victimCorpID","victimAllianceID","victimShipTypeID","victimItemID","solarSystemID","locationID","finalBlowID","totalDamage","isPodKill")
                VALUES ($1,$2,0,0,670,0,$3,$3,0,1,$4)"#,&[&crate::bulkdata::filetime_now(),&(pilot as i32),&(system as i32),&pod]).await.unwrap();
        }
        let killed = history(&ctx, 3, 24).await;
        let row = lines(&killed)
            .iter()
            .find(|r| r.seq().unwrap()[0].as_i64() == Some(system))
            .unwrap()
            .seq()
            .unwrap();
        assert_eq!(
            row.iter()
                .skip(1)
                .map(|v| v.as_i64().unwrap())
                .collect::<Vec<_>>(),
            vec![2, 1, 1]
        );
        let jumped = history(&ctx, 1, 1).await;
        let row = lines(&jumped)
            .iter()
            .find(|r| r.seq().unwrap()[0].as_i64() == Some(system))
            .unwrap()
            .seq()
            .unwrap();
        assert_eq!(row[1].as_i64(), Some(1));
        db.execute(
            r#"UPDATE "mapActivity" SET "eventTime"=0 WHERE "solarSystemID"=$1"#,
            &[&(system as i32)],
        )
        .await
        .unwrap();
        assert!(!lines(&history(&ctx, 1, 1).await)
            .iter()
            .any(|r| r.seq().unwrap()[0].as_i64() == Some(system)));
        db.execute(
            r#"DELETE FROM "mapActivity" WHERE "solarSystemID"=$1"#,
            &[&(system as i32)],
        )
        .await
        .unwrap();
        db.execute(
            r#"DELETE FROM "chrSystemVisits" WHERE "characterID"=$1"#,
            &[&(pilot as i32)],
        )
        .await
        .unwrap();
        db.execute(
            r#"DELETE FROM "killLog" WHERE "victimCharID"=$1"#,
            &[&(pilot as i32)],
        )
        .await
        .unwrap();
    }
}
