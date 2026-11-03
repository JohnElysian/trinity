//! The web dashboard: a page, built in, and the JSON it reads, showing
//! who is playing, the wars under way, the alliances and the space they
//! hold, the busiest systems, the news from null-sec, the latest kills and
//! the killboard, the bots' market, their feelings about players, the
//! pilots being followed and politics' upheavals. `RMR_WEB` is where it
//! listens (empty: off).
//!
//! It answers GET, and closes each connection after its answer: a dashboard
//! polling every few seconds needs no more. The one thing it changes is the
//! bots (POST /api/control, with the request in `do`), and only when
//! `bots.dashboardControls` allows it.
//!
//! The page is TypeScript (web/src/app.ts), compiled to web/dist/app.js with
//! `npm run build` in crates/eve-server/web; the compiled file is part of the
//! source, so building the server needs no Node.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value as Json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_postgres::types::Type;
use tokio_postgres::Row;
use tracing::{debug, error, info, warn};

use crate::conn::Server;

const INDEX: &str = include_str!("../web/index.html");
const STYLE: &str = include_str!("../web/style.css");
const SCRIPT: &str = include_str!("../web/dist/app.js");

/// The most a request may be before it is turned away.
const MAX_REQUEST: usize = 8 * 1024;
/// FILETIME of 1970-01-01, in milliseconds since 1601.
const UNIX_EPOCH_MS: i64 = 11_644_473_600_000;

static STARTED: OnceLock<Instant> = OnceLock::new();

pub async fn run(server: Arc<Server>, bind: String) {
    if bind.is_empty() {
        return;
    }
    STARTED.get_or_init(Instant::now);
    let listener = match TcpListener::bind(&bind).await {
        Ok(l) => l,
        Err(e) => {
            error!("web dashboard: cannot listen on {bind}: {e}");
            return;
        }
    };
    info!("web dashboard on http://{bind}/");
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let server = Arc::clone(&server);
        tokio::spawn(async move {
            match tokio::time::timeout(Duration::from_secs(15), answer(stream, &server)).await {
                Ok(Err(e)) => debug!("web: {e}"),
                Err(_) => debug!("web: request timed out"),
                Ok(Ok(())) => {},
            }
        });
    }
}

async fn answer(mut stream: TcpStream, server: &Server) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.len() > MAX_REQUEST {
            return respond(&mut stream, 431, "text/plain", b"Request too large").await;
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let mut parts = head.lines().next().unwrap_or("").split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));
    let path = target.split('?').next().unwrap_or("/");
    if method == "POST" && path == "/api/control" {
        let query = target.split_once('?').map_or("", |(_, q)| q);
        let asked = query.split('&').find_map(|kv| kv.strip_prefix("do=")).map(decode).unwrap_or_default();
        let (status, body) = if !crate::bots::controls_enabled() {
            (403, json!({ "error": "Dashboard controls are off (bots.dashboardControls in trinity.yml)." }))
        } else {
            let words: Vec<&str> = asked.split_whitespace().collect();
            match crate::bots::request(&words) {
                Ok(said) => (200, json!({ "ok": said })),
                Err(why) => (400, json!({ "error": why })),
            }
        };
        return respond(&mut stream, status, "application/json", body.to_string().as_bytes()).await;
    }
    if method != "GET" {
        return respond(&mut stream, 405, "text/plain", b"Only GET").await;
    }
    match path {
        "/" | "/index.html" => respond(&mut stream, 200, "text/html; charset=utf-8", INDEX.as_bytes()).await,
        "/style.css" => respond(&mut stream, 200, "text/css; charset=utf-8", STYLE.as_bytes()).await,
        "/app.js" => respond(&mut stream, 200, "text/javascript; charset=utf-8", SCRIPT.as_bytes()).await,
        api if api.starts_with("/api/") => {
            let body = match api {
                "/api/summary" => Some(summary(server).await),
                "/api/news" => Some(news(server)),
                "/api/sieges" => Some(sieges(server)),
                "/api/wars" => Some(wars(server).await),
                "/api/players" => Some(players(server).await),
                "/api/alliances" => Some(alliances(server).await),
                "/api/corporations" => Some(corporations(server).await),
                "/api/systems" => Some(systems(server).await),
                "/api/map" => Some(map(server).await),
                "/api/sov" => Some(sov(server).await),
                "/api/kills" => Some(kills(server).await),
                "/api/killboard" => Some(killboard(server).await),
                "/api/market" => Some(Ok(server.world.lock().bot_panels.market.clone())),
                "/api/relations" => Some(Ok(server.world.lock().bot_panels.relations.clone())),
                "/api/followed" => Some(Ok(server.world.lock().bot_panels.followed.clone())),
                "/api/events" => Some(Ok(server.world.lock().bot_panels.events.clone())),
                "/api/pilots" => {
                    let query = target.split_once('?').map_or("", |(_, q)| q);
                    let q = query.split('&').find_map(|kv| kv.strip_prefix("q=")).map(decode).unwrap_or_default();
                    Some(Ok(pilots(server, &q)))
                }
                _ => None,
            };
            match body {
                Some(Ok(v)) => respond(&mut stream, 200, "application/json", v.to_string().as_bytes()).await,
                Some(Err(e)) => {
                    warn!("web {api}: {}", crate::logging::chain(&e));
                    respond(&mut stream, 500, "application/json", br#"{"error":"database"}"#).await
                }
                None => respond(&mut stream, 404, "application/json", br#"{"error":"no such thing"}"#).await,
            }
        }
        _ => respond(&mut stream, 404, "text/plain", b"Not found").await,
    }
}

async fn respond(stream: &mut TcpStream, status: u16, kind: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        431 => "Request Header Fields Too Large",
        _ => "Internal Server Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await?;
    stream.write_all(body).await?;
    stream.shutdown().await
}

/// Rows as JSON objects, keyed by their column names.
fn rows(rows: &[Row]) -> Json {
    Json::Array(
        rows.iter()
            .map(|r| {
                let mut o = Map::new();
                for (i, c) in r.columns().iter().enumerate() {
                    let v = match *c.type_() {
                        Type::INT2 => r.try_get::<_, Option<i16>>(i).ok().flatten().map(Json::from),
                        Type::INT4 => r.try_get::<_, Option<i32>>(i).ok().flatten().map(Json::from),
                        Type::INT8 => r.try_get::<_, Option<i64>>(i).ok().flatten().map(Json::from),
                        Type::FLOAT4 => r.try_get::<_, Option<f32>>(i).ok().flatten().map(|f| json!(f)),
                        Type::FLOAT8 => r.try_get::<_, Option<f64>>(i).ok().flatten().map(|f| json!(f)),
                        Type::BOOL => r.try_get::<_, Option<bool>>(i).ok().flatten().map(Json::from),
                        _ => r.try_get::<_, Option<String>>(i).ok().flatten().map(Json::from),
                    };
                    o.insert(c.name().to_string(), v.unwrap_or(Json::Null));
                }
                Json::Object(o)
            })
            .collect(),
    )
}

/// A FILETIME column as milliseconds since 1970, for the page's Date.
fn ms(column: &str) -> String {
    format!("({column} / 10000 - {UNIX_EPOCH_MS})::bigint")
}

type Answer = Result<Json, crate::db::AcquireError>;

async fn summary(server: &Server) -> Answer {
    let db=server.db.lease().await?;
    let held: i64 = db.query_one(r#"SELECT count(*) FROM "mapSolarSystems" WHERE "allianceID" IS NOT NULL"#, &[]).await?.get(0);
    let w = server.world.lock();
    let b = &w.bot_stats;
    Ok(json!({
        "players": w.players().len(),
        "bots": { "total": b.total, "online": b.online, "inSpace": b.in_space, "running": b.running },
        "corporations": b.corporations,
        "alliances": b.alliances,
        "wars": b.wars,
        "lostInFights": b.lost,
        "systemsHeld": held,
        "uptimeSeconds": STARTED.get().map_or(0, |s| s.elapsed().as_secs()),
    }))
}

/// Sieges announced or under way, soonest first: where to go to see a battle.
fn sieges(server: &Server) -> Answer {
    let mut list = server.world.lock().bot_stats.sieges.clone();
    list.sort_by_key(|s| s.starts);
    Ok(Json::Array(
        list.iter()
            .map(|s| {
                json!({
                    "attacker": s.attacker, "defender": s.defender, "system": s.system, "region": s.region,
                    "security": (s.security * 10.0).round() / 10.0, "starts": s.starts / 10_000 - UNIX_EPOCH_MS,
                    "fighting": s.fleets.is_some(),
                    "attackers": s.fleets.map(|f| f.0), "defenders": s.fleets.map(|f| f.1),
                })
            })
            .collect(),
    ))
}

/// What the bot alliances did in null-sec lately, newest first.
fn news(server: &Server) -> Answer {
    let news = server.world.lock().bot_stats.news.clone();
    Ok(Json::Array(news.iter().rev().map(|(at, what)| json!({ "time": at / 10_000 - UNIX_EPOCH_MS, "text": what })).collect()))
}

async fn wars(server: &Server) -> Answer {
    let db=server.db.lease().await?;
    let now = crate::bulkdata::filetime_now();
    let sql = format!(
        r#"SELECT w."warID" AS id,
                  COALESCE(da."allianceName", dc."corporationName", '?') AS "declaredBy",
                  COALESCE(da."shortName", dc."tickerName", '') AS "declaredByTicker",
                  COALESCE(aa."allianceName", ac."corporationName", '?') AS against,
                  COALESCE(aa."shortName", ac."tickerName", '') AS "againstTicker",
                  {declared} AS declared, {finished} AS ends,
                  w."mutual" = 1 AS mutual, EXISTS (SELECT 1 FROM "botGroups" g WHERE g."ownerID" = w."declaredByID") AS bots
             FROM "crpWars" w
             LEFT JOIN alliances da ON da."allianceID" = w."declaredByID"
             LEFT JOIN "corporation" dc ON dc."corporationID" = w."declaredByID"
             LEFT JOIN alliances aa ON aa."allianceID" = w."againstID"
             LEFT JOIN "corporation" ac ON ac."corporationID" = w."againstID"
            WHERE w."retracted" IS NULL OR w."retracted" > $1
            ORDER BY w."timeDeclared" DESC LIMIT 200"#,
        declared = ms(r#"w."timeDeclared""#),
        finished = ms(r#"w."timeFinished""#),
    );
    let found = db.query(sql.as_str(), &[&now]).await?;
    let losses = server.world.lock().bot_stats.war_losses.clone();
    let mut list = rows(&found);
    if let Json::Array(items) = &mut list {
        for (item, r) in items.iter_mut().zip(&found) {
            let id: i32 = r.get(0);
            item["losses"] = json!(losses.get(&(id as i64)).copied().unwrap_or(0));
        }
    }
    Ok(list)
}

/// The people playing, not the bots: where each is and in what.
async fn players(server: &Server) -> Answer {
    let db=server.db.lease().await?;
    let present = server.world.lock().players();
    let ids: Vec<i32> = present.iter().map(|p| p.char_id as i32).collect();
    let sql = r#"SELECT c."characterID" AS id, c."characterName" AS name, co."corporationName" AS corporation,
                        co."tickerName" AS ticker, a."allianceName" AS alliance, round(c."securityRating"::numeric, 1)::float8 AS security,
                        t."typeName" AS ship
                   FROM "character_" c
                   LEFT JOIN "corporation" co ON co."corporationID" = c."corporationID"
                   LEFT JOIN alliances a ON a."allianceID" = co."allianceID"
                   LEFT JOIN entity e ON e."itemID" = c."shipID"
                   LEFT JOIN "invTypes" t ON t."typeID" = e."typeID"
                  WHERE c."characterID" = ANY($1) ORDER BY c."characterName""#;
    let found = db.query(sql, &[&ids]).await?;
    let systems: Vec<i32> = present.iter().map(|p| p.solar_system_id as i32).collect();
    let names: HashMap<i32, (String, f64)> = db
        .query(r#"SELECT "solarSystemID", "solarSystemName", "security" FROM "mapSolarSystems" WHERE "solarSystemID" = ANY($1)"#, &[&systems])
        .await?
        .iter()
        .map(|r| (r.get::<_, i32>(0), (r.get::<_, Option<String>>(1).unwrap_or_default(), r.get::<_, Option<f64>>(2).unwrap_or(0.0))))
        .collect();
    let mut list = rows(&found);
    if let Json::Array(items) = &mut list {
        for item in items.iter_mut() {
            let id = item["id"].as_i64().unwrap_or(0);
            if let Some(p) = present.iter().find(|p| p.char_id == id) {
                let (system, security) = names.get(&(p.solar_system_id as i32)).cloned().unwrap_or_default();
                item["system"] = json!(system);
                item["systemSecurity"] = json!((security * 10.0).round() / 10.0);
                item["docked"] = json!(p.docked);
            }
        }
    }
    Ok(list)
}

async fn alliances(server: &Server) -> Answer {
    let db=server.db.lease().await?;
    let sql = r#"SELECT a."allianceID" AS id, a."allianceName" AS name, a."shortName" AS ticker,
                        count(co."corporationID")::int AS corporations, COALESCE(sum(co."memberCount"), 0)::int AS pilots,
                        CASE g."kind" WHEN 11 THEN 'null-sec' WHEN 12 THEN 'pirate' WHEN 13 THEN 'industrial' ELSE 'players' END AS kind,
                        r."regionName" AS region,
                        (SELECT count(*) FROM "mapSolarSystems" s WHERE s."allianceID" = a."allianceID")::int AS systems
                   FROM alliances a
                   LEFT JOIN "corporation" co ON co."allianceID" = a."allianceID" AND co."deleted" = 0
                   LEFT JOIN "botGroups" g ON g."ownerID" = a."allianceID"
                   LEFT JOIN "mapRegions" r ON r."regionID" = g."regionID"
                  WHERE a."deleted" = 0
                  GROUP BY a."allianceID", g."kind", r."regionName"
                  ORDER BY pilots DESC LIMIT 60"#;
    Ok(rows(&db.query(sql, &[]).await?))
}

async fn corporations(server: &Server) -> Answer {
    let db=server.db.lease().await?;
    let sql = r#"SELECT co."corporationID" AS id, co."corporationName" AS name, co."tickerName" AS ticker, co."memberCount" AS members,
                        a."shortName" AS alliance, EXISTS (SELECT 1 FROM "botGroups" g WHERE g."ownerID" = co."corporationID") AS bots
                   FROM "corporation" co
                   LEFT JOIN alliances a ON a."allianceID" = co."allianceID"
                  WHERE co."deleted" = 0
                    AND NOT EXISTS (SELECT 1 FROM "crpNPCCorporations" n WHERE n."corporationID" = co."corporationID")
                  ORDER BY co."memberCount" DESC LIMIT 40"#;
    Ok(rows(&db.query(sql, &[]).await?))
}

/// The busiest systems by pilots online: in space and docked, bots included.
async fn systems(server: &Server) -> Answer {
    let db=server.db.lease().await?;
    let (space, docked) = server.world.lock().counts();
    let mut busiest: Vec<(i64, i64, i64)> = space.iter().map(|(s, n)| (*s, *n, docked.get(s).copied().unwrap_or(0))).collect();
    busiest.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)).then(a.0.cmp(&b.0)));
    busiest.truncate(30);
    let ids: Vec<i32> = busiest.iter().map(|(s, _, _)| *s as i32).collect();
    let sql = r#"SELECT s."solarSystemID", s."solarSystemName", round(s."security"::numeric, 1)::float8, r."regionName"
                   FROM "mapSolarSystems" s JOIN "mapRegions" r ON r."regionID" = s."regionID"
                  WHERE s."solarSystemID" = ANY($1)"#;
    let names: HashMap<i32, (String, f64, String)> = db
        .query(sql, &[&ids])
        .await?
        .iter()
        .map(|r| (r.get(0), (r.get::<_, Option<String>>(1).unwrap_or_default(), r.get::<_, Option<f64>>(2).unwrap_or(0.0), r.get::<_, Option<String>>(3).unwrap_or_default())))
        .collect();
    Ok(Json::Array(
        busiest
            .iter()
            .map(|(id, in_space, in_station)| {
                let (name, security, region) = names.get(&(*id as i32)).cloned().unwrap_or_default();
                json!({ "id": id, "name": name, "security": security, "region": region, "inSpace": in_space, "docked": in_station })
            })
            .collect(),
    ))
}

/// Metres in a light year: the map's coordinates are sent in these.
const LIGHT_YEAR: f64 = 9.4607e15;

static MAP: OnceLock<Json> = OnceLock::new();

/// The star map for the sovereignty map, which never changes: regions as
/// [id, name], systems as [id, name, region, constellation, x, z, security]
/// (x and z in light years, seen from above), and each gate link once as
/// [from, to].
async fn map(server: &Server) -> Answer {
    if let Some(m) = MAP.get() {
        return Ok(m.clone());
    }
    let db=server.db.lease().await?;
    let regions: Vec<Json> = db
        .query(r#"SELECT "regionID", "regionName" FROM "mapRegions" ORDER BY "regionName""#, &[])
        .await?
        .iter()
        .map(|r| json!([r.get::<_, i32>(0), r.get::<_, Option<String>>(1).unwrap_or_default()]))
        .collect();
    let ly = |v: Option<f64>| (v.unwrap_or(0.0) / LIGHT_YEAR * 100.0).round() / 100.0;
    let systems: Vec<Json> = db
        .query(
            r#"SELECT "solarSystemID", "solarSystemName", "regionID", "constellationID", x, z, security
                 FROM "mapSolarSystems""#,
            &[],
        )
        .await?
        .iter()
        .map(|r| {
            json!([
                r.get::<_, i32>(0),
                r.get::<_, Option<String>>(1).unwrap_or_default(),
                r.get::<_, Option<i32>>(2).unwrap_or(0),
                r.get::<_, Option<i32>>(3).unwrap_or(0),
                ly(r.get(4)),
                ly(r.get(5)),
                (r.get::<_, Option<f64>>(6).unwrap_or(0.0) * 100.0).round() / 100.0,
            ])
        })
        .collect();
    let jumps: Vec<Json> = db
        .query(
            r#"SELECT "fromSolarSystemID", "toSolarSystemID" FROM "mapSolarSystemJumps"
                WHERE "fromSolarSystemID" < "toSolarSystemID""#,
            &[],
        )
        .await?
        .iter()
        .map(|r| json!([r.get::<_, i32>(0), r.get::<_, i32>(1)]))
        .collect();
    let m = json!({ "regions": regions, "systems": systems, "jumps": jumps });
    Ok(MAP.get_or_init(|| m).clone())
}

/// Who holds what: [system, alliance] for every claimed system, and the
/// alliances holding any as [id, name, ticker].
async fn sov(server: &Server) -> Answer {
    let db=server.db.lease().await?;
    let held: Vec<Json> = db
        .query(r#"SELECT "solarSystemID", "allianceID" FROM "mapSolarSystems" WHERE "allianceID" IS NOT NULL"#, &[])
        .await?
        .iter()
        .map(|r| json!([r.get::<_, i32>(0), r.get::<_, i32>(1)]))
        .collect();
    let alliances: Vec<Json> = db
        .query(
            r#"SELECT a."allianceID", a."allianceName", a."shortName" FROM alliances a
                WHERE EXISTS (SELECT 1 FROM "mapSolarSystems" s WHERE s."allianceID" = a."allianceID")"#,
            &[],
        )
        .await?
        .iter()
        .map(|r| {
            json!([
                r.get::<_, i32>(0),
                r.get::<_, Option<String>>(1).unwrap_or_default(),
                r.get::<_, Option<String>>(2).unwrap_or_default(),
            ])
        })
        .collect();
    Ok(json!({ "systems": held, "alliances": alliances }))
}

/// A query string's value: `+` for spaces, `%XX` for bytes.
fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 2;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Bots whose names start with (or contain) `q`, for following one.
fn pilots(server: &Server, q: &str) -> Json {
    let q = q.trim().to_lowercase();
    if q.len() < 2 {
        return Json::Array(Vec::new());
    }
    let roster = std::sync::Arc::clone(&server.world.lock().bot_roster);
    let mut found: Vec<&crate::world::LocalBot> = roster.iter().filter(|b| b.name.to_lowercase().starts_with(&q)).take(15).collect();
    if found.len() < 15 {
        found.extend(roster.iter().filter(|b| !b.name.to_lowercase().starts_with(&q) && b.name.to_lowercase().contains(&q)).take(15 - found.len()));
    }
    Json::Array(found.iter().map(|b| json!({ "id": b.char_id, "name": b.name })).collect())
}

/// The last day's kills and losses by alliance (or corporation outside
/// one), bots' unseen fights included.
async fn killboard(server: &Server) -> Answer {
    let db=server.db.lease().await?;
    let since = crate::bulkdata::filetime_now() - 24 * 3_600 * 10_000_000;
    let sql = r#"WITH recent AS (SELECT * FROM "killLog" WHERE "killTime" > $1),
                      k AS (SELECT COALESCE(NULLIF(p."allianceID", 0), p."corporationID") AS side, count(*)::int AS n
                              FROM "killLogParticipants" p JOIN recent r ON r."killID" = p."killID" WHERE p."finalBlow" = 1 GROUP BY 1),
                      l AS (SELECT COALESCE(NULLIF(r."victimAllianceID", 0), r."victimCorpID") AS side, count(*)::int AS n FROM recent r GROUP BY 1),
                      sides AS (SELECT side FROM k UNION SELECT side FROM l)
                 SELECT s.side AS id, COALESCE(a."allianceName", c."corporationName", '?') AS name,
                        COALESCE(a."shortName", c."tickerName", '') AS ticker,
                        COALESCE(k.n, 0) AS kills, COALESCE(l.n, 0) AS losses,
                        EXISTS (SELECT 1 FROM "botGroups" g WHERE g."ownerID" = s.side) AS bots
                   FROM sides s LEFT JOIN k ON k.side = s.side LEFT JOIN l ON l.side = s.side
                   LEFT JOIN alliances a ON a."allianceID" = s.side LEFT JOIN "corporation" c ON c."corporationID" = s.side
                  ORDER BY kills DESC, losses DESC LIMIT 30"#;
    let sides = rows(&db.query(sql, &[&since]).await?);
    let total: i64 = db.query_one(r#"SELECT count(*) FROM "killLog" WHERE "killTime" > $1"#, &[&since]).await?.get(0);
    let ships = rows(
        &db
        .query(
                r#"SELECT t."typeName" AS ship, count(*)::int AS lost FROM "killLog" k JOIN "invTypes" t ON t."typeID" = k."victimShipTypeID"
                    WHERE k."killTime" > $1 GROUP BY 1 ORDER BY 2 DESC LIMIT 10"#,
                &[&since],
            )
            .await?,
    );
    Ok(json!({ "total": total, "sides": sides, "ships": ships }))
}

async fn kills(server: &Server) -> Answer {
    let db=server.db.lease().await?;
    let sql = format!(
        r#"SELECT k."killID" AS id, {time} AS time, vc."characterName" AS victim, vco."corporationName" AS "victimCorporation",
                  t."typeName" AS ship, s."solarSystemName" AS system, fc."characterName" AS "finalBlow", k."isPodKill" = 1 AS pod
             FROM "killLog" k
             LEFT JOIN "character_" vc ON vc."characterID" = k."victimCharID"
             LEFT JOIN "corporation" vco ON vco."corporationID" = k."victimCorpID"
             LEFT JOIN "invTypes" t ON t."typeID" = k."victimShipTypeID"
             LEFT JOIN "mapSolarSystems" s ON s."solarSystemID" = k."solarSystemID"
             LEFT JOIN "character_" fc ON fc."characterID" = k."finalBlowID"
            ORDER BY k."killID" DESC LIMIT 30"#,
        time = ms(r#"k."killTime""#)
    );
    Ok(rows(&db.query(sql.as_str(), &[]).await?))
}

#[cfg(test)] mod recovery_tests {
    use super::*;
    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE disposable database; terminates only its own backend"]
    async fn dashboard_database_requests_recover_on_the_next_request() {
        let f=crate::test_support::Database::load().await;
        let admin=crate::db::connect(&f.url).await.unwrap();
        let server=Server {table:Default::default(),db:crate::db::Shared::new(f.db,f.url.clone()),db_url:f.url,
            node_id:1,bulk:f.bulk,dogma:f.dogma,world:f.world,services:crate::svc::registry(),
            dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0};
        let before=summary(&server).await.unwrap();
        let original=server.db.snapshot();
        let pid:i32=original.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
        admin.query_one("SELECT pg_terminate_backend($1)",&[&pid]).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3),async {while !original.is_closed(){tokio::task::yield_now().await;}}).await.unwrap();
        let after=summary(&server).await.unwrap();
        assert_eq!(before["systemsHeld"],after["systemsHeld"]);
        assert!(!std::sync::Arc::ptr_eq(&original,&server.db.snapshot()));
    }
}
