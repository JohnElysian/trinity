use eve_marshal::Value;
use tokio_postgres::Row as PgRow;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{row, rowset};

pub async fn charmgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetPublicInfo" => public_info(ctx, subject(call, ctx)).await,
        "GetPublicInfo3" => public_info3(ctx, subject(call, ctx)).await,
        "GetPrivateInfo" => private_info(ctx, subject(call, ctx)).await,
        "GetCharacterInfo" => character_info(ctx).await,
        "GetTopBounties" => top_bounties(ctx).await,
        "AddToBounty" => return Some(add_to_bounty(ctx, call).await),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// AddToBounty(charID, amount): the caller pays; the bounty goes on the target.
async fn add_to_bounty(ctx: &Ctx<'_>, call: &Call) -> Outcome {
    match place_bounty(ctx, call).await {
        Ok(()) => {
            super::wallet::notify_balance(ctx, ctx.session.char_id.unwrap_or(0)).await;
            Outcome::Value(Value::None)
        }
        Err(outcome) => outcome,
    }
}

#[cfg(test)]
mod bounty_tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE pointing at a disposable imported database"]
    async fn dashboard_feature_combat_history_and_alliance_search() {
        let fixture = crate::test_support::Database::load().await;
        let pilot = fixture.id().await;
        let other = fixture.id().await;
        let alliance = fixture.id().await;
        let name = format!("Test Alliance {alliance}");
        let ticker = format!("T{alliance}");
        fixture.db.execute(r#"INSERT INTO alliances ("allianceID","allianceName","shortName") VALUES ($1,$2,$3)"#,
            &[&alliance,&name,&ticker]).await.unwrap();
        let session = crate::session::Session {char_id:Some(pilot as i64),..Default::default()};
        let ctx = fixture.ctx(&session);
        for (method, term) in [("LookupAlliances",name),("LookupAllianceShortNames",ticker)] {
            let call = Call { service:"lookupSvc".into(),method:method.into(),args:vec![Value::text(term),Value::Int(1)],kwargs:vec![] };
            let Some(Outcome::Value(Value::Instance {state,..})) = crate::svc::lookup::lookup_svc(&call,&ctx).await else {panic!("rowset")};
            let lines = state.get_str("lines").unwrap().seq().unwrap();
            assert_eq!(lines.len(),1);
            assert_eq!(lines[0].seq().unwrap()[1],Value::Int(alliance as i64));
        }
        let kill:i64 = fixture.db.query_one(r#"INSERT INTO "killLog" ("killTime","victimCharID","victimCorpID","victimShipTypeID","solarSystemID","finalBlowID","totalDamage","killBlob")
            VALUES (1234,$1,1000168,606,30004971,$2,250,'<items><i t="34" f="5" d="2" x="3"/></items>') RETURNING "killID""#,
            &[&pilot,&other]).await.unwrap().get(0);
        fixture.db.execute(r#"INSERT INTO "killLogParticipants" ("killID","ownerID","corporationID","shipTypeID","weaponTypeID","damageDone","finalBlow","securityStatus")
            VALUES ($1,$2,1000168,606,484,250,1,-1.5)"#,&[&kill,&other]).await.unwrap();
        let Value::Instance {state,..} = recent_kills(&ctx).await else {panic!("combat rowset")};
        let lines = state.get_str("lines").unwrap().seq().unwrap();
        assert_eq!(lines.len(),1);
        let fields = lines[0].seq().unwrap();
        assert_eq!(fields[9],Value::Int(other as i64));
        assert_eq!(fields[15],Value::Double(-1.5));
        assert!(fields[16].as_string().unwrap().contains("d=\"2\" x=\"3\""));
        let stranger = crate::session::Session {char_id:Some(fixture.id().await as i64),..Default::default()};
        let Value::Instance {state,..} = recent_kills(&fixture.ctx(&stranger)).await else {panic!("rowset")};
        assert!(state.get_str("lines").unwrap().seq().unwrap().is_empty());
    }
    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE pointing at a disposable imported database"]
    async fn bounty_payment_validation_and_payout_are_atomic() {
        let fixture = crate::test_support::Database::load().await;
        let payer = fixture.id().await;
        let victim = fixture.id().await;
        let killer = fixture.id().await;
        let missing = fixture.id().await;
        for id in [payer,victim,killer] {
            fixture.db.execute(r#"INSERT INTO "character_" ("characterID","characterName",balance,"securityRating") VALUES ($1,$2,20000,-2)"#,
                &[&id,&format!("Bounty test {id}")]).await.unwrap();
        }
        let session = crate::session::Session {char_id:Some(payer as i64),..Default::default()};
        let ctx = fixture.ctx(&session);
        let call = |target,amount| Call {service:"charmgr".into(),method:"AddToBounty".into(),args:vec![Value::Int(target as i64),Value::Double(amount)],kwargs:vec![]};
        for bad in [call(missing,5000.0),call(victim,f64::NAN),call(victim,4999.0),call(victim,30000.0),call(payer,5000.0)] {
            assert!(matches!(add_to_bounty(&ctx,&bad).await,Outcome::UserError{..}));
        }
        assert_eq!(fixture.db.query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,&[&payer]).await.unwrap().get::<_,f64>(0),20000.0);
        assert!(matches!(add_to_bounty(&ctx,&call(victim,5000.0)).await,Outcome::Value(Value::None)));
        let mut db = crate::db::connect(&fixture.url).await.unwrap();
        let tx = db.transaction().await.unwrap();
        assert_eq!(crate::losses::pay_player_bounty(&tx,victim as i64,killer as i64).await.unwrap(),5000.0);
        tx.rollback().await.unwrap();
        assert_eq!(fixture.db.query_one(r#"SELECT bounty FROM "character_" WHERE "characterID"=$1"#,&[&victim]).await.unwrap().get::<_,f64>(0),5000.0);
        let tx = db.transaction().await.unwrap();
        assert_eq!(crate::losses::pay_player_bounty(&tx,victim as i64,killer as i64).await.unwrap(),5000.0);
        tx.commit().await.unwrap();
        let tx = db.transaction().await.unwrap();
        assert_eq!(crate::losses::pay_player_bounty(&tx,victim as i64,killer as i64).await.unwrap(),0.0);
        tx.commit().await.unwrap();
        assert_eq!(fixture.db.query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,&[&killer]).await.unwrap().get::<_,f64>(0),25000.0);
        assert_eq!(fixture.db.query_one(r#"SELECT count(*) FROM bounties WHERE "characterID"=$1"#,&[&victim]).await.unwrap().get::<_,i64>(0),0);
        fixture.db.execute(r#"DELETE FROM market_journal WHERE "characterID"=ANY($1)"#,&[&vec![payer,killer]]).await.unwrap();
        fixture.db.execute(r#"DELETE FROM "character_" WHERE "characterID"=ANY($1)"#,&[&vec![payer,victim,killer]]).await.unwrap();
    }
}

async fn place_bounty(ctx: &Ctx<'_>, call: &Call) -> Result<(), Outcome> {
    use crate::svc::wallet::{post, Entry, Transfer, REF_BOUNTY};
    let target = i32::try_from(call.arg_id(0)).map_err(|_| Outcome::custom("Invalid bounty target."))?;
    let amount = call.args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
    let me = ctx.session.char_id.ok_or_else(|| Outcome::custom("Select a character first."))? as i32;
    // base_missions.py's bounty form requires at least 5,000 ISK.
    if !amount.is_finite() || amount < 5000.0 || target == me || target < 100_000_000 {
        return Err(Outcome::custom("A bounty must be at least 5,000 ISK and name another pilot."));
    }
    let failed = |e: tokio_postgres::Error| {
        error!("AddToBounty: {}", crate::logging::chain(&e));
        Outcome::custom("The bounty could not be placed. Your payment has not been taken.")
    };
    let mut db = crate::db::connect(ctx.db_url).await.map_err(failed)?;
    let tx = db.transaction().await.map_err(failed)?;
    let pilots = tx.query(r#"SELECT "characterID","securityRating" FROM "character_"
        WHERE "characterID"=ANY($1) ORDER BY "characterID" FOR UPDATE"#,
        &[&vec![me,target]]).await.map_err(failed)?;
    let Some(victim) = pilots.iter().find(|r| r.get::<_,i32>(0)==target) else {
        return Err(Outcome::custom("That pilot does not exist."));
    };
    if victim.get::<_,f64>(1) > -1.0 {
        return Err(Outcome::custom("Bounties require a security status of -1.0 or lower."));
    }
    let arg = target.to_string();
    let entry = Entry { payer:me as i64,payee:0,ref_type:REF_BOUNTY,arg_id1:&arg,reason:"Player bounty" };
    post(&tx,me as i64,-amount,&entry).await.map_err(|e| match e {
        Transfer::NotEnough{need,have} => Outcome::not_enough_money(need,have),
        Transfer::NoSuchOwner => Outcome::custom("Your wallet could not be found."),
        Transfer::Db(e) => failed(e),
    })?;
    let now = crate::bulkdata::filetime_now();
    let sql = r#"WITH placed AS (
                     INSERT INTO "bounties" ("characterID","ownerID","bounty","placedAt")
                     VALUES ($1,$2,$3,$4)
                     ON CONFLICT ("characterID","ownerID")
                     DO UPDATE SET "bounty" = "bounties"."bounty" + EXCLUDED."bounty",
                                   "placedAt" = EXCLUDED."placedAt")
                 UPDATE "character_" SET "bounty" = "bounty" + $3 WHERE "characterID" = $1"#;
    tx.execute(sql, &[&target, &me, &amount, &now]).await.map_err(failed)?;
    tx.commit().await.map_err(failed)?;
    tracing::info!("bounty of {amount} ISK placed on {target}");
    Ok(())
}

pub async fn character(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetPrivateInfo" => private_info(ctx, subject(call, ctx)).await,
        "GetCharacterInfo" => character_info(ctx).await,
        "GetCharacterDescription" => character_description(ctx, subject(call, ctx)).await,
        "SetCharacterDescription" => set_character_description(call, ctx).await,
        "GetCloneTypeID" => clone_type_id(ctx).await,
        "GetHomeStation" => crate::svc::medical::get_home_station(ctx).await,
        "GetRecentShipKillsAndLosses" => recent_kills(ctx).await,
        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// Trinity's combat log reads these fields directly, including the XML used
/// by util.CombatLog_CopyText. Older rows have no recoverable item-loss detail.
async fn recent_kills(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["killID","killTime","solarSystemID","moonID","victimCharacterID",
        "victimCorporationID","victimAllianceID","victimShipTypeID","victimDamageTaken",
        "finalCharacterID","finalCorporationID","finalAllianceID","finalShipTypeID",
        "finalWeaponTypeID","finalDamageDone","finalSecurityStatus","killBlob"];
    let Some(me) = ctx.session.char_id else { return rowset(HEADER, Vec::new()) };
    let sql = r#"SELECT k."killID",k."killTime",k."solarSystemID",NULL::integer AS "moonID",
        NULLIF(k."victimCharID",0),NULLIF(k."victimCorpID",0),NULLIF(k."victimAllianceID",0),k."victimShipTypeID",k."totalDamage",
        CASE WHEN k."finalBlowID">0 THEN k."finalBlowID" END,NULLIF(p."corporationID",0),NULLIF(p."allianceID",0),
        NULLIF(p."shipTypeID",0),NULLIF(p."weaponTypeID",0),COALESCE(p."damageDone",0),p."securityStatus",k."killBlob"
        FROM "killLog" k LEFT JOIN "killLogParticipants" p ON p."killID"=k."killID" AND p."ownerID"=k."finalBlowID"
        WHERE k."victimCharID"=$1 OR k."finalBlowID"=$1 ORDER BY k."killTime" DESC,k."killID" DESC LIMIT 100"#;
    let found = ctx.query(sql, &[&(me as i32)]).await;
    let ids = found.iter().map(|r| r.get::<_,i64>(0)).collect::<Vec<_>>();
    let participants = ctx.query(r#"SELECT "killID","ownerID","corporationID","allianceID","shipTypeID","weaponTypeID","damageDone","securityStatus"
        FROM "killLogParticipants" WHERE "killID"=ANY($1) AND "finalBlow"=0 ORDER BY "damageDone" DESC,"ownerID""#, &[&ids]).await;
    let lines = found.iter().map(|r| {
        let kill = r.get::<_,i64>(0);
        let mut blob = String::from("<attackers>");
        for p in participants.iter().filter(|p| p.get::<_,i64>(0)==kill) {
            blob.push_str("<a");
            for (column,key) in [(1,"c"),(2,"r"),(3,"a"),(4,"s"),(5,"w")] {
                if let Some(value) = p.get::<_,Option<i32>>(column).filter(|v| *v > 0) { blob.push_str(&format!(" {key}=\"{value}\"")); }
            }
            blob.push_str(&format!(" d=\"{}\"",p.get::<_,f64>(6).max(0.0).round() as i64));
            if let Some(sec) = p.get::<_,Option<f64>>(7) { blob.push_str(&format!(" t=\"{sec:.1}\"")); }
            blob.push_str("/>");
        }
        blob.push_str("</attackers>");
        blob.push_str(&r.get::<_,String>(16));
        let mut fields = crate::svc::rows::line(r);
        fields[16] = Value::text(blob);
        Value::List(fields)
    }).collect();
    rowset(HEADER, lines)
}

fn subject(call: &Call, ctx: &Ctx<'_>) -> i64 {
    call.args
        .first()
        .and_then(|v| v.as_i64())
        .or(ctx.session.char_id)
        .unwrap_or(0)
}



fn empty_row(header: &[&str]) -> Value {
    row(header, header.iter().map(|_| Value::None).collect())
}

fn int_col(r: &PgRow, i: usize) -> Value {
    match r.try_get::<_, Option<i32>>(i) {
        Ok(Some(v)) => Value::Int(v as i64),
        _ => Value::None,
    }
}

fn small_col(r: &PgRow, i: usize) -> Value {
    match r.try_get::<_, Option<i16>>(i) {
        Ok(Some(v)) => Value::Int(v as i64),
        _ => Value::None,
    }
}

fn long_col(r: &PgRow, i: usize) -> Value {
    match r.try_get::<_, Option<i64>>(i) {
        Ok(Some(v)) => Value::Long(v),
        _ => Value::None,
    }
}

fn dbl_col(r: &PgRow, i: usize) -> Value {
    match r.try_get::<_, Option<f64>>(i) {
        Ok(Some(v)) => Value::Double(v),
        _ => Value::None,
    }
}

fn txt_col(r: &PgRow, i: usize) -> Value {
    match r.try_get::<_, Option<String>>(i) {
        Ok(Some(v)) => Value::Bytes(v.into_bytes()),
        _ => Value::None,
    }
}

async fn public_info(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "characterID",
        "charName",
        "raceID",
        "raceName",
        "bloodlineID",
        "bloodlineName",
        "gender",
        "corporationID",
        "createDateTime",
        // What Trinity's character sheet shows of how the character was made.
        "ancestryID",
        "schoolID",
        "careerID",
        "careerSpecialityID",
    ];
    let sql = r#"SELECT c."characterID", c."characterName",
                        COALESCE(NULLIF(c."raceID", 0), b."raceID") AS "raceID",
                        r."raceName", c."bloodlineID", b."bloodlineName",
                        c."gender", c."corporationID", c."createDateTime",
                        c."ancestryID", c."schoolID", c."careerID", c."careerSpecialityID"
                   FROM "character_" c
                   LEFT JOIN "chrBloodlines" b ON b."bloodlineID" = c."bloodlineID"
                   LEFT JOIN "chrRaces" r
                          ON r."raceID" = COALESCE(NULLIF(c."raceID", 0), b."raceID")
                  WHERE c."characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) => match rows.first() {
            Some(r) => row(
                HEADER,
                vec![
                    int_col(r, 0),
                    txt_col(r, 1),
                    int_col(r, 2),
                    txt_col(r, 3),
                    int_col(r, 4),
                    txt_col(r, 5),
                    small_col(r, 6),
                    int_col(r, 7),
                    long_col(r, 8),
                    int_col(r, 9),
                    int_col(r, 10),
                    int_col(r, 11),
                    int_col(r, 12),
                ],
            ),
            None => empty_row(HEADER),
        },
        Err(e) => {
            error!("charmgr.GetPublicInfo({char_id}): {err}", err = crate::logging::chain(&e));
            empty_row(HEADER)
        }
    }
}

async fn public_info3(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &["description", "startDateTime", "bounty"];
    let sql = r#"SELECT "description",
                        CASE WHEN "corporationDateTime" > 0
                             THEN "corporationDateTime" ELSE "startDateTime" END,
                        "bounty"
                   FROM "character_" WHERE "characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) => {
            let lines = rows
                .iter()
                .map(|r| Value::List(vec![txt_col(r, 0), long_col(r, 1), dbl_col(r, 2)]))
                .collect();
            rowset(HEADER, lines)
        }
        Err(e) => {
            error!("charmgr.GetPublicInfo3({char_id}): {err}", err = crate::logging::chain(&e));
            rowset(HEADER, Vec::new())
        }
    }
}

async fn private_info(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "characterID",
        "charName",
        "corporationID",
        "title",
        "description",
        "balance",
        "bounty",
        "securityRating",
        "logonMinutes",
        "createDateTime",
        "startDateTime",
        "intelligence",
        "memory",
        "charisma",
        "perception",
        "willpower",
    ];
    let sql = r#"SELECT c."characterID", c."characterName", c."corporationID", c."title",
                        c."description", c."balance", c."bounty", c."securityRating",
                        c."logonMinutes", c."createDateTime", c."startDateTime",
                        b."intelligence", b."memory", b."charisma", b."perception",
                        b."willpower"
                   FROM "character_" c
                   LEFT JOIN "chrBloodlines" b ON b."bloodlineID" = c."bloodlineID"
                  WHERE c."characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) => match rows.first() {
            Some(r) => row(
                HEADER,
                vec![
                    int_col(r, 0),
                    txt_col(r, 1),
                    int_col(r, 2),
                    txt_col(r, 3),
                    txt_col(r, 4),
                    dbl_col(r, 5),
                    dbl_col(r, 6),
                    dbl_col(r, 7),
                    int_col(r, 8),
                    long_col(r, 9),
                    long_col(r, 10),
                    int_col(r, 11),
                    int_col(r, 12),
                    int_col(r, 13),
                    int_col(r, 14),
                    int_col(r, 15),
                ],
            ),
            None => empty_row(HEADER),
        },
        Err(e) => {
            error!("charmgr.GetPrivateInfo({char_id}): {err}", err = crate::logging::chain(&e));
            empty_row(HEADER)
        }
    }
}

async fn character_info(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["characterID", "characterName"];
    let Some(user_id) = ctx.session.user_id else { return rowset(HEADER, Vec::new()) };
    let sql = r#"SELECT "characterID","characterName" FROM "character_"
                  WHERE "accountID" = $1 ORDER BY "characterID""#;
    match ctx.db.query(sql, &[&(user_id as i32)]).await {
        Ok(rows) => {
            let lines = rows
                .iter()
                .map(|r| Value::List(vec![int_col(r, 0), txt_col(r, 1)]))
                .collect();
            rowset(HEADER, lines)
        }
        Err(e) => {
            error!("character.GetCharacterInfo: {err}", err = crate::logging::chain(&e));
            rowset(HEADER, Vec::new())
        }
    }
}

async fn top_bounties(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["characterID", "ownerName", "bounty", "online"];
    let sql = r#"SELECT "characterID","characterName","bounty" FROM "character_"
                  WHERE "bounty" > 0 ORDER BY "bounty" DESC LIMIT 10"#;
    match ctx.db.query(sql, &[]).await {
        Ok(rows) => {
            let lines = rows
                .iter()
                .map(|r| {
                    Value::List(vec![int_col(r, 0), txt_col(r, 1), dbl_col(r, 2), Value::Int(0)])
                })
                .collect();
            rowset(HEADER, lines)
        }
        Err(e) => {
            error!("charmgr.GetTopBounties: {err}", err = crate::logging::chain(&e));
            rowset(HEADER, Vec::new())
        }
    }
}

async fn character_description(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &["description"];
    let sql = r#"SELECT "description" FROM "character_" WHERE "characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) => match rows.first() {
            Some(r) => row(HEADER, vec![txt_col(r, 0)]),
            None => Value::None,
        },
        Err(e) => {
            error!("character.GetCharacterDescription({char_id}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

async fn set_character_description(call: &Call, ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let text = match call.args.first() {
        Some(Value::Bytes(b)) | Some(Value::Buffer(b)) => String::from_utf8_lossy(b).into_owned(),
        Some(Value::Unicode(s)) => s.clone(),
        _ => String::new(),
    };
    let text: String = text.chars().take(4000).collect();
    let sql = r#"UPDATE "character_" SET "description" = $1 WHERE "characterID" = $2"#;
    if let Err(e) = ctx.db.execute(sql, &[&text, &(char_id as i32)]).await {
        error!("character.SetCharacterDescription({char_id}): {err}", err = crate::logging::chain(&e));
    }
    Value::None
}

async fn clone_type_id(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::None };
    let sql = r#"SELECT e."typeID" FROM "entity" e
                   JOIN "invTypes" t ON t."typeID" = e."typeID"
                  WHERE e."ownerID" = $1 AND t."groupID" = 23
                  ORDER BY e."typeID" DESC LIMIT 1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) => match rows.first() {
            Some(r) => int_col(r, 0),
            None => Value::None,
        },
        Err(e) => {
            error!("character.GetCloneTypeID({char_id}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}
