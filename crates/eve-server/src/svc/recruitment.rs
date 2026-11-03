//! Corporation adverts, as consumed by Trinity's bco_recruitment.py.
use eve_marshal::Value;
use tokio_postgres::Client;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::{rows, wallet};

const DAY: i64 = 864_000_000_000;
const HEADER: &[&str] = &[
    "adID",
    "corporationID",
    "allianceID",
    "stationID",
    "regionID",
    "raceMask",
    "typeMask",
    "skillPoints",
    "description",
    "createDateTime",
    "expiryDateTime",
];
const SELECT: &str = r#"SELECT a."adID",a."corporationID",c."allianceID",a."stationID",
    s."regionID",a."raceMask",a."typeMask",a."skillPoints",a.description,
    a."createDateTime",a."expiryDateTime" FROM "crpRecruitmentAds" a
    JOIN corporation c ON c."corporationID"=a."corporationID"
    JOIN "staStations" s ON s."stationID"=a."stationID""#;

pub async fn ensure(db: &Client) -> Result<(), tokio_postgres::Error> {
    db.batch_execute(r#"
        CREATE TABLE IF NOT EXISTS "crpRecruitmentAds" (
            "adID" integer PRIMARY KEY DEFAULT nextval('dyn_id_seq'),
            "corporationID" integer NOT NULL, "stationID" integer NOT NULL,
            "raceMask" bigint NOT NULL, "typeMask" bigint NOT NULL,
            "skillPoints" bigint NOT NULL CHECK ("skillPoints" BETWEEN 0 AND 200000000),
            description text NOT NULL, "createDateTime" bigint NOT NULL,
            "expiryDateTime" bigint NOT NULL);
        CREATE INDEX IF NOT EXISTS "crpRecruitmentAds_corp" ON "crpRecruitmentAds" ("corporationID");
        CREATE TABLE IF NOT EXISTS "crpRecruitmentAdTypes" (
            "typeID" integer PRIMARY KEY, "typeMask" bigint NOT NULL UNIQUE,
            "typeName" text NOT NULL, description text NOT NULL,
            "groupID" integer NOT NULL, "groupName" text NOT NULL);
        -- The Trinity static export lacks this catalogue. Imported original
        -- rows are served verbatim; no later-era or invented categories are seeded.
    "#).await
}

fn number(call: &Call, i: usize) -> Option<i64> {
    call.arg_i64(i)
        .or_else(|| call.arg_text(i)?.trim().parse().ok())
}

fn failure(e: tokio_postgres::Error) -> Outcome {
    tracing::error!("recruitment: {}", crate::logging::chain(&e));
    Outcome::custom("The recruitment advert could not be saved. Please try again.")
}

pub async fn corporation_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let result = match call.method.as_str() {
        "GetRecruitmentAdTypes" => ctx
            .db
            .query(
                r#"SELECT "typeID","typeMask","typeName",description,"groupID","groupName"
               FROM "crpRecruitmentAdTypes" ORDER BY "groupID","typeID""#,
                &[],
            )
            .await
            .map(|r| {
                rows::rowset(
                    &[
                        "typeID",
                        "typeMask",
                        "typeName",
                        "description",
                        "groupID",
                        "groupName",
                    ],
                    rows::lines(&r),
                )
            }),
        "GetRecruitmentAdsForCorporation" => {
            let corp = call.arg_i64(0).or(ctx.session.corp_id).unwrap_or(0) as i32;
            ctx.db
                .query(
                    &format!("{SELECT} WHERE a.\"corporationID\"=$1 ORDER BY a.\"adID\""),
                    &[&corp],
                )
                .await
                .map(|r| rows::rowset(HEADER, rows::lines(&r)))
        }
        "GetRecruitmentAdsByCriteria" => {
            let region = number(call, 0).unwrap_or(0);
            let skill = number(call, 1).unwrap_or(i64::MAX).max(0);
            let types = number(call, 2).unwrap_or(0).max(0);
            let races = number(call, 3).unwrap_or(0).max(0);
            let alliance = number(call, 4).unwrap_or(0) != 0;
            let min = number(call, 5).unwrap_or(0).max(0);
            let max = number(call, 6).unwrap_or(0).max(0);
            ctx.db
                .query(
                    &format!(
                        r#"{SELECT} WHERE a."expiryDateTime">$1
                AND ($2::bigint<=0 OR s."regionID"=$2) AND a."skillPoints"<=$3
                AND ($4::bigint=0 OR a."typeMask"=0 OR a."typeMask" & $4 <> 0)
                AND ($5::bigint=0 OR a."raceMask"=0 OR a."raceMask" & $5 <> 0)
                AND (NOT $6 OR COALESCE(c."allianceID",0)<>0)
                AND c."memberCount">=$7::bigint AND ($8::bigint=0 OR c."memberCount"<=$8)
                ORDER BY a."createDateTime" DESC,a."adID" DESC LIMIT 1000"#
                    ),
                    &[
                        &crate::bulkdata::filetime_now(),
                        &region,
                        &skill,
                        &types,
                        &races,
                        &alliance,
                        &min,
                        &max,
                    ],
                )
                .await
                .map(|r| rows::rowset(HEADER, rows::lines(&r)))
        }
        _ => return None,
    };
    Some(result.map_or_else(failure, Outcome::Value))
}

pub async fn corp_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if call.method == "GetRecruitmentAdsForCorporation" {
        let corp = ctx.session.corp_id.unwrap_or(0) as i32;
        return Some(
            ctx.db
                .query(
                    &format!("{SELECT} WHERE a.\"corporationID\"=$1 ORDER BY a.\"adID\""),
                    &[&corp],
                )
                .await
                .map(|r| Outcome::Value(rows::rowset(HEADER, rows::lines(&r))))
                .unwrap_or_else(failure),
        );
    }
    if !matches!(
        call.method.as_str(),
        "CreateRecruitmentAd" | "UpdateRecruitmentAd" | "DeleteRecruitmentAd"
    ) {
        return None;
    }
    Some(write_ad(call, ctx).await.unwrap_or_else(|e| e))
}

fn validate(races: i64, types: i64, skill: i64, description: &str) -> Result<(), Outcome> {
    if races < 0
        || races & !15 != 0
        || types < 0
        || !(0..=200_000_000).contains(&skill)
        || description.chars().count() > 1000
    {
        return Err(Outcome::custom(
            "Invalid recruitment criteria or description.",
        ));
    }
    Ok(())
}

async fn write_ad(call: &Call, ctx: &Ctx<'_>) -> Result<Outcome, Outcome> {
    let corp = ctx
        .session
        .corp_id
        .ok_or_else(|| Outcome::custom("Join a corporation first."))? as i32;
    let character = ctx
        .session
        .char_id
        .ok_or_else(|| Outcome::custom("Select a character first."))?;
    if !ctx.session.has_corp_role(128)
        && !ctx
            .session
            .has_corp_role(crate::session::CORP_ROLE_DIRECTOR)
    {
        return Err(Outcome::custom(
            "Only personnel managers may manage recruitment adverts.",
        ));
    }
    let mut db = crate::db::connect(ctx.db_url).await.map_err(failure)?;
    let tx = db.transaction().await.map_err(failure)?;
    // Serialize the corporation's changes and verify membership in the same transaction.
    if tx
        .query_opt(
            r#"SELECT c."corporationID" FROM corporation c JOIN "character_" p
        ON p."corporationID"=c."corporationID" WHERE c."corporationID"=$1 AND p."characterID"=$2
        AND c."corporationID">=100000000 FOR UPDATE OF c"#,
            &[&corp, &(character as i32)],
        )
        .await
        .map_err(failure)?
        .is_none()
    {
        return Err(Outcome::custom(
            "Only members of player corporations may manage adverts.",
        ));
    }
    let create = call.method == "CreateRecruitmentAd";
    let delete = call.method == "DeleteRecruitmentAd";
    let mut id = number(call, 0).unwrap_or(0);
    let old = if create {
        None
    } else {
        let r = tx
            .query_opt(
                &format!("{SELECT} WHERE a.\"adID\"=$1 AND a.\"corporationID\"=$2 FOR UPDATE OF a"),
                &[&(id as i32), &corp],
            )
            .await
            .map_err(failure)?
            .ok_or_else(|| Outcome::custom("That advert does not belong to your corporation."))?;
        Some(rows::line(&r))
    };
    if delete {
        tx.execute(
            r#"DELETE FROM "crpRecruitmentAds" WHERE "adID"=$1 AND "corporationID"=$2"#,
            &[&(id as i32), &corp],
        )
        .await
        .map_err(failure)?;
    } else {
        let races = number(call, 2).unwrap_or(-1);
        let types = number(call, if create { 3 } else { 1 }).unwrap_or(-1);
        let skill = number(call, if create { 5 } else { 3 }).unwrap_or(-1);
        let description = call
            .arg_text(if create { 6 } else { 4 })
            .unwrap_or_default();
        validate(races, types, skill, &description)?;
        if create {
            let days = number(call, 0).unwrap_or(0);
            if !(1..=30).contains(&days) {
                return Err(Outcome::custom("Adverts must run for 1 to 30 days."));
            }
            let station = number(call, 1)
                .and_then(|v| i32::try_from(v).ok())
                .unwrap_or(0);
            if tx.query_opt(r#"SELECT "itemID" FROM "crpOffices" WHERE "corporationID"=$1 AND "stationID"=$2 FOR SHARE"#,
                &[&corp,&station]).await.map_err(failure)?.is_none() {
                return Err(Outcome::custom("Your corporation must rent an office at this station."));
            }
            let now = crate::bulkdata::filetime_now();
            let expires = now + days * DAY;
            id = tx.query_one(r#"INSERT INTO "crpRecruitmentAds" ("corporationID","stationID","raceMask","typeMask","skillPoints",description,"createDateTime","expiryDateTime")
                VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING "adID""#,
                &[&corp,&station,&races,&types,&skill,&description,&now,&expires]).await.map_err(failure)?.get::<_,i32>(0) as i64;
            let arg = id.to_string();
            let entry = wallet::Entry {
                ref_type: 86,
                payer: corp as i64,
                payee: 0,
                arg_id1: &arg,
                reason: "Corporation recruitment advert",
            };
            wallet::post(
                &tx,
                corp as i64,
                -(500_000.0 + 250_000.0 * days as f64),
                &entry,
            )
            .await
            .map_err(|e| match e {
                wallet::Transfer::NotEnough { need, have } => Outcome::not_enough_money(need, have),
                wallet::Transfer::Db(e) => failure(e),
                wallet::Transfer::NoSuchOwner => Outcome::custom("Corporation wallet not found."),
            })?;
        } else {
            tx.execute(r#"UPDATE "crpRecruitmentAds" SET "raceMask"=$2,"typeMask"=$3,"skillPoints"=$4,description=$5 WHERE "adID"=$1"#,
                &[&(id as i32),&races,&types,&skill,&description]).await.map_err(failure)?;
        }
    }
    let new = if delete {
        None
    } else {
        Some(rows::line(
            &tx.query_one(&format!("{SELECT} WHERE a.\"adID\"=$1"), &[&(id as i32)])
                .await
                .map_err(failure)?,
        ))
    };
    tx.commit().await.map_err(failure)?;
    if create {
        wallet::notify_balance(ctx, corp as i64).await;
    }
    let world = ctx.world.lock();
    world.notify(
        &world.corp_members(corp as i64),
        "OnCorporationRecruitmentAdChanged",
        Value::Tuple(vec![
            Value::Int(corp as i64),
            Value::Int(id),
            rows::change(HEADER, old.as_deref(), new.as_deref()),
        ]),
    );
    Ok(Outcome::Value(if create {
        Value::Int(id)
    } else {
        Value::None
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(method: &str, args: Vec<Value>) -> Call {
        Call {
            service: "corpRegistry".into(),
            method: method.into(),
            args,
            kwargs: vec![],
        }
    }
    fn value(out: Option<Outcome>) -> Value {
        match out {
            Some(Outcome::Value(v)) => v,
            Some(Outcome::UserError { msg, args }) => panic!("{msg}: {args:?}"),
            _ => panic!("call was not handled"),
        }
    }
    fn count(v: &Value) -> usize {
        match v {
            Value::Instance { state, .. } => match &**state {
                Value::Dict(d) => d
                    .iter()
                    .find(|(k, _)| k.as_string().as_deref() == Some("lines"))
                    .unwrap()
                    .1
                    .seq()
                    .unwrap()
                    .len(),
                _ => panic!("not rowset state"),
            },
            _ => panic!("not rowset"),
        }
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE pointing at a disposable imported database"]
    async fn adverts_persist_filter_charge_once_and_rollback_refusals() {
        let fixture = crate::test_support::Database::load().await;
        let db = &fixture.db;
        let corp = fixture.id().await;
        let pilot = fixture.id().await;
        let folder = fixture.id().await;
        let station = db
            .query_one(
                r#"SELECT "stationID","regionID" FROM "staStations" ORDER BY "stationID" LIMIT 1"#,
                &[],
            )
            .await
            .unwrap();
        let station_id: i32 = station.get(0);
        let region: i32 = station.get(1);
        db.execute(r#"INSERT INTO corporation ("corporationID","corporationName",balance,"memberCount") VALUES ($1,$2,10000000,5)"#, &[&corp,&format!("Recruitment test {corp}")]).await.unwrap();
        db.execute(r#"INSERT INTO "character_" ("characterID","characterName","corporationID") VALUES ($1,$2,$3)"#, &[&pilot,&format!("Recruitment pilot {pilot}"),&corp]).await.unwrap();
        db.execute(r#"INSERT INTO "crpOffices" ("corporationID","stationID","officeFolderID","itemID") VALUES ($1,$2,$3,$3)"#, &[&corp,&station_id,&folder]).await.unwrap();
        let session = crate::session::Session {
            char_id: Some(pilot as i64),
            corp_id: Some(corp as i64),
            roles_at_all: 128,
            ..Default::default()
        };
        let ctx = fixture.ctx(&session);
        let create = call(
            "CreateRecruitmentAd",
            vec![
                Value::text("2"),
                Value::Int(station_id as i64),
                Value::Int(1),
                Value::Int(4),
                Value::Int(999),
                Value::Int(500),
                Value::text("Pilots wanted"),
            ],
        );
        let id = value(corp_registry(&create, &ctx).await).as_i64().unwrap() as i32;
        assert_eq!(
            db.query_one(
                "SELECT balance FROM corporation WHERE \"corporationID\"=$1",
                &[&corp]
            )
            .await
            .unwrap()
            .get::<_, f64>(0),
            9_000_000.0
        );
        let search = |skills| {
            call(
                "GetRecruitmentAdsByCriteria",
                vec![
                    Value::Int(region as i64),
                    Value::Int(skills),
                    Value::Int(4),
                    Value::Int(1),
                    Value::Bool(false),
                    Value::Int(0),
                    Value::Int(0),
                ],
            )
        };
        assert!(count(&value(corporation_svc(&search(500), &ctx).await)) >= 1);
        let stored = db
            .query_one(&format!("{SELECT} WHERE a.\"adID\"=$1"), &[&id])
            .await
            .unwrap();
        assert_eq!(
            stored.get::<_, i32>("allianceID"),
            0,
            "ignore client-forged alliance"
        );
        assert_eq!(
            stored.get::<_, i64>("expiryDateTime") - stored.get::<_, i64>("createDateTime"),
            2 * DAY
        );
        value(
            corp_registry(
                &call(
                    "UpdateRecruitmentAd",
                    vec![
                        Value::Int(id as i64),
                        Value::Int(8),
                        Value::Int(2),
                        Value::Int(1000),
                        Value::text("Updated"),
                    ],
                ),
                &ctx,
            )
            .await,
        );
        assert_eq!(
            db.query_one(
                r#"SELECT description FROM "crpRecruitmentAds" WHERE "adID"=$1"#,
                &[&id]
            )
            .await
            .unwrap()
            .get::<_, String>(0),
            "Updated"
        );
        let unauthorized = crate::session::Session {
            roles_at_all: 0,
            ..session.clone()
        };
        assert!(matches!(
            corp_registry(
                &call("DeleteRecruitmentAd", vec![Value::Int(id as i64)]),
                &fixture.ctx(&unauthorized)
            )
            .await,
            Some(Outcome::UserError { .. })
        ));
        db.execute(
            "UPDATE corporation SET balance=0 WHERE \"corporationID\"=$1",
            &[&corp],
        )
        .await
        .unwrap();
        assert!(
            matches!(corp_registry(&create,&ctx).await,Some(Outcome::UserError{msg,..}) if msg=="NotEnoughMoney")
        );
        assert_eq!(
            db.query_one(
                r#"SELECT count(*) FROM "crpRecruitmentAds" WHERE "corporationID"=$1"#,
                &[&corp]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
            1,
            "failed payment must roll back the advert"
        );
        assert_eq!(
            db.query_one(
                r#"SELECT count(*) FROM market_journal WHERE "characterID"=$1 AND "refTypeID"=86"#,
                &[&corp]
            )
            .await
            .unwrap()
            .get::<_, i64>(0),
            1
        );
        value(
            corp_registry(
                &call("DeleteRecruitmentAd", vec![Value::Int(id as i64)]),
                &ctx,
            )
            .await,
        );
        assert_eq!(
            count(&value(
                corp_registry(&call("GetRecruitmentAdsForCorporation", vec![]), &ctx).await
            )),
            0
        );
        db.execute(
            r#"DELETE FROM market_journal WHERE "characterID"=$1"#,
            &[&corp],
        )
        .await
        .unwrap();
        db.execute(
            r#"DELETE FROM "crpOffices" WHERE "corporationID"=$1"#,
            &[&corp],
        )
        .await
        .unwrap();
        db.execute(
            r#"DELETE FROM "character_" WHERE "characterID"=$1"#,
            &[&pilot],
        )
        .await
        .unwrap();
        db.execute(
            r#"DELETE FROM corporation WHERE "corporationID"=$1"#,
            &[&corp],
        )
        .await
        .unwrap();
    }
    #[test]
    fn recruitment_rejects_invalid_criteria_and_counts_unicode_characters() {
        assert!(validate(15, 255, 200_000_000, &"日".repeat(1000)).is_ok());
        for (race, types, skill, text) in [
            (16, 0, 0, String::new()),
            (1, -1, 0, String::new()),
            (1, 1, -1, String::new()),
            (1, 1, 200_000_001, String::new()),
            (1, 1, 0, "日".repeat(1001)),
        ] {
            assert!(validate(race, types, skill, &text).is_err());
        }
    }
}
