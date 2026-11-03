use eve_marshal::Value;
use tracing::{error, warn};

use crate::items::{ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::{
    fitting,
    moves::{self, MoveError},
};

const SERVICE: &str = "skillMgr";

const FLAG_SKILL: i32 = 7;
const FLAG_SKILL_IN_TRAINING: i32 = 61;
const FLAG_IMPLANT: i32 = 89;

const CATEGORY_SKILL: i32 = 16;
const CATEGORY_IMPLANT: i32 = 20;
const ATTR_IMPLANT_SLOT: i32 = 331;

const ATTR_PRIMARY: i32 = 180;
const ATTR_SECONDARY: i32 = 181;
const ATTR_SKILL_TIME_CONSTANT: i32 = 275;
const ATTR_SKILL_POINTS: i32 = 276;
const ATTR_SKILL_LEVEL: i32 = 280;

/// The character's trained level in each of `types` (missing ones are 0).
/// Works on a client or inside a transaction.
pub async fn levels(
    db: &impl tokio_postgres::GenericClient,
    char_id: i64,
    types: &[i32],
) -> Result<std::collections::HashMap<i32, i64>, tokio_postgres::Error> {
    let rows = db
        .query(
            r#"SELECT e."typeID", max(coalesce(a."valueInt", a."valueFloat"::bigint))
                 FROM entity e
                 JOIN entity_attributes a ON a."itemID" = e."itemID" AND a."attributeID" = $3
                WHERE e."ownerID" = $1 AND e."flag" IN ($4, $5) AND e."typeID" = ANY($2)
                GROUP BY e."typeID""#,
            &[
                &(char_id as i32),
                &types,
                &ATTR_SKILL_LEVEL,
                &FLAG_SKILL,
                &FLAG_SKILL_IN_TRAINING,
            ],
        )
        .await?;
    let mut out: std::collections::HashMap<i32, i64> = types.iter().map(|t| (*t, 0)).collect();
    for r in rows {
        out.insert(r.get(0), r.get::<_, Option<i64>>(1).unwrap_or(0));
    }
    Ok(out)
}

pub async fn skill_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(method_call(&call.method, &call.args, ctx).await)
}

async fn method_call(method: &str, args: &[Value], ctx: &Ctx<'_>) -> Outcome {
    let item_id = args.first().and_then(|v| v.as_i64()).unwrap_or(0);
    match method {
        "GetEndOfTraining" => Outcome::Value(end_of_training(ctx, item_id).await),
        "CharStartTrainingSkill" => Outcome::Value(start_training(ctx, item_id).await),
        "CharStopTrainingSkill" => Outcome::Value(stop_training(ctx, item_id).await),
        "CharAddImplant" => change_implant(ctx, item_id, false).await,
        "CharUseBooster" => super::boosters::consume(ctx,args).await,
        "RemoveImplantFromCharacter" => change_implant(ctx, item_id, true).await,
        _ => Outcome::Unknown(format!("{}::{}", SERVICE, method)),
    }
}

async fn end_of_training(ctx: &Ctx<'_>, skill_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else {
        return Value::None;
    };
    let Some(it) = item(ctx, skill_id).await else {
        return Value::None;
    };
    if it.owner_id != char_id || it.flag != FLAG_SKILL_IN_TRAINING {
        return Value::None;
    }
    match training_eta(ctx, char_id, skill_id, it.type_id).await {
        Some(eta) => Value::Long(eta),
        None => Value::None,
    }
}

async fn start_training(ctx: &Ctx<'_>, skill_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else {
        return Value::None;
    };
    let Some(it) = item(ctx, skill_id).await else {
        warn!("CharStartTrainingSkill: no item {skill_id}");
        return Value::None;
    };
    if it.owner_id != char_id || it.category_id != CATEGORY_SKILL {
        warn!("CharStartTrainingSkill: {skill_id} is not {char_id}'s skill");
        return Value::None;
    }
    let level = attr(ctx, skill_id, it.type_id, ATTR_SKILL_LEVEL)
        .await
        .unwrap_or(0.0);
    if level >= 5.0 {
        return Value::None;
    }

    let halt = r#"UPDATE "entity" SET "flag" = $1
                   WHERE "ownerID" = $2 AND "flag" = $3 AND "itemID" <> $4"#;
    if let Err(e) = ctx
        .db
        .execute(
            halt,
            &[
                &FLAG_SKILL,
                &(char_id as i32),
                &FLAG_SKILL_IN_TRAINING,
                &(skill_id as i32),
            ],
        )
        .await
    {
        error!(
            "CharStartTrainingSkill (halt): {err}",
            err = crate::logging::chain(&e)
        );
        return Value::None;
    }

    let start = r#"UPDATE "entity" SET "flag" = $1 WHERE "itemID" = $2 AND "ownerID" = $3"#;
    if let Err(e) = ctx
        .db
        .execute(
            start,
            &[
                &FLAG_SKILL_IN_TRAINING,
                &(skill_id as i32),
                &(char_id as i32),
            ],
        )
        .await
    {
        error!(
            "CharStartTrainingSkill (start): {err}",
            err = crate::logging::chain(&e)
        );
    }
    Value::None
}

async fn stop_training(ctx: &Ctx<'_>, skill_id: i64) -> Value {
    let Some(char_id) = ctx.session.char_id else {
        return Value::None;
    };
    let sql = r#"UPDATE "entity" SET "flag" = $1
                   WHERE "itemID" = $2 AND "ownerID" = $3 AND "flag" = $4"#;
    if let Err(e) = ctx
        .db
        .execute(
            sql,
            &[
                &FLAG_SKILL,
                &(skill_id as i32),
                &(char_id as i32),
                &FLAG_SKILL_IN_TRAINING,
            ],
        )
        .await
    {
        error!(
            "CharStopTrainingSkill: {err}",
            err = crate::logging::chain(&e)
        );
    }
    Value::None
}

async fn change_implant(ctx: &Ctx<'_>, item_id: i64, remove: bool) -> Outcome {
    let Some(char_id) = ctx.session.char_id else {
        return Outcome::custom("Select a character first.");
    };
    let _trade_guard = crate::svc::pvptrade::inventory_guard(ctx.world, char_id).await;
    let result: Result<_, MoveError> = async {
        let mut db = crate::db::connect(ctx.db_url).await?;
        let mut txn = ItemTxn::begin(&mut db).await?;
        if !crate::svc::login::lock_character_lifecycle(txn.tx(), char_id as i32).await? {
            return moves::refuse("That character no longer exists.");
        }
        let current = txn.tx().query_opt(r#"SELECT "stationID","shipID","solarSystemID" FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#, &[&(char_id as i32)]).await?;
        let Some(current) = current else { return moves::refuse("That character no longer exists.") };
        if crate::svc::rows::int(&current,0).unwrap_or(0) != ctx.session.station_id.unwrap_or(0)
            || crate::svc::rows::int(&current,1).filter(|id| *id>0) != ctx.session.ship_id
            || crate::svc::rows::int(&current,2).filter(|id| *id>0) != ctx.session.solar_system_id2
        {
            return moves::refuse("Your location changed. Try again after the session change.");
        }
        let Some(implant) = txn.item(item_id).await? else { return moves::refuse("That implant no longer exists.") };
        if implant.owner_id != char_id || implant.category_id != CATEGORY_IMPLANT as i64 {
            return moves::refuse("That is not your implant.");
        }
        let before = fitting::snapshot(txn.tx(), ctx.dogma, char_id, ctx.session.ship_id).await?;
        if remove {
            if implant.location_id != char_id || implant.flag != FLAG_IMPLANT as i64 {
                return moves::refuse("Only implants in your current clone can be unplugged.");
            }
            txn.destroy(item_id).await?;
        } else {
            if !moves::reachable(ctx, &mut txn, &implant).await? || !matches!(implant.flag, 0 | 4 | 5) {
                return moves::refuse("That implant must be in accessible cargo or your hangar.");
            }
            let slot = txn.tx().query_opt(r#"SELECT COALESCE("valueFloat","valueInt"::float8) FROM "dgmTypeAttributes" WHERE "typeID"=$1 AND "attributeID"=$2"#, &[&(implant.type_id as i32),&ATTR_IMPLANT_SLOT]).await?
                .and_then(|r| r.get::<_,Option<f64>>(0)).filter(|slot| slot.is_finite() && *slot>0.0);
            let Some(slot) = slot else { return moves::refuse("That item cannot be installed as an implant.") };
            fitting::check_skills(txn.tx(), char_id, implant.type_id).await?;
            let occupied = txn.tx().query_opt(r#"SELECT 1 FROM entity e JOIN "dgmTypeAttributes" a ON a."typeID"=e."typeID" AND a."attributeID"=$2 WHERE e."ownerID"=$1 AND e."locationID"=$1 AND e.flag=89 AND COALESCE(a."valueFloat",a."valueInt"::float8)=$3 LIMIT 1"#, &[&(char_id as i32),&ATTR_IMPLANT_SLOT,&slot]).await?.is_some();
            if occupied { return moves::refuse("There is already an implant in that slot. Unplug it first."); }
            if implant.quantity < 1 { return moves::refuse("That implant stack is empty."); }
            if implant.quantity > 1 {
                txn.set_quantity(item_id, implant.quantity - 1).await?;
                txn.create(NewItem { type_id: implant.type_id, owner_id: char_id, location_id: char_id,
                    flag: FLAG_IMPLANT as i64, quantity: 1, singleton: true }).await?;
            } else {
                txn.move_to(item_id, char_id, FLAG_IMPLANT as i64).await?;
                txn.set_singleton(item_id, true).await?;
            }
        }
        let origins = txn.origins();
        Ok((txn.commit().await?, origins, before))
    }.await;
    match result {
        Ok((events, origins, before)) => {
            crate::items::notify(ctx.world, ctx.client_id, events);
            crate::svc::pvptrade::offers_changed(ctx.world, &origins);
            fitting::push_attribute_changes(ctx, &before).await;
            refresh_implant_ship(ctx).await;
            Outcome::Value(Value::None)
        }
        Err(MoveError::Refused(why)) => Outcome::custom(why),
        Err(MoveError::Reactivation{item,delay})=>super::moves::reactivation_error(item,delay),
        Err(MoveError::Db(e)) => {
            error!("changing implant: {}", crate::logging::chain(&e));
            Outcome::custom("The implant could not be changed.")
        }
    }
}

async fn refresh_implant_ship(ctx: &Ctx<'_>) {
    let (Some(char_id), Some(ship_id), Some(system)) = (
        ctx.session.char_id,
        ctx.session.ship_id,
        ctx.session.solar_system_id,
    ) else {
        return;
    };
    let active = {
        let world = ctx.world.lock();
        let Some(ship) = world
            .parks
            .get(&system)
            .and_then(|p| p.ships.get(&ship_id))
            .filter(|s| s.client_id == ctx.client_id && s.pilot.char_id == char_id)
        else {
            return;
        };
        ship.fight
            .as_ref()
            .map(|f| f.applied.clone())
            .unwrap_or_default()
    };
    match crate::svc::inflight::profile(
        ctx.db,
        ctx.dogma,
        char_id,
        ship_id,
        &active,
        crate::combat::now_ms(),
    )
    .await
    {
        Ok(Some(profile)) => {
            let mut world = ctx.world.lock();
            let Some(park) = world.parks.get_mut(&system) else {
                return;
            };
            let tick = park.tick;
            let Some(ship) = park.ships.get_mut(&ship_id).filter(|s| {
                s.client_id == ctx.client_id && s.pilot.char_id == char_id && s.is_player()
            }) else {
                return;
            };
            let speed = ship.stats.max_velocity;
            crate::svc::inflight::apply_profile(ship, profile, crate::combat::now_ms());
            if (speed - ship.stats.max_velocity).abs() > 1e-6 {
                let event = crate::svc::space::entry(
                    tick,
                    "SetMaxSpeed",
                    vec![Value::Int(ship_id), Value::Double(ship.stats.max_velocity)],
                );
                crate::svc::space::broadcast(&world, system, None, vec![event]);
            }
        }
        Ok(None) => {}
        Err(e) => error!("refreshing implant effects: {}", crate::logging::chain(&e)),
    }
}

async fn training_eta(ctx: &Ctx<'_>, char_id: i64, skill_id: i64, type_id: i64) -> Option<i64> {
    let stc = attr(ctx, skill_id, type_id, ATTR_SKILL_TIME_CONSTANT)
        .await
        .unwrap_or(0.0);
    let sp = attr(ctx, skill_id, type_id, ATTR_SKILL_POINTS)
        .await
        .unwrap_or(0.0);
    let level = attr(ctx, skill_id, type_id, ATTR_SKILL_LEVEL)
        .await
        .unwrap_or(0.0);
    let sp_hi = sp_for_level(stc * 250.0, level)?;

    let primary = attr(ctx, skill_id, type_id, ATTR_PRIMARY)
        .await
        .unwrap_or(0.0) as i32;
    let secondary = attr(ctx, skill_id, type_id, ATTR_SECONDARY)
        .await
        .unwrap_or(0.0) as i32;
    let mut spm = 0.0;
    if primary != 0 {
        spm += char_attr(ctx, char_id, primary).await;
    }
    if secondary != 0 {
        spm += char_attr(ctx, char_id, secondary).await / 2.0;
    }
    if spm.is_nan() || spm <= 0.0 {
        return None;
    }

    let minutes = ((sp_hi - sp) / spm).max(0.0);
    if !minutes.is_finite() {
        return None;
    }
    Some(crate::bulkdata::filetime_now() + (minutes * 60.0 * 10_000_000.0) as i64)
}

fn sp_for_level(stc: f64, level: f64) -> Option<f64> {
    if level >= 5.0 {
        None
    } else if level < 0.0 {
        Some(0.0)
    } else if level == 0.0 {
        Some(stc)
    } else {
        Some(stc * 2.0f64.powf(2.5 * level))
    }
}

struct Item {
    type_id: i64,
    owner_id: i64,
    flag: i32,
    category_id: i32,
}

async fn item(ctx: &Ctx<'_>, item_id: i64) -> Option<Item> {
    let sql = r#"SELECT e."typeID", e."ownerID", e."flag", g."groupID", g."categoryID"
                   FROM "entity" e
                   JOIN "invTypes" t ON t."typeID" = e."typeID"
                   JOIN "invGroups" g ON g."groupID" = t."groupID"
                  WHERE e."itemID" = $1"#;
    match ctx.db.query_opt(sql, &[&(item_id as i32)]).await {
        Ok(Some(r)) => Some(Item {
            type_id: r.get::<_, i32>(0) as i64,
            owner_id: r.get::<_, i32>(1) as i64,
            flag: r.get::<_, i32>(2),
            category_id: r.get::<_, i32>(4),
        }),
        Ok(None) => None,
        Err(e) => {
            error!("item {item_id}: {err}", err = crate::logging::chain(&e));
            None
        }
    }
}

#[cfg(test)]
mod implant_tests {
    use super::*;
    use crate::{session::Session, test_support::Database};

    async fn pilot(db: &Database) -> (Session, i32) {
        let owner = db.id().await;
        let hull = db.id().await;
        let place = db.db.query_one(r#"SELECT "stationID","solarSystemID" FROM "staStations" ORDER BY "stationID" LIMIT 1"#, &[]).await.unwrap();
        let station: i32 = place.get(0);
        let system: i32 = place.get(1);
        db.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","shipID","stationID","solarSystemID") VALUES ($1,$2,$3,$4,$5)"#, &[&owner,&format!("Implant fixture {owner}"),&hull,&station,&system]).await.unwrap();
        db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES ($1,1373,$1,$3,0,1,1),($2,587,$1,$3,4,1,1)"#, &[&owner,&hull,&station]).await.unwrap();
        (
            Session {
                char_id: Some(owner as i64),
                ship_id: Some(hull as i64),
                station_id: Some(station as i64),
                solar_system_id2: Some(system as i64),
                ..Default::default()
            },
            owner,
        )
    }

    async fn implant(db: &Database, owner: i32, location: i32, flag: i32, quantity: i32) -> i32 {
        let id = db.id().await;
        db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES ($1,13241,$2,$3,$4,$5,0)"#, &[&id,&owner,&location,&flag,&quantity]).await.unwrap();
        id
    }

    async fn train(db: &Database, owner: i32) {
        for (kind, level) in fitting::required_skill_levels(&db.db, 13241).await.unwrap() {
            let id = db.id().await;
            db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES ($1,$2,$3,$3,7,1,1)"#, &[&id,&kind,&owner]).await.unwrap();
            db.db.execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt") VALUES ($1,280,$2)"#, &[&id,&level]).await.unwrap();
        }
    }

    fn succeeded(result: &Outcome) -> bool {
        matches!(result, Outcome::Value(Value::None))
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn implant_custody_skills_slots_and_concurrent_installation_are_enforced() {
        let db = Database::load().await;
        let (session, owner) = pilot(&db).await;
        let station = session.station_id.unwrap() as i32;
        let stored_clone = db.id().await;
        let stored = implant(&db, owner, stored_clone, 89, 1).await;
        let remote = implant(&db, owner, station + 1, 4, 1).await;
        let foreign = implant(&db, owner + 1000, station, 4, 1).await;
        let first = implant(&db, owner, station, 4, 2).await;
        let second = implant(&db, owner, station, 4, 2).await;
        let ctx = db.ctx(&session);
        for id in [stored, remote, foreign] {
            assert!(!succeeded(&change_implant(&ctx, id as i64, false).await));
            assert!(!succeeded(&change_implant(&ctx, id as i64, true).await));
        }
        assert!(!fitting::required_skill_levels(&db.db, 13241)
            .await
            .unwrap()
            .is_empty());
        assert!(
            !succeeded(&change_implant(&ctx, first as i64, false).await),
            "implant prerequisites must be trained"
        );
        train(&db, owner).await;
        let (one, two) = tokio::join!(
            change_implant(&ctx, first as i64, false),
            change_implant(&ctx, second as i64, false)
        );
        assert_eq!(
            usize::from(succeeded(&one)) + usize::from(succeeded(&two)),
            1,
            "concurrent implants compete for the same slot"
        );
        let active = db.db.query(r#"SELECT "itemID",quantity,singleton::integer FROM entity WHERE "ownerID"=$1 AND "locationID"=$1 AND flag=89"#, &[&owner]).await.unwrap();
        assert_eq!(active.len(), 1);
        let active_id: i32 = active[0].get(0);
        assert_eq!(active[0].get::<_, i32>(1), 1);
        assert_eq!(active[0].get::<_, i32>(2), 1);
        let cargo_quantity: i64 = db
            .db
            .query_one(
                r#"SELECT sum(quantity) FROM entity WHERE "itemID"=ANY($1)"#,
                &[&vec![first, second]],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            cargo_quantity, 3,
            "install exactly one implant from the winning stack"
        );
        for id in [active_id, stored] {
            let call = Call {
                service: "invcontainer".into(),
                method: "Add".into(),
                args: vec![Value::Int(id as i64)],
                kwargs: vec![],
            };
            let moved = moves::container_write(
                &ctx,
                crate::svc::inventory::Container::Special {
                    container_id: crate::svc::inventory::CONTAINER_HANGAR,
                    owner_id: None,
                },
                &call,
            )
            .await
            .unwrap();
            assert!(
                !succeeded(&moved),
                "generic inventory moves must not extract installed implants"
            );
        }
        assert!(succeeded(
            &change_implant(&ctx, active_id as i64, true).await
        ));
        assert!(db
            .db
            .query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#, &[&active_id])
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            db.db
                .query_one(
                    r#"SELECT "locationID" FROM entity WHERE "itemID"=$1"#,
                    &[&stored]
                )
                .await
                .unwrap()
                .get::<_, i32>(0),
            stored_clone
        );
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn implant_installation_and_unplugging_roll_back_inventory_and_attributes() {
        let db = Database::load().await;
        let (session, owner) = pilot(&db).await;
        train(&db, owner).await;
        let stack = implant(&db, owner, session.station_id.unwrap() as i32, 4, 2).await;
        let ctx = db.ctx(&session);
        let constraint = format!("implant_failure_{owner}");
        db.db.batch_execute(&format!(r#"ALTER TABLE entity ADD CONSTRAINT {constraint} CHECK ("ownerID"<>{owner} OR flag<>89) NOT VALID"#)).await.unwrap();
        let result = change_implant(&ctx, stack as i64, false).await;
        db.db
            .batch_execute(&format!("ALTER TABLE entity DROP CONSTRAINT {constraint}"))
            .await
            .unwrap();
        assert!(!succeeded(&result));
        assert_eq!(
            db.db
                .query_one(
                    r#"SELECT quantity FROM entity WHERE "itemID"=$1"#,
                    &[&stack]
                )
                .await
                .unwrap()
                .get::<_, i32>(0),
            2,
            "failed insert must restore the source stack"
        );
        assert!(succeeded(&change_implant(&ctx, stack as i64, false).await));
        let active: i32 = db
            .db
            .query_one(
                r#"SELECT "itemID" FROM entity WHERE "ownerID"=$1 AND "locationID"=$1 AND flag=89"#,
                &[&owner],
            )
            .await
            .unwrap()
            .get(0);
        db.db.execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueFloat") VALUES ($1,331,8)"#, &[&active]).await.unwrap();
        let guard = format!("implant_delete_guard_{owner}");
        db.db.batch_execute(&format!(r#"CREATE TABLE {guard} (item integer REFERENCES entity("itemID")); INSERT INTO {guard} VALUES ({active})"#)).await.unwrap();
        let result = change_implant(&ctx, active as i64, true).await;
        db.db
            .batch_execute(&format!("DROP TABLE {guard}"))
            .await
            .unwrap();
        assert!(!succeeded(&result));
        assert!(
            db.db
                .query_opt(
                    r#"SELECT 1 FROM entity_attributes WHERE "itemID"=$1"#,
                    &[&active]
                )
                .await
                .unwrap()
                .is_some(),
            "failed item removal restores attributes too"
        );
        assert!(succeeded(&change_implant(&ctx, active as i64, true).await));
        assert!(db
            .db
            .query_opt(
                r#"SELECT 1 FROM entity_attributes WHERE "itemID"=$1"#,
                &[&active]
            )
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn implant_changes_refresh_the_live_ship_without_restoring_charge() {
        let db = Database::load().await;
        let (mut session, owner) = pilot(&db).await;
        train(&db, owner).await;
        let hull = session.ship_id.unwrap() as i32;
        let system = session.solar_system_id2.unwrap() as i32;
        db.db
            .execute(
                r#"UPDATE "character_" SET "stationID"=0 WHERE "characterID"=$1"#,
                &[&owner],
            )
            .await
            .unwrap();
        db.db
            .execute(
                r#"UPDATE entity SET "locationID"=$2,flag=0 WHERE "itemID"=ANY($1)"#,
                &[&vec![owner, hull], &system],
            )
            .await
            .unwrap();
        session.station_id = None;
        session.solar_system_id = Some(system as i64);
        let implant = implant(&db, owner, hull, 5, 1).await;
        let ctx = db.ctx(&session);
        let mut ship = crate::svc::inflight::ship_for_space(&ctx, owner as i64, hull as i64)
            .await
            .unwrap();
        let initial = ship.stats.warp_capacitor_need;
        ship.fight
            .as_mut()
            .unwrap()
            .vitals
            .capacitor
            .set(crate::combat::now_ms(), 17.0);
        ship.fight.as_mut().unwrap().vitals.capacitor.recharge_ms = 0.0;
        let mut park =
            crate::ballpark::Park::new(system as i64, vec![], crate::destiny::tick_now());
        park.ships.insert(hull as i64, ship);
        db.world.lock().parks.insert(system as i64, park);
        assert!(succeeded(
            &change_implant(&ctx, implant as i64, false).await
        ));
        {
            let world = db.world.lock();
            let ship = &world.parks[&(system as i64)].ships[&(hull as i64)];
            assert!((ship.stats.warp_capacitor_need - initial * 0.94).abs() < initial * 1e-10);
            assert!(
                ship.fight
                    .as_ref()
                    .unwrap()
                    .vitals
                    .capacitor
                    .at(crate::combat::now_ms())
                    < 18.0
            );
        }
        assert!(succeeded(&change_implant(&ctx, implant as i64, true).await));
        let world = db.world.lock();
        assert!(
            (world.parks[&(system as i64)].ships[&(hull as i64)]
                .stats
                .warp_capacitor_need
                - initial)
                .abs()
                < initial * 1e-10
        );
    }
}

async fn attr(ctx: &Ctx<'_>, item_id: i64, type_id: i64, attribute_id: i32) -> Option<f64> {
    let sql = r#"SELECT COALESCE(
                     (SELECT COALESCE(ea."valueFloat", ea."valueInt"::double precision)
                        FROM "entity_attributes" ea
                       WHERE ea."itemID" = $1 AND ea."attributeID" = $3),
                     (SELECT COALESCE(ta."valueFloat", ta."valueInt"::double precision)
                        FROM "dgmTypeAttributes" ta
                       WHERE ta."typeID" = $2 AND ta."attributeID" = $3),
                     (SELECT at."defaultValue"::double precision
                        FROM "dgmAttributeTypes" at
                       WHERE at."attributeID" = $3))"#;
    match ctx
        .db
        .query_opt(sql, &[&(item_id as i32), &(type_id as i32), &attribute_id])
        .await
    {
        Ok(Some(r)) => r.get::<_, Option<f64>>(0),
        Ok(None) => None,
        Err(e) => {
            error!(
                "attribute {attribute_id} of {item_id}: {err}",
                err = crate::logging::chain(&e)
            );
            None
        }
    }
}

async fn char_attr(ctx: &Ctx<'_>, char_id: i64, attribute_id: i32) -> f64 {
    let own = r#"SELECT COALESCE(ea."valueFloat", ea."valueInt"::double precision)
                   FROM "entity_attributes" ea
                  WHERE ea."itemID" = $1 AND ea."attributeID" = $2"#;
    if let Ok(Some(r)) = ctx
        .db
        .query_opt(own, &[&(char_id as i32), &attribute_id])
        .await
    {
        if let Some(v) = r.get::<_, Option<f64>>(0) {
            return v;
        }
    }

    let column = match attribute_id {
        164 => "charisma",
        165 => "intelligence",
        166 => "memory",
        167 => "perception",
        168 => "willpower",
        _ => return 0.0,
    };
    let sql = r#"SELECT b."charisma", b."intelligence", b."memory", b."perception", b."willpower"
                   FROM "character_" c
                   JOIN "chrBloodlines" b ON b."bloodlineID" = c."bloodlineID"
                  WHERE c."characterID" = $1"#;
    match ctx.db.query_opt(sql, &[&(char_id as i32)]).await {
        Ok(Some(r)) => {
            let idx = match column {
                "charisma" => 0,
                "intelligence" => 1,
                "memory" => 2,
                "perception" => 3,
                _ => 4,
            };
            r.get::<_, Option<i32>>(idx)
                .map(|v| v as f64)
                .unwrap_or(0.0)
        }
        Ok(None) => 0.0,
        Err(e) => {
            error!(
                "character attribute {attribute_id} of {char_id}: {err}",
                err = crate::logging::chain(&e)
            );
            0.0
        }
    }
}
