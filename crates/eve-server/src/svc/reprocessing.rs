//! `reprocessingSvc`, bound per station (moniker.py:470): the reprocessing plant
//! (base_reprocessingplant.py) and the reprocess dialog (reprocessingsvc.py).
//!
//! The client only displays the numbers it is given. Yields use the formula of
//! the period: the station's efficiency plus 37.5% scaled by Refining (+2% a
//! level) and Refinery Efficiency (+4% a level), capped at 100%; anything that
//! is not ore gets a further +5% a level of Scrapmetal Processing. The station
//! keeps its take (reprocessingStationsTake), less 0.75% per point of standing
//! with the owning corporation. Ore-specific skills are not modelled.

use std::collections::HashMap;

use eve_marshal::Value;
use tracing::{error, info};

use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Store, StoreError};
use crate::svc::bound;
use crate::items::{self, DbResult, ItemTxn, NewItem};
use crate::svc::inventory::FLAG_HANGAR;
use crate::svc::rows::{key_val, row};

const SKILL_REFINING: i64 = 3385;
const SKILL_REFINERY_EFFICIENCY: i64 = 3389;
const SKILL_SCRAPMETAL_PROCESSING: i64 = 12196;
const FLAG_SKILL: i64 = 7;
const ATTR_SKILL_LEVEL: i64 = 280;

const CATEGORY_BLUEPRINT: i64 = 9;
const CATEGORY_REACTION: i64 = 24;
const CATEGORY_ASTEROID: i32 = 25;
const GROUP_MINERAL: i64 = 18;
const GROUP_ROOKIESHIP: i64 = 237;
const TYPE_CREDITS: i64 = 29;
const TYPE_BOOKMARK: i64 = 51;

/// A type's reprocessing materials, in the data's order: the same materials
/// config.BulkData.ramtypematerials gives the client.
fn reprocessing_materials(type_id: i64) -> impl Iterator<Item = crate::sde::Row<'static>> {
    crate::sde::table("TL2MaterialsForTypeWithActivity")
        .find("typeID", type_id)
        .filter(|m| m.int("activity") == Some(6) && m.float("damagePerJob") == Some(1.0))
}

pub async fn reprocessing_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let station = bound::parse(&call.service)
        .and_then(|b| b.params.first().copied())
        .or(ctx.session.station_id)?;
    let value = match call.method.as_str() {
        "GetReprocessingInfo" => reprocessing_info(&terms(ctx, station)?),
        // GetOptionsForItemTypes({typeID: 0}) -> {typeID: option row}.
        "GetOptionsForItemTypes" => {
            let types: Vec<i32> = match call.args.first() {
                Some(Value::Dict(d)) => d.iter().filter_map(|(k, _)| k.as_i64()).map(|t| t as i32).collect(),
                _ => Vec::new(),
            };
            options(&types)
        }
        "GetQuotes" => {
            let terms = terms(ctx, station)?;
            let quotes = quotes(ctx, &terms, station, &item_ids(call));
            Value::Dict(quotes.into_iter().map(|q| (Value::Int(q.item_id), q.to_value())).collect())
        }
        "Reprocess" => return Some(reprocess(ctx, station, &item_ids(call)).await),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

fn item_ids(call: &Call) -> Vec<i64> {
    call.args.first().and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64()).collect()
}

/// What this station offers this character.
struct Terms {
    station_yield: f64,
    /// Station and refining skills together.
    combined: f64,
    scrapmetal: f64,
    /// The fraction of the recovered materials the station keeps.
    tax: f64,
    standing: f64,
    char_id: i64,
}

impl Terms {
    fn yield_for(&self, category: i32) -> f64 {
        if category == CATEGORY_ASTEROID {
            self.combined
        } else {
            (self.combined * (1.0 + 0.05 * self.scrapmetal)).min(1.0)
        }
    }
}

/// The highest level of `skill` among the character's trained skills (0
/// when it has none).
fn skill_level(db: &Db, char_id: i64, skill: i64) -> f64 {
    let attributes = db.table("entity_attributes");
    db.table("entity")
        .find(&["ownerID", "typeID"], &[store::Value::Int(char_id), store::Value::Int(skill)])
        .filter(|e| e.int("flag") == Some(FLAG_SKILL))
        .filter_map(|e| attributes.get(&[store::Value::Int(e.int("itemID")?), store::Value::Int(ATTR_SKILL_LEVEL)]))
        // A float level is rounded to the integer it is, as `::bigint` did.
        .filter_map(|a| a.int("valueInt").or_else(|| a.float("valueFloat").map(|f| f.round_ties_even() as i64)))
        .max()
        .unwrap_or(0) as f64
}

fn terms(ctx: &Ctx<'_>, station: i64) -> Option<Terms> {
    let char_id = ctx.session.char_id?;
    let (station_yield, take, standing, refining, efficiency, scrapmetal) = ctx.store.read(|db| {
        let st = db.table("staStations").get_id(station)?;
        let standing = st
            .int("corporationID")
            .and_then(|corp| db.table("chrNPCStandings").get(&[store::Value::Int(char_id), store::Value::Int(corp)]))
            .and_then(|n| n.float("standing"));
        Some((
            st.float("reprocessingEfficiency").unwrap_or(0.0),
            st.float("reprocessingStationsTake").unwrap_or(0.0),
            standing.unwrap_or(0.0),
            skill_level(db, char_id, SKILL_REFINING),
            skill_level(db, char_id, SKILL_REFINERY_EFFICIENCY),
            skill_level(db, char_id, SKILL_SCRAPMETAL_PROCESSING),
        ))
    })?;
    let combined =
        (station_yield + 0.375 * (1.0 + 0.02 * refining) * (1.0 + 0.04 * efficiency)).min(1.0);
    Some(Terms {
        station_yield,
        combined,
        scrapmetal,
        tax: (take - 0.0075 * standing).max(0.0),
        standing,
        char_id,
    })
}

/// {'yield', 'combinedyield', 'wetake': (fraction, standing)}
/// (base_reprocessingplant.py:93-105).
fn reprocessing_info(t: &Terms) -> Value {
    let key = |k: &str| Value::bytes(k);
    Value::Dict(vec![
        (key("yield"), Value::Double(t.station_yield)),
        (key("combinedyield"), Value::Double(t.combined)),
        (key("wetake"), Value::Tuple(vec![Value::Double(t.tax), Value::Double(t.standing)])),
    ])
}

/// The client's own rule (cfg.py GetReprocessingOptions): a type with materials
/// is refinable if it is ore and recyclable otherwise; money, bookmarks,
/// minerals, rookie ships, blueprints and reactions are neither.
fn options(types: &[i32]) -> Value {
    const HEADER: &[&str] = &["typeID", "isRecyclable", "isRefinable"];
    let sde = crate::sde::get();
    // Each known type once, in the data's (typeID) order.
    let mut types: Vec<i64> = types.iter().map(|t| *t as i64).collect();
    types.sort_unstable();
    types.dedup();
    let out = types
        .into_iter()
        .filter_map(|type_id| Some((type_id, sde.group_and_category(type_id)?)))
        .map(|(type_id, (group, category))| {
            let asteroid = category == CATEGORY_ASTEROID as i64;
            let usable = ![TYPE_CREDITS, TYPE_BOOKMARK].contains(&type_id)
                && ![GROUP_MINERAL, GROUP_ROOKIESHIP].contains(&group)
                && ![CATEGORY_BLUEPRINT, CATEGORY_REACTION].contains(&category)
                && reprocessing_materials(type_id).next().is_some();
            let flags = (usable && !asteroid, usable && asteroid);
            let line = vec![Value::Int(type_id), Value::Int(flags.0.into()), Value::Int(flags.1.into())];
            (Value::Int(type_id), row(HEADER, line))
        })
        .collect();
    Value::Dict(out)
}

struct Recoverable {
    type_id: i64,
    client: i64,
    station: i64,
    unrecoverable: i64,
}

struct Quote {
    item_id: i64,
    quantity: i64,
    leftover: i64,
    recoverables: Vec<Recoverable>,
}

impl Quote {
    fn to_value(&self) -> Value {
        let recoverables = self
            .recoverables
            .iter()
            .map(|m| {
                key_val(vec![
                    ("typeID", Value::Int(m.type_id)),
                    ("client", Value::Int(m.client)),
                    ("station", Value::Int(m.station)),
                    ("unrecoverable", Value::Int(m.unrecoverable)),
                ])
            })
            .collect();
        key_val(vec![
            ("itemID", Value::Int(self.item_id)),
            ("quantityToProcess", Value::Int(self.quantity - self.leftover)),
            ("leftOvers", Value::Int(self.leftover)),
            ("recoverables", Value::List(recoverables)),
        ])
    }
}

/// Quotes for those of `item_ids` the character can reprocess here: its own
/// items in the station hangar or in a ship of its docked here, holding at
/// least one portion. The active ship is never offered.
fn quotes(ctx: &Ctx<'_>, t: &Terms, station: i64, item_ids: &[i64]) -> Vec<Quote> {
    let mut ids = item_ids.to_vec();
    ids.sort_unstable();
    ids.dedup();
    let active_ship = ctx.session.ship_id;
    let items: Vec<[Option<i64>; 3]> = ctx.store.read(|db| {
        let entity = db.table("entity");
        // In the hangar, or in a container (a ship) of the character's there.
        let here = |location: i64| {
            location == station
                || entity.get_id(location).is_some_and(|s| s.int("ownerID") == Some(t.char_id) && s.int("locationID") == Some(station))
        };
        ids.iter()
            .filter(|id| Some(**id) != active_ship)
            .filter_map(|id| entity.get_id(*id))
            .filter(|e| e.int("ownerID") == Some(t.char_id) && e.int("locationID").is_some_and(here))
            .map(|e| [e.int("itemID"), e.int("typeID"), e.int("quantity")])
            .collect()
    });
    let sde = crate::sde::get();
    let mut materials: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    for type_id in items.iter().filter_map(|r| r[1]) {
        let mats: Vec<(i64, i64)> =
            reprocessing_materials(type_id).filter_map(|m| Some((m.int("requiredTypeID")?, m.int("quantity")?))).collect();
        if !mats.is_empty() {
            materials.insert(type_id, mats);
        }
    }

    let mut out = Vec::new();
    for r in &items {
        let [Some(item_id), Some(type_id), Some(quantity)] = *r else { continue };
        // An item of a type the data does not know is not reprocessed.
        let Some((_, category)) = sde.group_and_category(type_id) else { continue };
        let portion = sde.table("invTypes").get("typeID", type_id).and_then(|t| t.int("portionSize")).unwrap_or(1).max(1);
        let portions = quantity / portion;
        let Some(mats) = materials.get(&type_id) else { continue };
        if portions == 0 {
            continue;
        }
        let yield_ = t.yield_for(category as i32);
        let recoverables = mats
            .iter()
            .map(|&(material, per_portion)| {
                let total = per_portion * portions;
                let recovered = (total as f64 * yield_).floor() as i64;
                let station_share = (recovered as f64 * t.tax).floor() as i64;
                Recoverable {
                    type_id: material,
                    client: recovered - station_share,
                    station: station_share,
                    unrecoverable: total - recovered,
                }
            })
            .collect();
        out.push(Quote { item_id, quantity, leftover: quantity % portion, recoverables });
    }
    out
}

/// Reprocess(itemIDs): every quotable item is used up (a leftover short of a
/// portion stays), whatever it held goes to the hangar, and the character's
/// share of the materials is stacked in the hangar.
async fn reprocess(ctx: &Ctx<'_>, station: i64, item_ids: &[i64]) -> Outcome {
    let Some(terms) = terms(ctx, station) else { return Outcome::custom("You are not docked here.") };
    let quotes = quotes(ctx, &terms, station, item_ids);
    if quotes.is_empty() {
        return Outcome::custom("None of those items can be reprocessed here.");
    }
    match apply(ctx.store, terms.char_id, station, &quotes).await {
        Ok(events) => {
            items::notify(ctx.world, ctx.client_id, events);
            info!("reprocessed {} item(s) at station {station}", quotes.len());
            Outcome::Value(Value::None)
        }
        Err(e) => {
            error!("reprocessing at {station}: {}", crate::logging::chain(&e));
            Outcome::custom("Reprocessing failed; nothing was changed.")
        }
    }
}

async fn apply(store: &Store, char_id: i64, station: i64, quotes: &[Quote]) -> DbResult<Vec<Value>> {
    store.write(|tx| -> Result<_, StoreError> {
        let mut txn = ItemTxn::new(tx);
        let mut gained: HashMap<i64, i64> = HashMap::new();

        for q in quotes {
            if q.leftover > 0 {
                txn.set_quantity(q.item_id, q.leftover)?;
            } else {
                let held: Vec<i64> =
                    txn.db().table("entity").find(&["locationID"], &[store::Value::Int(q.item_id)]).filter_map(|r| r.int("itemID")).collect();
                for id in held {
                    txn.move_to(id, station, FLAG_HANGAR.into())?;
                }
                txn.destroy(q.item_id)?;
            }
            for m in q.recoverables.iter().filter(|m| m.client > 0) {
                *gained.entry(m.type_id).or_default() += m.client;
            }
        }
        for (type_id, quantity) in gained {
            let stack = NewItem {
                type_id,
                owner_id: char_id,
                location_id: station,
                flag: FLAG_HANGAR.into(),
                quantity,
                singleton: false,
            };
            txn.stack_onto(stack)?;
        }
        Ok(txn.events())
    })
}
