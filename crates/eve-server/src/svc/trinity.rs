//! Calls the Trinity client makes that the 2006 client never did, answered
//! with what an empty but working system looks like, so the client's callers
//! (most of them on the way into the game) carry on rather than raise.
//!
//! Each answer is the shape its caller reads; the file and line are given.

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};

/// In-game voice (Vivox) is not available: audioService.Subscriber asks
/// whether this account may use it.
pub async fn voice_mgr(call: &Call, _ctx: &Ctx<'_>) -> Option<Outcome> {
    match call.method.as_str() {
        "VoiceEnabled" => Some(Outcome::Value(Value::Bool(false))),
        _ => None,
    }
}

/// `GetStationServiceStates`, asked for outposts and player-ownable stations
/// (station/base.py GetServiceStates): `{serviceID: state}`. No state means
/// every service as its station type defines it.
///
/// `GetNumberOfUnrentedOffices` (lobby.py, the Offices tab): the station
/// type's office slots less the offices rented there.
pub async fn corp_station_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    match call.method.as_str() {
        "GetStationServiceStates" => Some(Outcome::Value(Value::Dict(Vec::new()))),
        "GetNumberOfUnrentedOffices" => {
            let station = crate::svc::bound::parse(&call.service)
                .and_then(|b| b.params.first().copied())
                .or(ctx.session.station_id)
                .unwrap_or(0) as i32;
            let sql = r#"SELECT GREATEST(COALESCE(t."officeSlots", 0)
                                  - (SELECT count(*) FROM "crpOffices" o WHERE o."stationID" = s."stationID"), 0)::integer
                           FROM "staStations" s JOIN "staStationTypes" t ON t."stationTypeID" = s."stationTypeID"
                          WHERE s."stationID" = $1"#;
            let n: i32 = ctx.query_one(sql, &[&station]).await.map(|r| r.get(0)).unwrap_or(0);
            Some(Outcome::Value(Value::Int(n as i64)))
        }
        _ => None,
    }
}

/// standingsvc.__RefreshStandings: `(charstandings, charprime,
/// npccharstandings)`, and the same for the corporation. The first is indexed
/// by toID, the last by fromID; prime is owner rows for cfg.eveowners.
pub async fn standing2(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    const TO_HEADER: &[&str] = &["toID", "standing"];
    const FROM_HEADER: &[&str] = &["fromID", "standing"];
    const OWNER_HEADER: &[&str] = &["ownerID", "ownerName", "typeID"];
    let char_id = ctx.session.char_id.unwrap_or(0) as i32;
    let corp_id = ctx.session.corp_id.unwrap_or(0) as i32;
    let (mine, prime, npc, id) = match call.method.as_str() {
        "GetCharStandings" => (
            r#"SELECT "toID","standing" FROM "chrStandings" WHERE "characterID" = $1"#,
            r#"SELECT o."ownerID",o."ownerName",o."typeID" FROM "eveStaticOwners" o
                WHERE o."ownerID" IN (SELECT "toID" FROM "chrStandings" WHERE "characterID" = $1)"#,
            r#"SELECT "fromID","standing" FROM "chrNPCStandings" WHERE "characterID" = $1"#,
            char_id,
        ),
        "GetCorpStandings" => (
            r#"SELECT "toID","standing" FROM "ownerStandings" WHERE "fromID" = $1"#,
            r#"SELECT o."ownerID",o."ownerName",o."typeID" FROM "eveStaticOwners" o
                WHERE o."ownerID" IN (SELECT "toID" FROM "ownerStandings" WHERE "fromID" = $1)"#,
            r#"SELECT "fromID","standing" FROM "npcStandings" WHERE "toID" = $1"#,
            corp_id,
        ),
        _ => return None,
    };
    Some(Outcome::Value(Value::Tuple(vec![
        ctx.rowset(TO_HEADER, mine, &[&id]).await,
        ctx.rowset(OWNER_HEADER, prime, &[&id]).await,
        ctx.rowset(FROM_HEADER, npc, &[&id]).await,
    ])))
}

/// tutorialsvc.GetContextHelp: rows of (keywords, description, url) for the
/// help links in window captions. None are defined; the client treats an
/// empty answer as no help.
pub async fn tutorial_svc(call: &Call, _ctx: &Ctx<'_>) -> Option<Outcome> {
    match call.method.as_str() {
        "GetContextHelp" => Some(Outcome::Value(Value::List(Vec::new()))),
        _ => None,
    }
}

/// The channel a pilot with the rookie role is put in at login
/// (lscengine.py): "Help\Rookie Help", channel 1 in channelsStatic.
pub async fn lsc(call: &Call, _ctx: &Ctx<'_>) -> Option<Outcome> {
    match call.method.as_str() {
        "GetRookieHelpChannel" => Some(Outcome::Value(Value::Int(1))),
        _ => None,
    }
}

/// `character.GetFactions`: rows read as `faction.factionID` (factionsvc.py
/// GetFactionEx).
pub async fn character(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    match call.method.as_str() {
        // The creator's keep-alive between steps (nextgencharcreation_base.py
        // DoPingServer); the client logs the answer as the user ID.
        "Ping" => Some(Outcome::Value(Value::int_or_none(ctx.session.user_id))),
        "GetFactions" => {
            const HEADER: &[&str] = &[
                "factionID", "factionName", "description", "raceIDs", "solarSystemID", "corporationID",
                "sizeFactor", "stationCount", "stationSystemCount",
            ];
            let sql = r#"SELECT "factionID","factionName","description","raceIDs","solarSystemID","corporationID",
                                "sizeFactor","stationCount","stationSystemCount"
                           FROM "chrFactions" ORDER BY "factionID""#;
            Some(Outcome::Value(ctx.rowset(HEADER, sql, &[]).await))
        }
        _ => None,
    }
}
