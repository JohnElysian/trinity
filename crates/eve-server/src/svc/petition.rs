//! `petitioner`: the help window's petitions (ui/shared/neocom/petition.py).
//!
//! A pilot files a petition in a category; it waits in its category's queue
//! until a petitionee (a session with ROLE_PETITIONEE) claims it. The two
//! then trade messages, each side hearing OnPetitionMessage as the other
//! writes; a petitionee may add comments, which the pilot never sees. The
//! petitionee closes it, hands it back, or escalates it to the queue the
//! category names. Every step is written to the petition's log.
//!
//! The categories, their extra fields and the queues are server data the
//! client only reads; db/petitionCategories.sql.gz and its neighbours hold a
//! set made for this server. Categories 1 and -5 open a web page in the
//! client and are left out; 4 and 5 carry the pilot's chat logs, and 7 no
//! combat log.

use std::collections::HashMap;

use eve_marshal::Value;
use tracing::info;

use crate::services::{Call, Ctx, Outcome};
use crate::session::ROLE_PETITIONEE;
use crate::svc::membership::{db_failed, message, Refused};
use crate::svc::rows::{int, line, row, rowset, text};

/// const.maxPetitionsPerDay.
const MAX_PER_DAY: i64 = 8;
const DAY: i64 = 24 * 3600 * 10_000_000;
const TEXT_MAX: usize = 10_000;
/// const.petitionMaxChatLogSize and petitionMaxCombatLogSize.
const LOG_MAX: usize = 200_000;

pub async fn ensure(db: &tokio_postgres::Client) -> Result<(), tokio_postgres::Error> {
    db.batch_execute(r#"
      ALTER TABLE petitions ADD COLUMN IF NOT EXISTS rating integer;
      ALTER TABLE petitions ADD COLUMN IF NOT EXISTS "ratingComment" text NOT NULL DEFAULT '';
      ALTER TABLE petitions ADD COLUMN IF NOT EXISTS "ratingDateTime" bigint;
      CREATE TABLE IF NOT EXISTS "petitionCategoryGroups" ("parentCategoryID" integer PRIMARY KEY,"displayName" text NOT NULL);
      INSERT INTO "petitionCategoryGroups" VALUES (1,'Game support') ON CONFLICT DO NOTHING;
      ALTER TABLE "petitionCategories" ADD COLUMN IF NOT EXISTS "parentCategoryID" integer NOT NULL DEFAULT 1;
      ALTER TABLE "petitionCategoryProperties" ADD COLUMN IF NOT EXISTS "propertyID" serial;
      CREATE UNIQUE INDEX IF NOT EXISTS petition_property_id ON "petitionCategoryProperties" ("propertyID");
      ALTER TABLE "petitionCategoryProperties" ADD COLUMN IF NOT EXISTS required boolean NOT NULL DEFAULT true;
      ALTER TABLE "petitionCategoryProperties" ADD COLUMN IF NOT EXISTS description text NOT NULL DEFAULT '';
      ALTER TABLE "petitionCategoryProperties" ADD COLUMN IF NOT EXISTS "inputType" text NOT NULL DEFAULT 'EditBox';
      ALTER TABLE "petitionCategoryProperties" ADD COLUMN IF NOT EXISTS "inputInfo" text NOT NULL DEFAULT '';
    "#).await
}

/// What the log records, as petitionLog.actionID, with the text it shows.
#[derive(Clone, Copy)]
enum Action {
    Created = 1,
    Claimed,
    Unclaimed,
    Escalated,
    Closed,
    Cancelled,
    Deleted,
    Message,
    Comment,
}

impl Action {
    fn text(self) -> &'static str {
        match self {
            Action::Created => "Petition created",
            Action::Claimed => "Petition claimed",
            Action::Unclaimed => "Petition unclaimed",
            Action::Escalated => "Petition escalated",
            Action::Closed => "Petition closed",
            Action::Cancelled => "Petition cancelled",
            Action::Deleted => "Petition deleted",
            Action::Message => "Message sent",
            Action::Comment => "Comment added",
        }
    }
}

/// The petition as the list entries and the view window read it
/// (PetitionField.Load and ShowPetition). touchDate is the last message, or
/// the filing; updated is 1 when the petitionee spoke last, which the queue
/// shows as "Waiting".
const PETITION_HEADER: &[&str] = &[
    "petitionID",
    "petitionerID",
    "categoryID",
    "queueID",
    "subject",
    "petition",
    "createDate",
    "touchDate",
    "claimed",
    "claimedBy",
    "closed",
    "closedAt",
    "deleted",
    "escalatesTo",
    "updated",
    "rating",
    "ratingComment",
    "ratingDateTime",
    "isOnline",
    "properties",
];
const PETITION_SELECT: &str = r#"SELECT p."petitionID", p."petitionerID", p."categoryID", p."queueID", p."subject",
        p."petition", p."createdAt",
        COALESCE((SELECT max(m."created") FROM "petitionMessages" m WHERE m."petitionID" = p."petitionID"), p."createdAt"),
        p."claimed", p."claimedBy", p."closed", p."closedAt", p."deleted", p."escalatesTo",
        COALESCE((SELECT m."isPetitionee" FROM "petitionMessages" m
                   WHERE m."petitionID" = p."petitionID" AND m."comment" = 0
                   ORDER BY m."messageID" DESC LIMIT 1), 0), p.rating,p."ratingComment",p."ratingDateTime"
   FROM "petitions" p"#;
const MESSAGE_HEADER: &[&str] = &[
    "messageID",
    "petitionID",
    "senderID",
    "text",
    "sentDate",
    "comment",
    "read",
];
const LOG_HEADER: &[&str] = &[
    "logID",
    "petitionID",
    "logDateTime",
    "characterID",
    "eventText",
    "userName",
];

pub async fn petitioner(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let me = ctx.session.char_id.unwrap_or(0);
    let id = call.arg_id(0);
    let result = match call.method.as_str() {
        // CheckNewMessages reads .petitionID, .text and .messageID.
        "GetUnreadMessages" => {
            let sql = r#"SELECT m."messageID", m."petitionID", m."text" FROM "petitionMessages" m
                           JOIN "petitions" p ON p."petitionID" = m."petitionID"
                          WHERE p."petitionerID" = $1 AND m."isPetitionee" = 1 AND m."comment" = 0 AND m."read" = 0
                          ORDER BY m."messageID""#;
            Ok(ctx.rowset(&["messageID", "petitionID", "text"], sql, &[&(me as i32)]).await)
        }
        "MarkAsRead" => {
            let sql = r#"UPDATE "petitionMessages" m SET "read" = 1 FROM "petitions" p
                          WHERE m."messageID" = $1 AND p."petitionID" = m."petitionID"
                            AND (p."petitionerID" = $2 OR p."claimedBy" = $2)"#;
            ctx.execute(sql, &[&(id as i32), &(me as i32)]).await;
            Ok(Value::None)
        }
        "GetMyPetitions" => Ok(petitions(ctx, r#"WHERE p."petitionerID" = $1 AND p."deleted" = 0"#, me).await),
        "GetMyPetitionsEx" => Ok(petitions(ctx,&format!(r#"WHERE (p."petitionerID"=$1 OR p."accountID"={}) AND p.deleted=0"#,ctx.session.user_id.unwrap_or(-1)),me).await),
        "AddPetitionRating" | "UpdatePetitionRating" => rating(call,ctx).await,
        "GetClaimedPetitions" => match petitionee(ctx) {
            Ok(()) => Ok(petitions(ctx, r#"WHERE p."claimedBy" = $1 AND p."claimed" = 1 AND p."closed" = 0"#, me).await),
            Err(e) => Err(e),
        },
        "GetPetitionQueue" => match petitionee(ctx) {
            Ok(()) => Ok(petitions(ctx, r#"WHERE p."queueID" = $1 AND p."claimed" = 0 AND p."closed" = 0 AND p."deleted" = 0"#, id).await),
            Err(e) => Err(e),
        },
        "GetPetition" => get_petition(ctx, id).await,
        "GetPetitionMessages" => messages(ctx, id).await,
        "GetPetitionLog" => log_of(ctx, id).await,
        "GetLog" => log_ids(ctx,id).await,
        "GetEvents" => Ok(rowset(&["eventID","eventText"],[Action::Created,Action::Claimed,Action::Unclaimed,Action::Escalated,Action::Closed,Action::Cancelled,Action::Deleted,Action::Message,Action::Comment].iter().map(|a|Value::List(vec![Value::Int(*a as i64),Value::text(a.text())])).collect())),
        "GetCategoryHierarchicalInfo" => hierarchy(ctx).await,
        "PropertyPopulationInfo" | "GetClientPickerInfo" => population(call,ctx).await,
        "GetCategories" => {
            let sql = r#"SELECT "categoryID","displayName","description","escalatesTo"
                           FROM "petitionCategories" ORDER BY "categoryID""#;
            Ok(ctx.rowset(&["categoryID", "displayName", "description", "escalatesTo"], sql, &[]).await)
        }
        "GetCategoryName" => {
            let sql = r#"SELECT "displayName" FROM "petitionCategories" WHERE "categoryID" = $1"#;
            let name = ctx.query_one(sql, &[&(id as i32)]).await.and_then(|r| text(&r, 0));
            Ok(Value::text(name.unwrap_or_default()))
        }
        "GetCategoryProperties" => {
            let sql = r#"SELECT k,"propertyID",k,required,COALESCE(NULLIF(description,''),k),"inputType","inputInfo" FROM "petitionCategoryProperties" WHERE "categoryID" = $1 ORDER BY "orderID""#;
            Ok(ctx.rowset(&["k","propertyID","propertyName","required","description","inputType","inputInfo"], sql, &[&(id as i32)]).await)
        }
        "GetQueues" => {
            let sql = r#"SELECT "queueID","queueName" FROM "petitionQueues" ORDER BY "queueID""#;
            Ok(ctx.rowset(&["queueID", "queueName"], sql, &[]).await)
        }
        "MayPetition" => Ok(Value::Int(may_petition(ctx, id, call.arg_i64(1)).await)),
        "CreatePetition" => create(ctx, call).await,
        "PetitionerChat" => chat(ctx, id, &call.arg_text(1).unwrap_or_default(), false, false).await,
        "PetitioneeChat" => {
            let comment = call.arg_i64(2).unwrap_or(0) != 0;
            chat(ctx, id, &call.arg_text(1).unwrap_or_default(), true, comment).await
        }
        "ClaimPetition" => claim(ctx, id).await,
        "UnClaimPetition" => unclaim(ctx, id, None).await,
        "EscalatePetition" => unclaim(ctx, id, Some(call.arg_i64(1))).await,
        "ClosePetition" => close(ctx, id).await,
        "CancelPetition" => withdraw(ctx, id, Action::Cancelled).await,
        "DeletePetition" => withdraw(ctx, id, Action::Deleted).await,
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

fn petitionee(ctx: &Ctx<'_>) -> Refused<()> {
    if ctx.session.role & ROLE_PETITIONEE == ROLE_PETITIONEE {
        Ok(())
    } else {
        Err(Outcome::custom("Only petitionees may do that."))
    }
}

async fn rating(call: &Call, ctx: &Ctx<'_>) -> Refused<Value> {
    let value = call.arg_id(1);
    let comment = call.arg_text(2).unwrap_or_default();
    if !(0..=100).contains(&value) || value % 10 != 0 || comment.chars().count() > 1000 {
        return Err(Outcome::custom(
            "Choose a rating from 0 to 100 and a comment of at most 1000 characters.",
        ));
    }
    let now = crate::bulkdata::filetime_now();
    let add = call.method == "AddPetitionRating";
    let n=ctx.db.execute(r#"UPDATE petitions SET rating=$3,"ratingComment"=$4,"ratingDateTime"=COALESCE("ratingDateTime",$5)
          WHERE "petitionID"=$1 AND "petitionerID"=$2 AND closed=1 AND deleted=0
            AND (($6 AND "ratingDateTime" IS NULL) OR (NOT $6 AND "ratingDateTime">$7))"#,
        &[&(call.arg_id(0) as i32),&(ctx.session.char_id.unwrap_or(0) as i32),&(value as i32),&comment,&now,&add,&(now-7*DAY)]).await.map_err(|e|db_failed("saving the petition rating",e))?;
    if n == 0 {
        return Err(Outcome::custom("Only your own closed petitions can be rated; existing ratings can be edited for one week."));
    }
    Ok(Value::None)
}

async fn hierarchy(ctx: &Ctx<'_>) -> Refused<Value> {
    let groups=ctx.db.query(r#"SELECT "parentCategoryID","displayName" FROM "petitionCategoryGroups" ORDER BY "parentCategoryID""#,&[]).await.map_err(|e|db_failed("reading category groups",e))?;
    let categories=ctx.db.query(r#"SELECT "categoryID","parentCategoryID","displayName",description FROM "petitionCategories" ORDER BY "categoryID""#,&[]).await.map_err(|e|db_failed("reading categories",e))?;
    let parents = groups
        .iter()
        .map(|r| {
            (
                Value::Int(int(r, 0).unwrap_or(0)),
                Value::text(text(r, 1).unwrap_or_default()),
            )
        })
        .collect();
    let mut children: std::collections::BTreeMap<i64, Vec<(Value, Value)>> =
        std::collections::BTreeMap::new();
    let mut descriptions = Vec::new();
    for r in categories {
        let id = Value::Int(int(&r, 0).unwrap_or(0));
        children
            .entry(int(&r, 1).unwrap_or(1))
            .or_default()
            .push((id.clone(), Value::text(text(&r, 2).unwrap_or_default())));
        descriptions.push((id, Value::text(text(&r, 3).unwrap_or_default())));
    }
    Ok(Value::Tuple(vec![
        Value::Dict(parents),
        Value::Dict(
            children
                .into_iter()
                .map(|(id, entries)| (Value::Int(id), Value::Dict(entries)))
                .collect(),
        ),
        Value::Dict(descriptions),
    ]))
}

async fn log_ids(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    petitionee(ctx)?;
    parties(ctx, id).await?;
    Ok(ctx.rowset(&["logID","petitionID","logDateTime","characterID","eventID","userName"],r#"SELECT l."logID",l."petitionID",l."logDateTime",l."actorID",l."actionID",COALESCE(a."accountName",'') FROM "petitionLog" l LEFT JOIN "character_" c ON c."characterID"=l."actorID" LEFT JOIN account a ON a."accountID"=c."accountID" WHERE l."petitionID"=$1 ORDER BY l."logID""#,&[&(id as i32)]).await)
}

async fn population(call: &Call, ctx: &Ctx<'_>) -> Refused<Value> {
    let picker = call.method == "GetClientPickerInfo";
    let input = if picker {
        let property = call.arg_text(1).unwrap_or_default();
        ctx.db.query_opt(r#"SELECT "inputInfo" FROM "petitionCategoryProperties" WHERE k=$1 AND "inputType"='Picker' ORDER BY "propertyID" LIMIT 1"#,&[&property]).await.map_err(|e|db_failed("reading picker configuration",e))?.map(|r|r.get::<_,String>(0)).unwrap_or(property)
    } else {
        call.arg_text(0).unwrap_or_default()
    };
    let filter = if picker {
        call.arg_text(0).unwrap_or_default().to_lowercase()
    } else {
        String::new()
    };
    if picker && filter.chars().count() < 3 {
        return Ok(Value::List(vec![]));
    }
    let (table, id, name) = match input.to_lowercase().as_str() {
        "character" | "characters" | "pilot name" => {
            ("\"character_\"", "\"characterID\"", "\"characterName\"")
        }
        "corporation" | "corporations" => {
            ("corporation", "\"corporationID\"", "\"corporationName\"")
        }
        "solar system" | "solarsystem" | "solarsystems" => (
            "\"mapSolarSystems\"",
            "\"solarSystemID\"",
            "\"solarSystemName\"",
        ),
        "station" | "stations" => ("\"staStations\"", "\"stationID\"", "\"stationName\""),
        "type" | "types" | "item" | "items" => ("\"invTypes\"", "\"typeID\"", "\"typeName\""),
        _ => return Ok(Value::List(vec![])),
    };
    let own = if !picker && table == "\"character_\"" {
        format!(" AND \"accountID\"={}", ctx.session.user_id.unwrap_or(-1))
    } else {
        String::new()
    };
    let sql=format!("SELECT {id},{name} FROM {table} WHERE strpos(lower({name}),$1)>0{own} ORDER BY {name} LIMIT 25");
    let rows = ctx
        .db
        .query(&sql, &[&filter])
        .await
        .map_err(|e| db_failed("populating petition choices", e))?;
    Ok(Value::List(
        rows.iter()
            .map(|r| {
                let mut cells = line(r);
                if picker {
                    cells.swap(0, 1)
                }
                Value::Tuple(cells)
            })
            .collect(),
    ))
}

/// Petitions matching `filter` (which takes one parameter), each with whether
/// its pilot is online and its extra fields.
async fn petitions(ctx: &Ctx<'_>, filter: &str, param: i64) -> Value {
    let sql = format!(r#"{PETITION_SELECT} {filter} ORDER BY p."petitionID""#);
    let rows = ctx.query(&sql, &[&(param as i32)]).await;
    let ids: Vec<i32> = rows
        .iter()
        .filter_map(|r| int(r, 0))
        .map(|i| i as i32)
        .collect();
    let mut properties: HashMap<i64, Vec<(Value, Value)>> = HashMap::new();
    let sql = r#"SELECT "petitionID", "k", "v" FROM "petitionProperties" WHERE "petitionID" = ANY($1) ORDER BY "k""#;
    for r in ctx.query(sql, &[&ids]).await {
        let (k, v) = (
            text(&r, 1).unwrap_or_default(),
            text(&r, 2).unwrap_or_default(),
        );
        properties
            .entry(int(&r, 0).unwrap_or(0))
            .or_default()
            .push((Value::text(k), Value::text(v)));
    }
    let world = ctx.world.lock();
    let lines = rows
        .iter()
        .map(|r| {
            let mut l = line(r);
            let id = int(r, 0).unwrap_or(0);
            l.push(Value::Int(i64::from(
                world.is_online(int(r, 1).unwrap_or(0)),
            )));
            l.push(properties.remove(&id).map_or(Value::None, Value::Dict));
            Value::List(l)
        })
        .collect();
    rowset(PETITION_HEADER, lines)
}

/// A petition's petitioner and claimant, if the caller may see it: its
/// petitioner, or a petitionee.
async fn parties(ctx: &Ctx<'_>, id: i64) -> Refused<Parties> {
    let sql = r#"SELECT "petitionerID", "claimedBy", "claimed", "closed", "escalatesTo" FROM "petitions"
                  WHERE "petitionID" = $1 AND "deleted" = 0"#;
    let r = ctx
        .query_one(sql, &[&(id as i32)])
        .await
        .ok_or_else(|| Outcome::custom("That petition does not exist."))?;
    let p = Parties {
        petitioner: int(&r, 0).unwrap_or(0),
        claimant: int(&r, 1).filter(|_| int(&r, 2) == Some(1)),
        closed: int(&r, 3) == Some(1),
        escalates_to: int(&r, 4),
    };
    if Some(p.petitioner) != ctx.session.char_id && petitionee(ctx).is_err() {
        return Err(Outcome::custom("That is not your petition."));
    }
    Ok(p)
}

struct Parties {
    petitioner: i64,
    claimant: Option<i64>,
    closed: bool,
    escalates_to: Option<i64>,
}

async fn get_petition(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    parties(ctx, id).await?;
    let found = petitions(ctx, r#"WHERE p."petitionID" = $1"#, id).await;
    let line = match &found {
        Value::Instance { state, .. } => match state.as_ref() {
            Value::Dict(d) => d
                .iter()
                .find(|(k, _)| *k == Value::bytes("lines"))
                .and_then(|(_, v)| v.seq())
                .and_then(|l| l.first().cloned()),
            _ => None,
        },
        _ => None,
    };
    Ok(match line {
        Some(Value::List(l)) => row(PETITION_HEADER, l),
        _ => Value::None,
    })
}

/// The messages; comments only for petitionees.
async fn messages(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    parties(ctx, id).await?;
    let comments = i16::from(petitionee(ctx).is_ok());
    let sql = r#"SELECT "messageID", "petitionID", "senderID", "text", "created", "comment", "read"
                   FROM "petitionMessages" WHERE "petitionID" = $1 AND "comment" <= $2 ORDER BY "messageID""#;
    Ok(ctx
        .rowset(MESSAGE_HEADER, sql, &[&(id as i32), &comments])
        .await)
}

async fn log_of(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    petitionee(ctx)?;
    let sql = r#"SELECT l."logID", l."petitionID", l."logDateTime", l."actorID", COALESCE(l."message", ''),
                        COALESCE(a."accountName", '')
                   FROM "petitionLog" l
                   LEFT JOIN "character_" c ON c."characterID" = l."actorID"
                   LEFT JOIN "account" a ON a."accountID" = c."accountID"
                  WHERE l."petitionID" = $1 ORDER BY l."logID""#;
    Ok(ctx.rowset(LOG_HEADER, sql, &[&(id as i32)]).await)
}

/// MayPetition(categoryID, OocCharacterID): 1, or the client's refusals:
/// -1 one open petition per category, -2 too many today, -3 no character,
/// -4 not one of the account's characters.
async fn may_petition(ctx: &Ctx<'_>, category: i64, ooc: Option<i64>) -> i64 {
    let Some(char_id) = petitioner_of(ctx, ooc).await else {
        return if ctx.session.char_id.is_none() && ooc.is_none() {
            -3
        } else {
            -4
        };
    };
    let open = r#"SELECT 1 FROM "petitions" WHERE "petitionerID" = $1 AND "categoryID" = $2 AND "closed" = 0 AND "deleted" = 0"#;
    if ctx
        .query_one(open, &[&(char_id as i32), &(category as i32)])
        .await
        .is_some()
    {
        return -1;
    }
    let today =
        r#"SELECT count(*) FROM "petitions" WHERE "petitionerID" = $1 AND "createdAt" > $2"#;
    let since = crate::bulkdata::filetime_now() - DAY;
    let count = ctx
        .query_one(today, &[&(char_id as i32), &since])
        .await
        .and_then(|r| int(&r, 0))
        .unwrap_or(0);
    if count >= MAX_PER_DAY {
        return -2;
    }
    1
}

/// The character a petition is filed for: the session's, or (filed from
/// character selection) one of the account's.
async fn petitioner_of(ctx: &Ctx<'_>, ooc: Option<i64>) -> Option<i64> {
    if let Some(char_id) = ctx.session.char_id {
        return Some(char_id);
    }
    let ooc = ooc?;
    let sql = r#"SELECT 1 FROM "character_" WHERE "characterID" = $1 AND "accountID" = $2"#;
    ctx.query_one(
        sql,
        &[&(ooc as i32), &(ctx.session.user_id.unwrap_or(0) as i32)],
    )
    .await
    .map(|_| ooc)
}

/// CreatePetition(subject, petition, categoryID, properties, OocCharacterID,
/// combatLog=None, chatLog=None): files it in its category's first queue.
async fn create(ctx: &Ctx<'_>, call: &Call) -> Refused<Value> {
    let category = call.arg_id(2);
    let Some(char_id) = petitioner_of(ctx, call.arg_i64(4)).await else {
        return Ok(Value::Bool(false));
    };
    let subject = call.arg_text(0).unwrap_or_default();
    let body = call.arg_text(1).unwrap_or_default();
    if subject.trim().is_empty()
        || subject.chars().count() > 200
        || body.trim().is_empty()
        || body.chars().count() > TEXT_MAX
    {
        return Err(Outcome::custom("Enter a subject of at most 200 characters and petition text of at most 10000 characters."));
    }
    let log = |name: &str, index: usize| {
        call.kwarg(name)
            .or(call.args.get(index))
            .and_then(Value::as_string)
            .map(|s| {
                s.chars()
                    .rev()
                    .take(LOG_MAX)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>()
            })
    };
    let chat = log("chatLog", 5);
    let combat = log("combatLog", 6);
    let mut client = crate::db::connect(ctx.db_url)
        .await
        .map_err(|e| db_failed("connecting to file the petition", e))?;
    let tx = client
        .transaction()
        .await
        .map_err(|e| db_failed("starting the petition", e))?;
    tx.query_one("SELECT pg_advisory_xact_lock($1)", &[&char_id])
        .await
        .map_err(|e| db_failed("checking petition limits", e))?;
    let now = crate::bulkdata::filetime_now();
    let count=tx.query_one(r#"SELECT count(*) FILTER (WHERE "categoryID"=$2 AND closed=0 AND deleted=0),count(*) FILTER (WHERE "createdAt">$3) FROM petitions WHERE "petitionerID"=$1"#,&[&(char_id as i32),&(category as i32),&(now-DAY)]).await.map_err(|e|db_failed("checking petition limits",e))?;
    if count.get::<_, i64>(0) > 0 || count.get::<_, i64>(1) >= MAX_PER_DAY {
        return Ok(Value::Bool(false));
    }
    let definitions=tx.query(r#"SELECT "propertyID",k,required FROM "petitionCategoryProperties" WHERE "categoryID"=$1 ORDER BY "orderID""#,&[&(category as i32)]).await.map_err(|e|db_failed("reading petition fields",e))?;
    let mut values: HashMap<String, String> = HashMap::new();
    if let Some(Value::Dict(fields)) = call.args.get(3) {
        for (k, v) in fields {
            if let (Some(k), Some(v)) = (
                k.as_string(),
                v.as_string().or_else(|| v.as_i64().map(|n| n.to_string())),
            ) {
                values.insert(k, v);
            }
        }
    }
    if let Some(entries) = call.args.get(7).and_then(Value::seq) {
        for entry in entries {
            let pair = entry
                .seq()
                .filter(|p| p.len() == 2)
                .ok_or_else(|| Outcome::custom("Invalid petition property."))?;
            let id = pair[0].as_i64().unwrap_or(0);
            let definition = definitions
                .iter()
                .find(|r| int(r, 0) == Some(id))
                .ok_or_else(|| {
                    Outcome::custom("That field does not belong to this petition category.")
                })?;
            let value = pair[1]
                .as_string()
                .or_else(|| pair[1].as_i64().map(|n| n.to_string()))
                .unwrap_or_default();
            values.insert(text(definition, 1).unwrap_or_default(), value);
        }
    }
    for definition in &definitions {
        let name = text(definition, 1).unwrap_or_default();
        let value = values.get(&name).map(String::as_str).unwrap_or("");
        if definition.get::<_, bool>(2) && (value.trim().is_empty() || value == "-1") {
            return Err(Outcome::custom(format!(
                "Complete the required field: {name}."
            )));
        }
        if value.chars().count() > 200 {
            return Err(Outcome::custom(format!("The {name} field is too long.")));
        }
    }
    let account = ctx.session.user_id.unwrap_or(0) as i32;
    let id=tx.query_opt(r#"INSERT INTO petitions ("petitionerID","accountID","categoryID","queueID",subject,petition,"createdAt","escalatesTo","combatLog","chatLog") SELECT $1,$2,c."categoryID",(SELECT min("queueID") FROM "petitionQueues"),$4,$5,$6,c."escalatesTo",$7,$8 FROM "petitionCategories" c WHERE c."categoryID"=$3 RETURNING "petitionID""#,&[&(char_id as i32),&account,&(category as i32),&subject,&body,&now,&combat,&chat]).await.map_err(|e|db_failed("filing the petition",e))?.map(|r|r.get::<_,i32>(0)).ok_or_else(||Outcome::custom("That petition category does not exist."))?;
    for definition in definitions {
        let key = text(&definition, 1).unwrap_or_default();
        if let Some(value) = values.get(&key) {
            tx.execute(
                r#"INSERT INTO "petitionProperties" ("petitionID",k,v) VALUES ($1,$2,$3)"#,
                &[&id, &key, value],
            )
            .await
            .map_err(|e| db_failed("saving the petition fields", e))?;
        }
    }
    tx.execute(r#"INSERT INTO "petitionLog" ("petitionID","logDateTime","actorID","actionID",message) VALUES ($1,$2,$3,1,$4)"#,&[&id,&now,&(char_id as i32),&Action::Created.text()]).await.map_err(|e|db_failed("recording the petition",e))?;
    tx.commit()
        .await
        .map_err(|e| db_failed("committing the petition", e))?;
    info!("{char_id} filed petition {id} in category {category}: {subject:?}");
    let w = ctx.world.lock();
    w.notify(
        &w.with_role(ROLE_PETITIONEE),
        "OnNewPetition",
        Value::Tuple(vec![
            Value::Int(id as i64),
            Value::Int(char_id),
            Value::text(subject),
            Value::Int(category),
        ]),
    );
    Ok(Value::Bool(true))
}

/// A message from the pilot, or from the petitionee who has it (a comment
/// stays among petitionees). Both sides hear it, the writer included, so an
/// open view reloads.
async fn chat(
    ctx: &Ctx<'_>,
    id: i64,
    text_in: &str,
    from_petitionee: bool,
    comment: bool,
) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let p = parties(ctx, id).await?;
    if from_petitionee {
        petitionee(ctx)?;
    } else if p.petitioner != me {
        return Err(Outcome::custom("That is not your petition."));
    }
    if p.closed {
        return Err(message("PetitionClosed"));
    }
    let body: String = text_in.chars().take(TEXT_MAX).collect();
    let sql = r#"INSERT INTO "petitionMessages" ("petitionID", "senderID", "isPetitionee", "text", "created", "comment")
                 VALUES ($1, $2, $3, $4, $5, $6) RETURNING "messageID""#;
    let now = crate::bulkdata::filetime_now();
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 6] = [
        &(id as i32),
        &(me as i32),
        &i16::from(from_petitionee),
        &body,
        &now,
        &i16::from(comment),
    ];
    let message_id = ctx
        .query_one(sql, &params)
        .await
        .and_then(|r| int(&r, 0))
        .ok_or_else(|| Outcome::custom("The message could not be saved."))?;
    record(
        ctx,
        id,
        me,
        if comment {
            Action::Comment
        } else {
            Action::Message
        },
    )
    .await;
    let mut to = vec![me];
    to.extend(p.claimant);
    if !comment {
        to.push(p.petitioner);
    }
    tell(
        ctx,
        &to,
        "OnPetitionMessage",
        vec![
            Value::Int(id),
            Value::Int(me),
            Value::text(&body),
            Value::Int(message_id),
        ],
    );
    Ok(Value::None)
}

/// ClaimPetition(petitionID): 1 if the caller now has it, 0 if someone else
/// got there first.
async fn claim(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    petitionee(ctx)?;
    let me = ctx.session.char_id.unwrap_or(0);
    let sql = r#"UPDATE "petitions" SET "claimed" = 1, "claimedBy" = $2
                  WHERE "petitionID" = $1 AND "claimed" = 0 AND "closed" = 0 AND "deleted" = 0 AND "petitionerID" <> $2
                 RETURNING "petitionerID""#;
    let Some(petitioner) = ctx
        .query_one(sql, &[&(id as i32), &(me as i32)])
        .await
        .and_then(|r| int(&r, 0))
    else {
        return Ok(Value::Int(0));
    };
    record(ctx, id, me, Action::Claimed).await;
    info!("{me} claimed petition {id}");
    tell(
        ctx,
        &[petitioner],
        "OnAssignPetition",
        vec![Value::Int(id), Value::Int(me)],
    );
    Ok(Value::Int(1))
}

/// UnClaimPetition(petitionID) puts it back in its queue;
/// EscalatePetition(petitionID, escalatesTo) into the queue above.
async fn unclaim(ctx: &Ctx<'_>, id: i64, escalate: Option<Option<i64>>) -> Refused<Value> {
    petitionee(ctx)?;
    let me = ctx.session.char_id.unwrap_or(0);
    let p = parties(ctx, id).await?;
    if p.claimant != Some(me) {
        return Err(Outcome::custom("You have not claimed that petition."));
    }
    let queue = match escalate {
        None => None,
        Some(to) => Some(
            to.or(p.escalates_to)
                .ok_or_else(|| Outcome::custom("That petition cannot be escalated any further."))?,
        ),
    };
    let sql = r#"UPDATE "petitions" SET "claimed" = 0, "claimedBy" = NULL,
                        "queueID" = COALESCE($2, "queueID"),
                        "escalatesTo" = CASE WHEN $2::int IS NULL THEN "escalatesTo" ELSE NULL END
                  WHERE "petitionID" = $1"#;
    ctx.execute(sql, &[&(id as i32), &queue.map(|q| q as i32)])
        .await;
    record(
        ctx,
        id,
        me,
        if queue.is_some() {
            Action::Escalated
        } else {
            Action::Unclaimed
        },
    )
    .await;
    info!(
        "{me} {} petition {id}",
        if queue.is_some() {
            "escalated"
        } else {
            "unclaimed"
        }
    );
    tell(
        ctx,
        &[p.petitioner],
        "OnAssignPetition",
        vec![Value::Int(id), Value::None],
    );
    Ok(Value::None)
}

async fn close(ctx: &Ctx<'_>, id: i64) -> Refused<Value> {
    petitionee(ctx)?;
    let me = ctx.session.char_id.unwrap_or(0);
    let p = parties(ctx, id).await?;
    let sql = r#"UPDATE "petitions" SET "closed" = 1, "closedAt" = $2 WHERE "petitionID" = $1 AND "closed" = 0"#;
    ctx.execute(sql, &[&(id as i32), &crate::bulkdata::filetime_now()])
        .await;
    record(ctx, id, me, Action::Closed).await;
    info!("{me} closed petition {id}");
    tell(
        ctx,
        &[p.petitioner],
        "OnClosePetition",
        vec![Value::Int(id)],
    );
    Ok(Value::None)
}

/// The pilot takes back a petition nobody has claimed (CancelPetition), or
/// clears away a closed one (DeletePetition).
async fn withdraw(ctx: &Ctx<'_>, id: i64, how: Action) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let p = parties(ctx, id).await?;
    if p.petitioner != me {
        return Err(Outcome::custom("That is not your petition."));
    }
    let sql = match how {
        Action::Cancelled => {
            r#"UPDATE "petitions" SET "deleted" = 1, "closed" = 1, "closedAt" = $2 WHERE "petitionID" = $1 AND "claimed" = 0"#
        }
        _ => {
            r#"UPDATE "petitions" SET "deleted" = 1, "closedAt" = COALESCE("closedAt", $2) WHERE "petitionID" = $1 AND "closed" = 1"#
        }
    };
    if ctx
        .db
        .execute(sql, &[&(id as i32), &crate::bulkdata::filetime_now()])
        .await
        .unwrap_or(0)
        == 0
    {
        return Err(Outcome::custom(match how {
            Action::Cancelled => "A petition someone has claimed cannot be cancelled.",
            _ => "Only a closed petition can be deleted.",
        }));
    }
    record(ctx, id, me, how).await;
    info!("{me}: {} ({id})", how.text());
    Ok(Value::None)
}

async fn record(ctx: &Ctx<'_>, id: i64, actor: i64, what: Action) {
    let sql = r#"INSERT INTO "petitionLog" ("petitionID", "logDateTime", "actorID", "actionID", "message") VALUES ($1, $2, $3, $4, $5)"#;
    let now = crate::bulkdata::filetime_now();
    ctx.execute(
        sql,
        &[
            &(id as i32),
            &now,
            &(actor as i32),
            &(what as i32),
            &what.text(),
        ],
    )
    .await;
}

fn tell(ctx: &Ctx<'_>, chars: &[i64], event: &'static str, args: Vec<Value>) {
    let w = ctx.world.lock();
    let mut clients: Vec<i64> = chars.iter().flat_map(|c| w.clients_of(*c)).collect();
    clients.sort_unstable();
    clients.dedup();
    w.notify(&clients, event, Value::Tuple(args));
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(method: &str, args: Vec<Value>) -> Call {
        Call {
            service: "petitioner".into(),
            method: method.into(),
            args,
            kwargs: vec![],
        }
    }
    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn trinity_petition_properties_logs_and_rating_lifecycle() {
        let fixture = crate::test_support::Database::load().await;
        let pilot = fixture.id().await;
        let gm = fixture.id().await;
        for id in [pilot, gm] {
            fixture
                .db
                .execute(
                    r#"INSERT INTO "character_" ("characterID","characterName") VALUES ($1,$2)"#,
                    &[&id, &format!("Petition fixture {id}")],
                )
                .await
                .unwrap();
        }
        let player = crate::session::Session {
            char_id: Some(pilot as i64),
            ..Default::default()
        };
        let gm = crate::session::Session {
            char_id: Some(gm as i64),
            role: ROLE_PETITIONEE,
            ..Default::default()
        };
        let ctx = fixture.ctx(&player);
        let staff = fixture.ctx(&gm);
        let property=fixture.db.query_one(r#"SELECT "propertyID" FROM "petitionCategoryProperties" WHERE "categoryID"=2 ORDER BY "orderID" LIMIT 1"#,&[]).await.unwrap().get::<_,i32>(0);
        let args = vec![
            Value::bytes("Stuck ship"),
            Value::text("Help ".repeat(300)),
            Value::Int(2),
            Value::None,
            Value::None,
            Value::bytes("chat log"),
            Value::bytes("combat log"),
            Value::List(vec![Value::List(vec![
                Value::Int(property as i64),
                Value::Int(30000142),
            ])]),
        ];
        assert!(matches!(
            petitioner(&call("CreatePetition", args), &ctx).await,
            Some(Outcome::Value(Value::Bool(true)))
        ));
        let r=fixture.db.query_one(r#"SELECT "petitionID","chatLog","combatLog",length(petition) FROM petitions WHERE "petitionerID"=$1 ORDER BY "petitionID" DESC LIMIT 1"#,&[&pilot]).await.unwrap();
        let id = r.get::<_, i32>(0) as i64;
        assert_eq!(r.get::<_, String>(1), "chat log");
        assert_eq!(r.get::<_, String>(2), "combat log");
        assert!(r.get::<_, i32>(3) > 1000);
        let saved: String = fixture
            .db
            .query_one(
                r#"SELECT v FROM "petitionProperties" WHERE "petitionID"=$1"#,
                &[&(id as i32)],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(saved, "30000142");
        assert!(matches!(
            petitioner(&call("ClaimPetition", vec![Value::Int(id)]), &staff).await,
            Some(Outcome::Value(Value::Int(1)))
        ));
        assert!(matches!(
            petitioner(&call("ClosePetition", vec![Value::Int(id)]), &staff).await,
            Some(Outcome::Value(_))
        ));
        assert!(matches!(
            petitioner(
                &call(
                    "AddPetitionRating",
                    vec![
                        Value::Int(id),
                        Value::Int(80),
                        Value::bytes("Thank you"),
                        Value::Int(0)
                    ]
                ),
                &ctx
            )
            .await,
            Some(Outcome::Value(_))
        ));
        assert!(!matches!(
            petitioner(
                &call(
                    "UpdatePetitionRating",
                    vec![
                        Value::Int(id),
                        Value::Int(100),
                        Value::bytes("Other person's rating")
                    ]
                ),
                &staff
            )
            .await,
            Some(Outcome::Value(_))
        ));
        assert!(matches!(
            petitioner(
                &call(
                    "UpdatePetitionRating",
                    vec![Value::Int(id), Value::Int(90), Value::bytes("Updated")]
                ),
                &ctx
            )
            .await,
            Some(Outcome::Value(_))
        ));
        fixture
            .db
            .execute(
                r#"UPDATE petitions SET "ratingDateTime"=$2 WHERE "petitionID"=$1"#,
                &[&(id as i32), &(crate::bulkdata::filetime_now() - 8 * DAY)],
            )
            .await
            .unwrap();
        assert!(!matches!(
            petitioner(
                &call(
                    "UpdatePetitionRating",
                    vec![Value::Int(id), Value::Int(100), Value::bytes("Too late")]
                ),
                &ctx
            )
            .await,
            Some(Outcome::Value(_))
        ));
        for method in [
            "GetMyPetitionsEx",
            "GetCategories",
            "GetCategoryProperties",
            "GetCategoryHierarchicalInfo",
            "GetEvents",
        ] {
            assert!(
                matches!(
                    petitioner(&call(method, vec![Value::Int(2)]), &ctx).await,
                    Some(Outcome::Value(_))
                ),
                "{method}"
            );
        }
        assert!(matches!(
            petitioner(&call("GetLog", vec![Value::Int(id)]), &staff).await,
            Some(Outcome::Value(_))
        ));
        assert!(!matches!(
            petitioner(&call("GetLog", vec![Value::Int(id)]), &ctx).await,
            Some(Outcome::Value(_))
        ));
        assert!(matches!(
            petitioner(
                &call(
                    "PropertyPopulationInfo",
                    vec![Value::bytes("characters"), Value::None]
                ),
                &ctx
            )
            .await,
            Some(Outcome::Value(Value::List(_)))
        ));
        assert!(matches!(
            petitioner(
                &call(
                    "GetClientPickerInfo",
                    vec![Value::bytes("Jita"), Value::bytes("Solar system")]
                ),
                &ctx
            )
            .await,
            Some(Outcome::Value(Value::List(_)))
        ));
    }
}
