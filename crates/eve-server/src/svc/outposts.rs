//! Player outposts (menusvc.py "Assemble Platform", "Access Resources",
//! "Build"; base_stationmanagement.py).
//!
//! An outpost platform, assembled in a hangar (invbroker
//! AssembleConstructionPlatform), is launched for a corporation like a
//! starbase structure. In space, `Build` on it (from its owner corporation's
//! directors) starts construction, if the corporation's alliance holds the
//! system's sovereignty, the system has no station yet, and the platform is
//! near a planet. Construction takes `BUILD_HOURS` (scaled like starbases);
//! then the platform becomes a real station of its race's outpost type and
//! services, owned by the corporation, which anyone the station allows may
//! dock at.
//!
//! The corporation that owns a station runs it from the station management
//! window (corpStationMgr): its name and description, docking and office
//! costs, the reprocessing take, who may use each service, and the clones
//! installed there.

mod management;
pub use management::ensure as ensure_management_schema;

use eve_marshal::Value;
use tokio_postgres::GenericClient;
use tracing::{error, info};

use crate::services::Ctx;
use crate::svc::rows::{float, int, text};

pub const GROUP_CONSTRUCTION_PLATFORM: i64 = 307;
const GROUP_PLANET: i32 = 7;
const GROUP_STATION: i32 = 15;
const BUILD_HOURS: f64 = 24.0;
/// A platform must be this close to a planet to be built.
const PLANET_RANGE: f64 = 1.0e10;
const ROLE_DIRECTOR: i64 = crate::session::CORP_ROLE_DIRECTOR;

/// The outpost each platform becomes: (platform, station type, operation).
const OUTPOSTS: [(i64, i32, i32); 5] = [(10257, 21645, 50), (10258, 21646, 51), (10260, 21644, 49), (19758, 21642, 48), (22313, 21646, 51)];

pub type Refused<T> = Result<T, String>;

/// AssembleConstructionPlatform / AssembleCargoContainer(itemID, ...): the
/// packaged item in a hangar becomes one ready to use.
pub async fn assemble(ctx: &Ctx<'_>, item_id: i64) -> Refused<()> {
    let me = ctx.session.char_id.ok_or("No character.")?;
    let result = async {
        let mut client = crate::db::connect(ctx.db_url).await?;
        let mut txn = crate::items::ItemTxn::begin(&mut client).await?;
        let Some(item) = txn.item(item_id).await? else { return Ok(Err("That item no longer exists.")) };
        if item.owner_id != me && Some(item.owner_id) != ctx.session.corp_id {
            return Ok(Err("That is not yours."));
        }
        if item.quantity > 1 {
            txn.set_quantity(item_id, item.quantity - 1).await?;
            txn.create(crate::items::NewItem { type_id: item.type_id, owner_id: item.owner_id, location_id: item.location_id, flag: item.flag, quantity: 1, singleton: true }).await?;
        } else {
            txn.set_singleton(item_id, true).await?;
        }
        Ok::<_, tokio_postgres::Error>(Ok(txn.commit().await?))
    }
    .await;
    match result {
        Ok(Ok(events)) => {
            crate::items::notify(ctx.world, ctx.client_id, events);
            Ok(())
        }
        Ok(Err(why)) => Err(why.into()),
        Err(e) => {
            error!("assembling {item_id}: {}", crate::logging::chain(&e));
            Err("It could not be assembled.".into())
        }
    }
}

/// `Build` on a platform in space: construction starts.
pub async fn build(ctx: &Ctx<'_>, platform: i64) -> Refused<()> {
    let (Some(me), Some(system)) = (ctx.session.char_id, ctx.session.solar_system_id) else { return Err("You must be in space by the platform.".into()) };
    let row = ctx
        .query_one(
            r#"SELECT e."typeID", e."ownerID", e."x", e."y", e."z", t."groupID", s."allianceID", COALESCE(co."allianceID", 0),
                      (SELECT count(*) FROM "staStations" st WHERE st."solarSystemID" = e."locationID"),
                      (SELECT min(sqrt(power(d."x" - e."x", 2) + power(d."y" - e."y", 2) + power(d."z" - e."z", 2))) FROM "mapDenormalize" d
                        WHERE d."solarSystemID" = e."locationID" AND d."groupID" = $3),
                      EXISTS (SELECT 1 FROM "outpostConstruction" o WHERE o."platformID" = e."itemID")
                 FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                 JOIN "mapSolarSystems" s ON s."solarSystemID" = e."locationID"
                 LEFT JOIN "corporation" co ON co."corporationID" = e."ownerID"
                WHERE e."itemID" = $1 AND e."locationID" = $2"#,
            &[&(platform as i32), &(system as i32), &GROUP_PLANET],
        )
        .await
        .ok_or("That platform is not here.")?;
    let (type_id, owner, group) = (int(&row, 0).unwrap_or(0), int(&row, 1).unwrap_or(0), int(&row, 5).unwrap_or(0));
    let (sov, owner_alliance, stations) = (int(&row, 6), int(&row, 7).unwrap_or(0), int(&row, 8).unwrap_or(0));
    let planet_gap = float(&row, 9).unwrap_or(f64::MAX);
    if group != GROUP_CONSTRUCTION_PLATFORM {
        return Err("Only an outpost platform can be built into an outpost.".into());
    }
    if int(&row, 10) == Some(1) || row.get::<_, Option<bool>>(10) == Some(true) {
        return Err("That outpost is already being built.".into());
    }
    if Some(owner) != ctx.session.corp_id || !ctx.session.has_corp_role(ROLE_DIRECTOR) {
        return Err("Only a director of the platform's corporation can build it.".into());
    }
    if owner_alliance == 0 || sov != Some(owner_alliance) {
        return Err("Your alliance must hold sovereignty in this system to build an outpost here.".into());
    }
    if stations > 0 {
        return Err("This system already has a station.".into());
    }
    if planet_gap > PLANET_RANGE {
        return Err("An outpost must be built near a planet.".into());
    }
    if !OUTPOSTS.iter().any(|(p, _, _)| *p == type_id) {
        return Err("That platform has no outpost design.".into());
    }
    let now = crate::bulkdata::filetime_now();
    let hours = BUILD_HOURS / crate::pos::speedup().max(0.001);
    let done = now + (hours * 3_600.0 * 10_000_000.0) as i64;
    ctx.db
        .execute(
            r#"INSERT INTO "outpostConstruction" ("platformID","ownerID","solarSystemID","startedAt","doneAt","builderID") VALUES ($1,$2,$3,$4,$5,$6)"#,
            &[&(platform as i32), &(owner as i32), &(system as i32), &now, &done, &(me as i32)],
        )
        .await
        .map_err(|e| {
            error!("starting outpost {platform}: {}", crate::logging::chain(&e));
            "The construction could not start.".to_string()
        })?;
    info!("{me} started building outpost platform {platform} in {system}, done in {hours:.1} h");
    Ok(())
}

/// Outposts whose construction is done become stations. Run by the heartbeat.
pub async fn finish(world: &crate::world::World, db: &tokio_postgres::Client) {
    let now = crate::bulkdata::filetime_now();
    let due = match db
        .query(
            r#"SELECT o."platformID", o."ownerID", o."solarSystemID", e."typeID", e."x", e."y", e."z",
                      s."constellationID", s."regionID", s."security", s."solarSystemName", COALESCE(co."corporationName", '')
                 FROM "outpostConstruction" o JOIN entity e ON e."itemID" = o."platformID"
                 JOIN "mapSolarSystems" s ON s."solarSystemID" = o."solarSystemID"
                 LEFT JOIN "corporation" co ON co."corporationID" = o."ownerID"
                WHERE o."doneAt" <= $1"#,
            &[&now],
        )
        .await
    {
        Ok(rows) => rows,
        Err(e) => return error!("outposts due: {}", crate::logging::chain(&e)),
    };
    for r in due {
        let platform = int(&r, 0).unwrap_or(0) as i32;
        let (owner, system, type_id) = (int(&r, 1).unwrap_or(0) as i32, int(&r, 2).unwrap_or(0) as i32, int(&r, 3).unwrap_or(0));
        let (x, y, z) = (float(&r, 4).unwrap_or(0.0), float(&r, 5).unwrap_or(0.0), float(&r, 6).unwrap_or(0.0));
        let (constellation, region, security) = (int(&r, 7).unwrap_or(0) as i32, int(&r, 8).unwrap_or(0) as i32, float(&r, 9).unwrap_or(0.0));
        let name = format!("{} - {} Outpost", text(&r, 10).unwrap_or_default(), text(&r, 11).unwrap_or_default());
        let Some(&(_, station_type, operation)) = OUTPOSTS.iter().find(|(p, _, _)| *p == type_id) else { continue };
        let made = async {
            let station: i32 = db
                .query_one(r#"SELECT GREATEST(COALESCE(MAX("itemID"), 61000000), 61000000) + 1 FROM "mapDenormalize" WHERE "itemID" >= 61000000 AND "itemID" < 62000000"#, &[])
                .await?
                .get(0);
            db.execute(
                r#"INSERT INTO "staStations" ("stationID","security","dockingCostPerVolume","maxShipVolumeDockable","officeRentalCost","operationID",
                                              "stationTypeID","corporationID","solarSystemID","constellationID","regionID","stationName","x","y","z",
                                              "reprocessingEfficiency","reprocessingStationsTake","reprocessingHangarFlag")
                   VALUES ($1, $2, 0, 50000000, 10000, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, 0.5, 0.05, 4)"#,
                &[&station, &((security * 1000.0) as i32), &operation, &station_type, &owner, &system, &constellation, &region, &name, &x, &y, &z],
            )
            .await?;
            db.execute(
                r#"INSERT INTO "mapDenormalize" ("itemID","typeID","groupID","solarSystemID","constellationID","regionID","x","y","z","radius","itemName","security")
                   SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, COALESCE(t."radius", 15000), $10, $11 FROM "invTypes" t WHERE t."typeID" = $2"#,
                &[&station, &station_type, &GROUP_STATION, &system, &constellation, &region, &x, &y, &z, &name, &security],
            )
            .await?;
            db.execute(r#"INSERT INTO "eveNames" ("itemID","itemName","categoryID","groupID","typeID") VALUES ($1, $2, 3, $3, $4) ON CONFLICT DO NOTHING"#, &[&station, &name, &GROUP_STATION, &station_type])
                .await?;
            db.execute(r#"DELETE FROM "outpostConstruction" WHERE "platformID" = $1"#, &[&platform]).await?;
            db.execute(r#"DELETE FROM "posStructures" WHERE "itemID" = $1"#, &[&platform]).await?;
            db.execute(r#"DELETE FROM entity WHERE "itemID" = $1 OR "locationID" = $1"#, &[&platform]).await?;
            Ok::<_, tokio_postgres::Error>(station)
        }
        .await;
        match made {
            Ok(station) => {
                info!("outpost {name} ({station}) built for corporation {owner}");
                show(world, db, system as i64, platform as i64, station as i64).await;
            }
            Err(e) => error!("building outpost from platform {platform}: {}", crate::logging::chain(&e)),
        }
    }
}

/// The platform goes from the system's park and the station appears.
async fn show(world: &crate::world::World, db: &tokio_postgres::Client, system: i64, platform: i64, station: i64) {
    let row = db
        .query_opt(r#"SELECT "typeID", "x", "y", "z", "radius", "itemName" FROM "mapDenormalize" WHERE "itemID" = $1"#, &[&(station as i32)])
        .await
        .ok()
        .flatten();
    let Some(r) = row else { return };
    let celestial = crate::ballpark::Celestial {
        id: station,
        type_id: int(&r, 0).unwrap_or(0),
        group_id: GROUP_STATION as i64,
        name: text(&r, 5).unwrap_or_default(),
        pos: crate::ballpark::Vec3::new(float(&r, 1).unwrap_or(0.0), float(&r, 2).unwrap_or(0.0), float(&r, 3).unwrap_or(0.0)),
        radius: float(&r, 4).unwrap_or(15_000.0),
        jump: None,
    };
    let mut w = world.lock();
    let Some(park) = w.parks.get_mut(&system) else { return };
    let mut entries = Vec::new();
    if park.structures.remove(&platform).is_some() {
        entries.push(crate::svc::space::entry(park.tick, "RemoveBalls", vec![Value::List(vec![Value::Int(platform)])]));
    }
    let ball = crate::destiny::Ball::celestial(celestial.id, celestial.radius, celestial.pos.tuple(), &celestial.name, true);
    let blob = crate::destiny::encode(crate::destiny::stamp(park.tick), &[ball]);
    let slim = crate::svc::space::slim(vec![("itemID", Value::Int(celestial.id)), ("typeID", Value::Int(celestial.type_id)), ("name", Value::text(&celestial.name))]);
    entries.push(crate::svc::space::entry(park.tick, "AddBalls", vec![Value::Tuple(vec![Value::Bytes(blob), Value::List(vec![slim]), Value::Dict(Vec::new())])]));
    park.celestials.push(celestial);
    crate::svc::space::broadcast(&w, system, None, entries);
}

// ---- station management ---------------------------------------------------------------

fn key_val(fields: Vec<(&str, Value)>) -> Value {
    Value::Instance {
        class: Box::new(Value::bytes("util.KeyVal")),
        state: Box::new(Value::Dict(fields.into_iter().map(|(k, v)| (Value::bytes(k), v)).collect())),
    }
}

/// The corpStationMgr calls of the station management window; None for others.
pub async fn station_mgr(ctx: &Ctx<'_>, method: &str, args: &[Value], station: i64) -> Option<Result<Value, String>> {
    let arg = |i: usize| args.get(i).and_then(|v| v.as_i64());
    Some(match method {
        "GetStationServiceIdentifiers" => {
            Ok(ctx.rowset(&["serviceID", "serviceName", "serviceNameID"], r#"SELECT "serviceID", "serviceName", 0 FROM "staServices" ORDER BY 1"#, &[]).await)
        }
        "GetStationServiceAccessRule" | "GetStationManagementServiceCostModifiers" => management::read(ctx,station,method,args).await,
        "GetStationDetails" => management::details(ctx,station,args).await,
        "GetRentableItems" => management::rentables(ctx,station,args).await,
        "GetOwnerIDsOfClonesAtStation" => management::read(ctx,station,method,args).await,
        "RevokeCloneContractsAtStation" => {
            let operation=async {
                if args.len()!=2 || !arg(0).is_some_and(|n|(1..=i32::MAX as i64).contains(&n))
                    || !arg(1).is_some_and(|n|n==-1 || (1..=i32::MAX as i64).contains(&n)) {
                    return Err("Choose a valid corporation and clone owner, or -1 for every owner in that corporation.".into());
                }
                let mut db=crate::db::connect(ctx.db_url).await.map_err(management::failed)?;
                let tx=db.transaction().await.map_err(management::failed)?;
                management::authorize(ctx,&tx,station).await?;
                let (corp,char_id)=(arg(0).unwrap_or(-1),arg(1).unwrap_or(-1));
                let gone=tx.execute(r#"DELETE FROM "chrJumpClones" j USING "character_" c WHERE c."characterID"=j."ownerID"
                    AND j."locationID"=$1 AND c."corporationID"=$2 AND ($3=-1 OR j."ownerID"=$3)"#,
                    &[&(station as i32),&(corp as i32),&(char_id as i32)]).await.map_err(management::failed)?;
                tx.commit().await.map_err(management::failed)?;
                info!(station,gone,"station clone contracts revoked");Ok(Value::None)
            };
            operation.await
        }
        "UpdateStationManagementSettings" => update(ctx,station,args).await.map(|_|Value::None),
        _ => return None,
    })
}

/// Source-shaped settings are validated and committed atomically.
async fn update(ctx:&Ctx<'_>,station:i64,args:&[Value])->Refused<()> {
    management::update(ctx,station,args).await
}

/// Whether a station's owners let a pilot use a service (docking included),
/// by the rules they set: standing towards them and security status.
pub async fn may_use(db: &impl GenericClient, station: i64, service: i64, char_id: i64) -> bool {
    match may_use_checked(db,station,service,char_id).await {
        Ok(allowed)=>allowed,
        Err(error)=>{error!("checking station {station} service access: {}",crate::logging::chain(&error));false}
    }
}

pub async fn may_use_checked(db:&impl GenericClient,station:i64,service:i64,char_id:i64)->Result<bool,tokio_postgres::Error> {
    let sql = r#"SELECT r."minimumStanding", r."minimumCharSecurity", r."maximumCharSecurity", c."securityRating",
                        COALESCE((SELECT o."standing" FROM "ownerStandings" o WHERE o."fromID" = s."corporationID" AND o."toID" IN (c."characterID", c."corporationID") ORDER BY o."standing" LIMIT 1), 0)::float8,
                        s."corporationID" = c."corporationID"
                   FROM "staStations" s JOIN "character_" c ON c."characterID" = $3
                   LEFT JOIN "stationServiceRules" r ON r."stationID" = s."stationID" AND r."serviceID" = $2
                  WHERE s."stationID" = $1 AND s."corporationID" IS NOT NULL"#;
    let Some(r) = db.query_opt(sql, &[&(station as i32), &(service as i32), &(char_id as i32)]).await? else { return Ok(false) };
    if r.get::<_, Option<bool>>(5) == Some(true) {
        return Ok(true);
    }
    let (min_standing, min_sec, max_sec) = (float(&r, 0).unwrap_or(-10.0), float(&r, 1).unwrap_or(-10.0), float(&r, 2).unwrap_or(10.0));
    let (sec, standing) = (float(&r, 3).unwrap_or(0.0), float(&r, 4).unwrap_or(0.0));
    Ok([standing,sec,min_standing,min_sec,max_sec].into_iter().all(f64::is_finite)
        && standing >= min_standing && sec >= min_sec && sec <= max_sec)
}

/// Guard absent standing rows as well as existing ones. The trigger covers all
/// writers, including bot updates and administrative SQL, without depending on
/// callers remembering a service-specific mutex. Bigint advisory locks have a
/// separate namespace from the two-integer character lifecycle locks.
pub async fn ensure_access_locks(db:&tokio_postgres::Client)->Result<(),tokio_postgres::Error>{
    db.batch_execute(r#"
        CREATE OR REPLACE FUNCTION station_standing_access_lock() RETURNS trigger LANGUAGE plpgsql AS $$
        DECLARE keys bigint[]; access_key bigint;
        BEGIN
            IF TG_OP='INSERT' THEN
                keys=ARRAY[hashtextextended('trinity:station-access-standing:'||NEW."fromID"::text||':'||NEW."toID"::text,0)];
            ELSIF TG_OP='DELETE' THEN
                keys=ARRAY[hashtextextended('trinity:station-access-standing:'||OLD."fromID"::text||':'||OLD."toID"::text,0)];
            ELSE
                keys=ARRAY[hashtextextended('trinity:station-access-standing:'||OLD."fromID"::text||':'||OLD."toID"::text,0),
                           hashtextextended('trinity:station-access-standing:'||NEW."fromID"::text||':'||NEW."toID"::text,0)];
            END IF;
            FOR access_key IN SELECT DISTINCT value FROM unnest(keys) AS value ORDER BY value LOOP
                PERFORM pg_advisory_xact_lock(access_key);
            END LOOP;
            IF TG_OP='DELETE' THEN RETURN OLD; END IF;
            RETURN NEW;
        END $$;
        DO $$ BEGIN
            IF NOT EXISTS(SELECT 1 FROM pg_trigger WHERE tgname='station_standing_access_lock') THEN
                CREATE TRIGGER station_standing_access_lock BEFORE INSERT OR UPDATE OR DELETE ON "ownerStandings"
                    FOR EACH ROW EXECUTE FUNCTION station_standing_access_lock();
            END IF;
        END $$;
    "#).await
}

/// Arrival already owns the character row. Keep the station and its rules
/// locked through COMMIT; station management takes the same station lock before
/// inserting even a previously absent service rule.
pub async fn may_use_locked(tx:&tokio_postgres::Transaction<'_>,station:i64,service:i64,char_id:i64)->Result<bool,tokio_postgres::Error> {
    let Some(character)=tx.query_opt(r#"SELECT "corporationID","securityRating" FROM "character_" WHERE "characterID"=$1 FOR SHARE"#,
        &[&(char_id as i32)]).await? else{return Ok(false);};
    let Some(owner)=tx.query_opt(r#"SELECT "corporationID" FROM "staStations" WHERE "stationID"=$1 FOR SHARE"#,
        &[&(station as i32)]).await? else{return Ok(false);};
    let corporation=character.get::<_,i32>(0);
    let Some(owner)=owner.get::<_,Option<i32>>(0) else {return Ok(false);};
    if corporation==owner {return Ok(true);}
    let rule=tx.query_opt(r#"SELECT "minimumStanding","minimumCharSecurity","maximumCharSecurity" FROM "stationServiceRules"
        WHERE "stationID"=$1 AND "serviceID"=$2 FOR SHARE"#,&[&(station as i32),&(service as i32)]).await?;
    let keys=tx.query(r#"SELECT DISTINCT hashtextextended('trinity:station-access-standing:'||$1::int::text||':'||target::text,0) AS key
        FROM unnest($2::int[]) AS target ORDER BY key"#,&[&owner,&vec![char_id as i32,corporation]]).await?;
    for key in keys {tx.query_one("SELECT pg_advisory_xact_lock_shared($1)",&[&key.get::<_,i64>(0)]).await?;}
    // Do not add row locks here: UPDATE obtains its row lock before its BEFORE
    // trigger takes the advisory lock. Taking the reverse order would deadlock.
    // A fresh READ COMMITTED query after the advisory waits sees the committed
    // standing, and both existing and absent pairs stay stable through COMMIT.
    let standings=tx.query(r#"SELECT standing FROM "ownerStandings" WHERE "fromID"=$1 AND "toID"=ANY($2) ORDER BY "toID""#,
        &[&owner,&vec![char_id as i32,corporation]]).await?;
    let standing=standings.iter().filter_map(|r|float(r,0)).min_by(f64::total_cmp).unwrap_or(0.0);
    let security=float(&character,1).unwrap_or(0.0);
    let (minimum,low,high)=rule.map_or((-10.0,-10.0,10.0),|r|(r.get::<_,f64>(0),r.get::<_,f64>(1),r.get::<_,f64>(2)));
    Ok([standing,security,minimum,low,high].into_iter().all(f64::is_finite) && standing>=minimum && security>=low && security<=high)
}

#[cfg(test)] mod access_tests;
