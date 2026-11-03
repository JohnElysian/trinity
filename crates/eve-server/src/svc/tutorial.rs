use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{key_val, lines, rowset};

pub async fn tutorial_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg = |i: usize| call.arg_i64(i);
    let value = match call.method.as_str() {
        "GetTutorials" => get_tutorials(ctx).await,
        "GetCriterias" => get_criterias(ctx).await,
        "GetTutorialInfo" => tutorial_info(ctx, arg(0).unwrap_or(0)).await,
        "GetTutorialVersionInfo" => tutorial_info(ctx, arg(0).unwrap_or(0)).await,

        "LogAppClosed" | "LogClosed" | "LogAborted" | "LogCompleted" => return Some(log_progress(call,ctx).await),
        "GetCharacterTutorialState" => ctx.rowset(&["tutorialID","pageID","eventTypeID"],
            r#"SELECT "tutorialID","pageID","eventTypeID" FROM "chrTutorialState" WHERE "characterID"=$1 ORDER BY "updatedAt","tutorialID""#,
            &[&(ctx.session.char_id.unwrap_or(0) as i32)]).await,

        _ => return None,
    };
    Some(Outcome::Value(value))
}

pub async fn ensure(db:&tokio_postgres::Client) -> Result<(),tokio_postgres::Error> {
    db.batch_execute(r#"CREATE TABLE IF NOT EXISTS "chrTutorialState" (
        "characterID" integer NOT NULL,"tutorialID" integer NOT NULL,"pageID" integer NOT NULL,
        "eventTypeID" integer NOT NULL,"elapsedSeconds" bigint NOT NULL,"updatedAt" bigint NOT NULL,
        PRIMARY KEY ("characterID","tutorialID"))"#).await
}

async fn log_progress(call:&Call,ctx:&Ctx<'_>) -> Outcome {
    let Some(character)=ctx.session.char_id else {return Outcome::custom("Select a character first.")};
    let Some(tutorial)=call.arg_i64(0).and_then(|id|i32::try_from(id).ok()).filter(|id|*id>0) else {return Outcome::custom("Invalid tutorial.")};
    let Some(page)=call.arg_i64(1).and_then(|id|i32::try_from(id).ok()).filter(|id|*id>=0) else {return Outcome::custom("Invalid tutorial page.")};
    let elapsed=call.arg_i64(2).unwrap_or(0).max(0);
    // tutorialsvc.py: 155 finishes a sequence, 158 disables tutorials. The
    // original event IDs for both resumable close events are unavailable;
    // 0 represents their shared client-visible nonterminal behavior.
    let event=match call.method.as_str(){"LogCompleted"=>155i32,"LogAborted"=>158,_=>0};
    let result=ctx.db.execute(r#"INSERT INTO "chrTutorialState" ("characterID","tutorialID","pageID","eventTypeID","elapsedSeconds","updatedAt")
        VALUES ($1,$2,$3,$4,$5,$6) ON CONFLICT ("characterID","tutorialID") DO UPDATE
        SET "pageID"=$3,"eventTypeID"=CASE WHEN "chrTutorialState"."eventTypeID" IN (155,158) AND $4=0
            THEN "chrTutorialState"."eventTypeID" ELSE $4 END,"elapsedSeconds"=$5,"updatedAt"=$6"#,
        &[&(character as i32),&tutorial,&page,&event,&elapsed,&crate::bulkdata::filetime_now()]).await;
    match result {
        Ok(_)=>Outcome::Value(Value::None),
        Err(e)=>{error!("saving tutorial progress: {}",crate::logging::chain(&e));Outcome::custom("Tutorial progress could not be saved.")}
    }
}

/// Trinity tutorial IDs this data has no tutorial for, and the tutorial whose
/// content stands in. Trinity starts new pilots at tutorial 109 (uix.tutorial);
/// the data here is the 2006 sequence, which starts at 5 (the station Aura
/// tutorial), so 109 shows 5's pages and continues where 5 does.
const ALIASES: &[(i32, i32)] = &[(109, 5)];

fn alias_source(id: i32) -> i32 {
    ALIASES.iter().find(|(a, _)| *a == id).map_or(id, |(_, s)| *s)
}

async fn get_tutorials(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["tutorialID", "tutorialVName", "nextTutorialID"];
    let sql = r#"SELECT "tutorialID","tutorialVName","nextTutorialID" FROM tutorials
                 UNION ALL
                 SELECT a.alias, t."tutorialVName", t."nextTutorialID"
                   FROM unnest($1::integer[], $2::integer[]) AS a(alias, source)
                   JOIN tutorials t ON t."tutorialID" = a.source
                  WHERE NOT EXISTS (SELECT 1 FROM tutorials x WHERE x."tutorialID" = a.alias)
                 ORDER BY 1"#;
    let aliases: Vec<i32> = ALIASES.iter().map(|(a, _)| *a).collect();
    let sources: Vec<i32> = ALIASES.iter().map(|(_, s)| *s).collect();
    ctx.rowset(HEADER, sql, &[&aliases, &sources]).await
}

async fn get_criterias(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["criteriaID", "criteriaName", "messageText", "audioPath"];
    let sql = r#"SELECT "criteriaID","criteriaName","messageText","audioPath"
                   FROM tutorial_criteria ORDER BY "criteriaID""#;
    ctx.rowset(HEADER, sql, &[]).await
}

async fn tutorial_info(ctx: &Ctx<'_>, tutorial_id: i64) -> Value {
    const TUTORIAL_HEADER: &[&str] = &["tutorialID", "tutorialVName", "nextTutorialID"];
    const PAGE_HEADER: &[&str] =
        &["pageID", "pageNumber", "pageName", "text", "imagePath", "audioPath"];
    const CRITERIA_HEADER: &[&str] = &["criteriaID"];
    const PAGE_CRITERIA_HEADER: &[&str] = &["pageID", "criteriaID"];

    // An aliased tutorial shows its source's pages under its own ID.
    let asked = tutorial_id as i32;
    let id = if ctx.query_one(r#"SELECT 1 FROM tutorials WHERE "tutorialID"=$1"#,&[&asked]).await.is_some() {
        asked
    } else {alias_source(asked)};

    let tutorial_sql = r#"SELECT $2::integer,"tutorialVName","nextTutorialID"
                            FROM tutorials WHERE "tutorialID" = $1"#;
    let tutorial_rows = match ctx.db.query(tutorial_sql, &[&id, &asked]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("GetTutorialInfo({tutorial_id}): {err}", err = crate::logging::chain(&e));
            return Value::None;
        }
    };
    if tutorial_rows.is_empty() {
        return Value::None;
    }

    let page_sql = r#"SELECT "pageID","pageNumber","pageName","text","imagePath","audioPath"
                        FROM tutorial_pages WHERE "tutorialID" = $1 ORDER BY "pageNumber""#;

    let criteria_sql = r#"SELECT "criteriaID" FROM tutorials_criterias
                           WHERE "tutorialID" = $1 ORDER BY "criteriaID""#;

    let page_criteria_sql = r#"SELECT pc."pageID", pc."criteriaID"
                                 FROM tutorial_page_criteria pc
                                 JOIN tutorial_pages p ON p."pageID" = pc."pageID"
                                WHERE p."tutorialID" = $1
                                ORDER BY p."pageNumber", pc."criteriaID""#;

    let pages = ctx.rowset(PAGE_HEADER, page_sql, &[&id]).await;
    let criterias = ctx.rowset(CRITERIA_HEADER, criteria_sql, &[&id]).await;
    let pagecriterias =
        ctx.rowset(PAGE_CRITERIA_HEADER, page_criteria_sql, &[&id]).await;

    key_val(vec![
        ("tutorial", rowset(TUTORIAL_HEADER, lines(&tutorial_rows))),
        ("pages", pages),
        ("criterias", criterias),
        ("pagecriterias", pagecriterias),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn progress_survives_reconnect_and_close_preserves_completion() {
        let fixture = crate::test_support::Database::load().await;
        let character = fixture.id().await;
        let session = crate::session::Session { char_id: Some(character as i64), ..Default::default() };
        let ctx = fixture.ctx(&session);
        let call = |method: &str, tutorial, page| Call {
            service: "tutorialSvc".into(), method: method.into(),
            args: vec![Value::Int(tutorial), Value::Int(page), Value::Int(47)], kwargs: vec![],
        };
        for (method, tutorial, page) in [("LogClosed", 5, 2), ("LogCompleted", 7, 8), ("LogAppClosed", 7, 8), ("LogAborted", 9, 1)] {
            assert!(matches!(tutorial_svc(&call(method, tutorial, page), &ctx).await, Some(Outcome::Value(Value::None))));
        }
        assert!(matches!(tutorial_svc(&call("LogClosed", 5, -1), &ctx).await, Some(Outcome::UserError { .. })));
        let connection = crate::db::connect(&fixture.url).await.unwrap();
        let rows = connection.query(r#"SELECT "tutorialID","pageID","eventTypeID","elapsedSeconds" FROM "chrTutorialState" WHERE "characterID"=$1 ORDER BY "tutorialID""#, &[&character]).await.unwrap();
        let saved: Vec<(i32, i32, i32, i64)> = rows.iter().map(|r| (r.get(0), r.get(1), r.get(2), r.get(3))).collect();
        assert_eq!(saved, vec![(5, 2, 0, 47), (7, 8, 155, 47), (9, 1, 158, 47)]);
        let read = Call { service: "tutorialSvc".into(), method: "GetCharacterTutorialState".into(), args: vec![], kwargs: vec![] };
        let expected = rowset(&["tutorialID", "pageID", "eventTypeID"], vec![
            Value::List(vec![Value::Int(5), Value::Int(2), Value::Int(0)]),
            Value::List(vec![Value::Int(7), Value::Int(8), Value::Int(155)]),
            Value::List(vec![Value::Int(9), Value::Int(1), Value::Int(158)]),
        ]);
        assert!(matches!(tutorial_svc(&read, &ctx).await, Some(Outcome::Value(value)) if value == expected));
        let other = crate::session::Session { char_id: Some(fixture.id().await as i64), ..Default::default() };
        assert!(matches!(tutorial_svc(&read, &fixture.ctx(&other)).await, Some(Outcome::Value(value)) if value == rowset(&["tutorialID", "pageID", "eventTypeID"], vec![])));
    }
}
