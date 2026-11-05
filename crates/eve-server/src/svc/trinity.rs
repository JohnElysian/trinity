//! Calls the Trinity client makes that the 2006 client never did, answered
//! with what an empty but working system looks like, so the client's callers
//! (most of them on the way into the game) carry on rather than raise.
//!
//! Each answer is the shape its caller reads; the file and line are given.

use std::collections::HashSet;

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Row};
use crate::svc::rows::rowset;

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
                .unwrap_or(0);
            // No count for a station that is gone or whose type the static
            // data does not know, as joining on them left it out.
            let n = ctx
                .store
                .read(|db| {
                    let station_type = db.table("staStations").get_id(station)?.int("stationTypeID")?;
                    let slots = crate::sde::table("staStationTypes").get("stationTypeID", station_type)?.int("officeSlots").unwrap_or(0);
                    let rented = db.table("crpOffices").find(&["stationID"], &[store::Value::Int(station)]).count() as i64;
                    Some((slots - rented).max(0))
                })
                .unwrap_or(0);
            Some(Outcome::Value(Value::Int(n)))
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
    let char_id = ctx.session.char_id.unwrap_or(0);
    let corp_id = ctx.session.corp_id.unwrap_or(0);
    // The standings table and the column holding whose they are, and where
    // the NPCs' standings toward them are kept.
    let (mine, owner, npc, id) = match call.method.as_str() {
        "GetCharStandings" => ("chrStandings", "characterID", Some("chrNPCStandings"), char_id),
        // NPC standings toward a corporation are static (npcStandings).
        "GetCorpStandings" => ("ownerStandings", "fromID", None, corp_id),
        _ => return None,
    };
    fn lines(rows: &[Row], header: &[&str]) -> Vec<Value> {
        rows.iter().map(|r| Value::List(r.line(header))).collect()
    }
    let (mine, prime, npc) = ctx.store.read(|db| {
        let standings: Vec<Row> = db.table(mine).find(&[owner], &[store::Value::Int(id)]).collect();
        let to: HashSet<i64> = standings.iter().filter_map(|r| r.int("toID")).collect();
        let prime: Vec<Row> = db.table("eveStaticOwners").rows().filter(|o| o.int("ownerID").is_some_and(|o| to.contains(&o))).collect();
        let npc = npc.map(|t| {
            let rows: Vec<Row> = db.table(t).find(&["characterID"], &[store::Value::Int(id)]).collect();
            rowset(FROM_HEADER, lines(&rows, FROM_HEADER))
        });
        (rowset(TO_HEADER, lines(&standings, TO_HEADER)), rowset(OWNER_HEADER, lines(&prime, OWNER_HEADER)), npc)
    });
    let npc = npc.unwrap_or_else(|| {
        rowset(
            FROM_HEADER,
            crate::sde::table("npcStandings").find("toID", id).map(|r| Value::List(r.line(FROM_HEADER))).collect(),
        )
    });
    Some(Outcome::Value(Value::Tuple(vec![mine, prime, npc])))
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
            let mut factions: Vec<_> = crate::sde::table("chrFactions").rows().collect();
            factions.sort_by_key(|r| r.int("factionID"));
            Some(Outcome::Value(rowset(HEADER, factions.iter().map(|r| Value::List(r.line(HEADER))).collect())))
        }
        _ => None,
    }
}
