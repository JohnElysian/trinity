use eve_marshal::Value;
use tracing::error;

use crate::items::{ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, StoreError};
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
/// Works on the data as read or inside a transaction.
pub fn levels(
    db: &Db,
    char_id: i64,
    types: &[i32],
) -> Result<std::collections::HashMap<i32, i64>, StoreError> {
    let entity = db.table("entity");
    let attributes = db.table("entity_attributes");
    let mut out: std::collections::HashMap<i32, i64> = types.iter().map(|t| (*t, 0)).collect();
    for &type_id in types {
        // The highest level among the character's skills of this type; a
        // float level counts as `::bigint` makes it whole.
        let level = entity
            .find(&["ownerID", "typeID"], &[store::Value::Int(char_id), store::Value::Int(i64::from(type_id))])
            .filter(|e| matches!(e.int("flag"), Some(f) if f == i64::from(FLAG_SKILL) || f == i64::from(FLAG_SKILL_IN_TRAINING)))
            .filter_map(|e| attributes.get(&[e.value("itemID").clone(), store::Value::Int(i64::from(ATTR_SKILL_LEVEL))]))
            .filter_map(|a| a.int("valueInt").or_else(|| a.float("valueFloat").map(|v| v.round_ties_even() as i64)))
            .max();
        out.insert(type_id, level.unwrap_or(0));
    }
    Ok(out)
}

mod training;

pub use training::run as run_training;

/// At start, see `training::ensure`.
pub fn ensure(tx: &mut crate::store::Tx<'_>) -> Result<(), StoreError> {
    training::ensure(tx)
}

pub async fn skill_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(method_call(&call.method, &call.args, ctx).await)
}

async fn method_call(method: &str, args: &[Value], ctx: &Ctx<'_>) -> Outcome {
    let item_id = args.first().and_then(|v| v.as_i64()).unwrap_or(0);
    match method {
        "GetEndOfTraining" => {
            let char_id = ctx.session.char_id.unwrap_or(0);
            Outcome::Value(ctx.store.read(|db| training::end_of(db, char_id, item_id)).map_or(Value::None, Value::Long))
        }
        "CharStartTrainingSkill" => Outcome::Value(training::start(ctx, item_id).await),
        "CharStopTrainingSkill" => Outcome::Value(training::stop(ctx, item_id).await),
        "CharAddImplant" => change_implant(ctx, item_id, false).await,
        "CharUseBooster" => super::boosters::consume(ctx,args).await,
        "RemoveImplantFromCharacter" => change_implant(ctx, item_id, true).await,
        _ => Outcome::Unknown(format!("{}::{}", SERVICE, method)),
    }
}

async fn change_implant(ctx: &Ctx<'_>, item_id: i64, remove: bool) -> Outcome {
    let Some(char_id) = ctx.session.char_id else {
        return Outcome::custom("Select a character first.");
    };
    let _trade_guard = crate::svc::pvptrade::inventory_guard(ctx.world, char_id).await;
    let result: Result<_, MoveError> = ctx.store.write(|tx| {
        let mut txn = ItemTxn::new(tx);
        let current = txn.db().table("character_").get_id(char_id).map(|c| (c.int("stationID"), c.int("shipID"), c.int("solarSystemID")));
        let Some((station, ship, system)) = current else { return moves::refuse("That character no longer exists.") };
        if station.unwrap_or(0) != ctx.session.station_id.unwrap_or(0)
            || ship.filter(|id| *id>0) != ctx.session.ship_id
            || system.filter(|id| *id>0) != ctx.session.solar_system_id2
        {
            return moves::refuse("Your location changed. Try again after the session change.");
        }
        let Some(implant) = txn.item(item_id)? else { return moves::refuse("That implant no longer exists.") };
        if implant.owner_id != char_id || implant.category_id != CATEGORY_IMPLANT as i64 {
            return moves::refuse("That is not your implant.");
        }
        let before = fitting::snapshot(txn.db(), ctx.dogma, char_id, ctx.session.ship_id)?;
        if remove {
            if implant.location_id != char_id || implant.flag != FLAG_IMPLANT as i64 {
                return moves::refuse("Only implants in your current clone can be unplugged.");
            }
            txn.destroy(item_id)?;
        } else {
            if !moves::reachable(ctx, &mut txn, &implant)? || !matches!(implant.flag, 0 | 4 | 5) {
                return moves::refuse("That implant must be in accessible cargo or your hangar.");
            }
            let sde = crate::sde::get();
            let slot = sde.type_attribute(implant.type_id, ATTR_IMPLANT_SLOT).filter(|slot| slot.is_finite() && *slot>0.0);
            let Some(slot) = slot else { return moves::refuse("That item cannot be installed as an implant.") };
            fitting::check_skills(txn.db(), char_id, implant.type_id)?;
            // The implant types that go in that slot.
            let same_slot: Vec<i32> = crate::sde::table("dgmTypeAttributes").find("attributeID", ATTR_IMPLANT_SLOT as i64)
                .filter(|a| a.float("valueFloat").or_else(|| a.float("valueInt")) == Some(slot))
                .filter_map(|a| a.int("typeID").map(|t| t as i32))
                .collect();
            let occupied = txn.db().table("entity")
                .find(&["ownerID", "locationID", "flag"], &[store::Value::Int(char_id), store::Value::Int(char_id), store::Value::Int(i64::from(FLAG_IMPLANT))])
                .any(|e| e.int("typeID").is_some_and(|t| same_slot.iter().any(|s| i64::from(*s) == t)));
            if occupied { return moves::refuse("There is already an implant in that slot. Unplug it first."); }
            if implant.quantity < 1 { return moves::refuse("That implant stack is empty."); }
            if implant.quantity > 1 {
                txn.set_quantity(item_id, implant.quantity - 1)?;
                txn.create(NewItem { type_id: implant.type_id, owner_id: char_id, location_id: char_id,
                    flag: FLAG_IMPLANT as i64, quantity: 1, singleton: true })?;
            } else {
                txn.move_to(item_id, char_id, FLAG_IMPLANT as i64)?;
                txn.set_singleton(item_id, true)?;
            }
        }
        let origins = txn.origins();
        Ok((txn.events(), origins, before))
    });
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
    match ctx.store.read(|db| crate::svc::inflight::profile(
        db,
        ctx.dogma,
        char_id,
        ship_id,
        &active,
        crate::combat::now_ms(),
    )) {
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

#[cfg(test)]
mod implant_tests {
    use super::*;
    use crate::{session::Session, store::Tx, test_support::Database};

    fn entity(tx: &mut Tx<'_>, id: i64, type_id: i64, owner: i64, location: i64, flag: i64, quantity: i64, singleton: i64) {
        tx.insert("entity", &[
            ("itemID", store::Value::Int(id)), ("typeID", store::Value::Int(type_id)), ("ownerID", store::Value::Int(owner)),
            ("locationID", store::Value::Int(location)), ("flag", store::Value::Int(flag)),
            ("quantity", store::Value::Int(quantity)), ("singleton", store::Value::Int(singleton)),
        ]).unwrap();
    }

    async fn pilot(db: &Database) -> (Session, i64) {
        let owner = i64::from(db.id().await);
        let hull = i64::from(db.id().await);
        let (station, system) = db.store.write(|tx| {
            let place = tx.table("staStations").rows()
                .filter_map(|s| Some((s.int("stationID")?, s.int("solarSystemID"))))
                .min_by_key(|(station, _)| *station)
                .expect("a station");
            let (station, system) = (place.0, place.1.unwrap_or(0));
            tx.insert("character_", &[
                ("characterID", store::Value::Int(owner)), ("characterName", store::Value::Text(format!("Implant fixture {owner}").into())),
                ("shipID", store::Value::Int(hull)), ("stationID", store::Value::Int(station)), ("solarSystemID", store::Value::Int(system)),
            ])?;
            entity(tx, owner, 1373, owner, station, 0, 1, 1);
            entity(tx, hull, 587, owner, station, 4, 1, 1);
            Ok::<_, StoreError>((station, system))
        }).unwrap();
        (
            Session {
                char_id: Some(owner),
                ship_id: Some(hull),
                station_id: Some(station),
                solar_system_id2: Some(system),
                ..Default::default()
            },
            owner,
        )
    }

    async fn implant(db: &Database, owner: i64, location: i64, flag: i64, quantity: i64) -> i64 {
        let id = i64::from(db.id().await);
        db.store.write(|tx| {
            entity(tx, id, 13241, owner, location, flag, quantity, 0);
            Ok::<_, StoreError>(())
        }).unwrap();
        id
    }

    async fn train(db: &Database, owner: i64) {
        let required = db.store.read(|d| fitting::required_skill_levels(d, 13241)).unwrap();
        for (kind, level) in required {
            let id = i64::from(db.id().await);
            db.store.write(|tx| {
                entity(tx, id, i64::from(kind), owner, owner, 7, 1, 1);
                tx.insert("entity_attributes", &[("itemID", store::Value::Int(id)), ("attributeID", store::Value::Int(280)), ("valueInt", store::Value::Int(level))])?;
                Ok::<_, StoreError>(())
            }).unwrap();
        }
    }

    fn succeeded(result: &Outcome) -> bool {
        matches!(result, Outcome::Value(Value::None))
    }

    /// The character's installed implants: (itemID, quantity, singleton).
    fn installed(db: &Database, owner: i64) -> Vec<(i64, i64, i64)> {
        db.store.read(|d| {
            d.table("entity")
                .find(&["ownerID", "locationID", "flag"], &[store::Value::Int(owner), store::Value::Int(owner), store::Value::Int(89)])
                .map(|e| (e.int("itemID").unwrap(), e.int("quantity").unwrap(), e.int("singleton").unwrap()))
                .collect()
        })
    }

    #[tokio::test]
    async fn implant_custody_skills_slots_and_concurrent_installation_are_enforced() {
        let db = Database::load().await;
        let (session, owner) = pilot(&db).await;
        let station = session.station_id.unwrap();
        let stored_clone = i64::from(db.id().await);
        let stored = implant(&db, owner, stored_clone, 89, 1).await;
        let remote = implant(&db, owner, station + 1, 4, 1).await;
        let foreign = implant(&db, owner + 1000, station, 4, 1).await;
        let first = implant(&db, owner, station, 4, 2).await;
        let second = implant(&db, owner, station, 4, 2).await;
        let ctx = db.ctx(&session);
        for id in [stored, remote, foreign] {
            assert!(!succeeded(&change_implant(&ctx, id, false).await));
            assert!(!succeeded(&change_implant(&ctx, id, true).await));
        }
        assert!(!db.store.read(|d| fitting::required_skill_levels(d, 13241))
            .unwrap()
            .is_empty());
        assert!(
            !succeeded(&change_implant(&ctx, first, false).await),
            "implant prerequisites must be trained"
        );
        train(&db, owner).await;
        let (one, two) = tokio::join!(
            change_implant(&ctx, first, false),
            change_implant(&ctx, second, false)
        );
        assert_eq!(
            usize::from(succeeded(&one)) + usize::from(succeeded(&two)),
            1,
            "two implants compete for the same slot"
        );
        let active = installed(&db, owner);
        assert_eq!(active.len(), 1);
        let active_id = active[0].0;
        assert_eq!(active[0].1, 1);
        assert_eq!(active[0].2, 1);
        let cargo_quantity: i64 = db.store.read(|d| {
            [first, second].iter().filter_map(|id| d.table("entity").get_id(*id)?.int("quantity")).sum()
        });
        assert_eq!(
            cargo_quantity, 3,
            "install exactly one implant from the winning stack"
        );
        for id in [active_id, stored] {
            let call = Call {
                service: "invcontainer".into(),
                method: "Add".into(),
                args: vec![Value::Int(id)],
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
            &change_implant(&ctx, active_id, true).await
        ));
        assert!(db.store.read(|d| d.table("entity").get_id(active_id).is_none()));
        assert_eq!(
            db.store.read(|d| d.table("entity").get_id(stored).unwrap().int("locationID")),
            Some(stored_clone)
        );
    }

    #[tokio::test]
    async fn implant_installation_and_unplugging_move_inventory_and_attributes() {
        let db = Database::load().await;
        let (session, owner) = pilot(&db).await;
        train(&db, owner).await;
        let stack = implant(&db, owner, session.station_id.unwrap(), 4, 2).await;
        let ctx = db.ctx(&session);
        assert!(succeeded(&change_implant(&ctx, stack, false).await));
        assert_eq!(
            db.store.read(|d| d.table("entity").get_id(stack).unwrap().int("quantity")),
            Some(1),
            "one implant leaves the source stack"
        );
        let active = installed(&db, owner)[0].0;
        db.store.write(|tx| {
            tx.insert("entity_attributes", &[("itemID", store::Value::Int(active)), ("attributeID", store::Value::Int(331)), ("valueFloat", store::Value::Float(8.0))])?;
            Ok::<_, StoreError>(())
        }).unwrap();
        assert!(succeeded(&change_implant(&ctx, active, true).await));
        assert!(db.store.read(|d| d.table("entity_attributes").find(&["itemID"], &[store::Value::Int(active)]).next().is_none()));
    }

    #[tokio::test]
    async fn implant_changes_refresh_the_live_ship_without_restoring_charge() {
        let db = Database::load().await;
        let (mut session, owner) = pilot(&db).await;
        train(&db, owner).await;
        let hull = session.ship_id.unwrap();
        let system = session.solar_system_id2.unwrap();
        db.store.write(|tx| {
            let character = tx.table("character_").get_id(owner).unwrap().id;
            tx.update("character_", character, &[("stationID", store::Value::Int(0))])?;
            for id in [owner, hull] {
                let row = tx.table("entity").get_id(id).unwrap().id;
                tx.update("entity", row, &[("locationID", store::Value::Int(system)), ("flag", store::Value::Int(0))])?;
            }
            Ok::<_, StoreError>(())
        }).unwrap();
        session.station_id = None;
        session.solar_system_id = Some(system);
        let implant = implant(&db, owner, hull, 5, 1).await;
        let ctx = db.ctx(&session);
        let mut ship = crate::svc::inflight::ship_for_space(&ctx, owner, hull)
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
            crate::ballpark::Park::new(system, vec![], crate::destiny::tick_now());
        park.ships.insert(hull, ship);
        db.world.lock().parks.insert(system, park);
        assert!(succeeded(
            &change_implant(&ctx, implant, false).await
        ));
        {
            let world = db.world.lock();
            let ship = &world.parks[&system].ships[&hull];
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
        assert!(succeeded(&change_implant(&ctx, implant, true).await));
        let world = db.world.lock();
        assert!(
            (world.parks[&system].ships[&hull]
                .stats
                .warp_capacitor_need
                - initial)
                .abs()
                < initial * 1e-10
        );
    }
}

/// An item's own value of an attribute, `coalesce(valueFloat, valueInt)`.
fn own_attr(db: &Db, item_id: i64, attribute_id: i32) -> Option<f64> {
    let row = db.table("entity_attributes").get(&[store::Value::Int(item_id), store::Value::Int(i64::from(attribute_id))])?;
    row.float("valueFloat").or_else(|| row.float("valueInt"))
}

