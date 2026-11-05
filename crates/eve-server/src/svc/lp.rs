//! `LPSvc`: loyalty points and the LP store (Trinity, ui/station/lpstore.py).
//!
//! A pilot holds loyalty points with each NPC corporation
//! (`chrLoyaltyPoints`). The store's catalogue is CCP's `agtOfferDetails`: an
//! offer gives `agentTypeID1` x `agentQuantity1` for `loyaltyPointCost` LP and
//! the `charTypeIDn` x `charQuantityn` it asks for, where type 29 (Credits) is
//! the ISK price.
//!
//! The export does not say which corporation sells which offer, so this server
//! decides: an offer whose reward is named for a faction ("Caldari Navy ...",
//! "Dread Guristas ...") is sold by that faction's corporations, and the rest
//! (implants, hardwirings, boosters, plain blueprints) by every NPC
//! corporation. See [`offer_faction`].

use eve_marshal::Value;
use tracing::{info, warn};

use crate::items::{ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::store::{Db, StoreError, Tx, Value as V};
use crate::svc::rows::key_val;
use crate::svc::wallet::{self, Entry, Transfer};

/// `invTypes` 29, Credits: an offer asking for these asks for ISK.
const TYPE_CREDITS: i32 = 29;
const CATEGORY_BLUEPRINT: i32 = 9;
const FLAG_HANGAR: i64 = 4;
/// "Agent Miscellaneous": Trinity has no journal type of its own for the store.
const REF_AGENT_MISCELLANEOUS: i32 = 25;

/// Which faction sells an offer, from the first word of its reward's name;
/// None for offers every corporation sells.
fn offer_faction(reward_name: Option<&str>) -> Option<i64> {
    Some(match reward_name?.split(' ').next()? {
        "Caldari" => 500001,
        "Republic" | "Minmatar" => 500002,
        "Amarr" | "Imperial" => 500003,
        "Gallente" | "Federation" => 500004,
        "Ammatar" => 500007,
        "Khanid" => 500008,
        "Syndicate" => 500009,
        "Guristas" | "Dread" => 500010,
        "Arch" | "Domination" => 500011,
        "Blood" | "Dark" => 500012,
        "ORE" => 500014,
        "Thukker" => 500015,
        "Sisters" => 500016,
        "Legion" => 500018,
        "Sanshas" | "True" => 500019,
        "Shadow" => 500020,
        _ => return None,
    })
}

pub async fn lp_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let me = ctx.session.char_id.unwrap_or(0) as i32;
    Some(match call.method.as_str() {
        // lpstore.py GetMyLPs: the LP shown in the store's header.
        "GetLPForCharacterCorp" => Outcome::Value(Value::Int(lp(ctx, me, call.arg_id(0) as i32).await)),
        // journal.py GetMyLoyaltyPoints: row[0] corporation, row[1] points.
        "GetLPsForCharacter" => Outcome::Value(Value::List(ctx.store.read(|db| {
            // By corporation: the order of the table's key.
            db.table("chrLoyaltyPoints")
                .find(&["characterID"], &[V::Int(me.into())])
                .filter_map(|r| Some((r.int("corporationID")?, r.int("loyaltyPoints")?)))
                .filter(|(_, points)| *points > 0)
                .map(|(corp, points)| Value::Tuple(vec![Value::Int(corp), Value::Int(points)]))
                .collect()
        }))),
        "GetAvailableOffersFromCorp" => Outcome::Value(Value::List(
            offers(call.arg_id(0) as i32, None).iter().map(Offer::to_value).collect(),
        )),
        "TakeOffer" => match take_offer(ctx, call.arg_id(0) as i32, call.arg_id(1) as i32).await {
            Ok(()) => Outcome::Value(Value::None),
            Err(why) => why,
        },
        _ => return None,
    })
}

/// The pilot's loyalty points with a corporation.
pub async fn lp(ctx: &Ctx<'_>, char_id: i32, corp_id: i32) -> i64 {
    ctx.store.read(|db| points(db, char_id, corp_id))
}

fn points(db: &Db, char_id: i32, corp_id: i32) -> i64 {
    db.table("chrLoyaltyPoints").get(&[V::Int(char_id.into()), V::Int(corp_id.into())]).and_then(|r| r.int("loyaltyPoints")).unwrap_or(0)
}

/// Add (or, negative, take) loyalty points, inside the caller's transaction.
/// Refuses to go below zero; returns the new total.
pub fn add_lp(tx: &mut Tx<'_>, char_id: i32, corp_id: i32, delta: i64) -> Result<i64, String> {
    // The points column is an integer, as the SQL's parameter was.
    let delta = i64::from(delta as i32);
    let key = [V::Int(char_id.into()), V::Int(corp_id.into())];
    let total = match tx.table("chrLoyaltyPoints").get(&key).map(|r| (r.id, r.int("loyaltyPoints").unwrap_or(0))) {
        None => {
            let total = delta.max(0);
            tx.insert("chrLoyaltyPoints", &[("characterID", key[0].clone()), ("corporationID", key[1].clone()), ("loyaltyPoints", V::Int(total))])
                .map_err(|e| e.to_string())?;
            total
        }
        Some((_, have)) if have + delta < 0 => return Err("not enough loyalty points".into()),
        Some((row, have)) => {
            tx.update("chrLoyaltyPoints", row, &[("loyaltyPoints", V::Int(have + delta))]).map_err(|e| e.to_string())?;
            have + delta
        }
    };
    Ok(total)
}

struct Offer {
    id: i32,
    type_id: i32,
    qty: i32,
    lp_cost: i32,
    isk_cost: i64,
    /// What the pilot hands over besides ISK.
    req_items: Vec<(i32, i32)>,
    name: String,
    blueprint: bool,
}

impl Offer {
    /// What lpstore.py reads: offer.offerID, typeID, qty, lpCost, iskCost,
    /// reqItems; a KeyVal, since the store calls offer.Set() to sort it.
    fn to_value(&self) -> Value {
        key_val(vec![
            ("offerID", Value::Int(self.id as i64)),
            ("typeID", Value::Int(self.type_id as i64)),
            ("qty", Value::Int(self.qty as i64)),
            ("lpCost", Value::Int(self.lp_cost as i64)),
            ("iskCost", Value::Int(self.isk_cost)),
            (
                "reqItems",
                Value::List(
                    self.req_items
                        .iter()
                        .map(|(t, q)| Value::Tuple(vec![Value::Int(*t as i64), Value::Int(*q as i64)]))
                        .collect(),
                ),
            ),
        ])
    }
}

/// The offers a corporation sells (or, with `only`, that one of them), by LP
/// cost. Only an NPC corporation sells any, and only offers whose reward is a
/// type of a known group.
fn offers(corp_id: i32, only: Option<i32>) -> Vec<Offer> {
    let Some(corporation) = crate::sde::table("crpNPCCorporations").get("corporationID", corp_id.into()) else {
        return Vec::new();
    };
    let faction = corporation.int("factionID");
    let mut sold: Vec<(crate::sde::Row, crate::sde::Row, Option<i64>)> = crate::sde::table("agtOfferDetails")
        .rows()
        .filter_map(|o| {
            let id = o.int("contentID")?;
            if only.is_some_and(|only| i64::from(only) != id) {
                return None;
            }
            let reward = crate::sde::table("invTypes").get("typeID", o.int("agentTypeID1")?)?;
            let group = crate::sde::table("invGroups").get("groupID", reward.int("groupID")?)?;
            let sold_by = offer_faction(reward.text("typeName"));
            (sold_by.is_none() || sold_by == faction).then_some((o, reward, group.int("categoryID")))
        })
        .collect();
    // Cheapest first (an offer without a cost last, as the database sorted
    // them), then by offer.
    sold.sort_by_key(|(o, _, _)| {
        let cost = o.int("loyaltyPointCost");
        (cost.is_none(), cost, o.int("contentID"))
    });
    sold.iter()
        .map(|(o, reward, category)| {
            let int = |c: &str| o.int(c).unwrap_or(0) as i32;
            let mut isk_cost = 0i64;
            let mut req_items = Vec::new();
            for n in 1..=5 {
                let (t, q) = (int(&format!("charTypeID{n}")), int(&format!("charQuantity{n}")));
                match (t, q) {
                    (0, _) | (_, 0) => {}
                    (TYPE_CREDITS, q) => isk_cost += q as i64,
                    (t, q) => req_items.push((t, q)),
                }
            }
            Offer {
                id: int("contentID"),
                type_id: int("agentTypeID1"),
                qty: int("agentQuantity1").max(1),
                lp_cost: int("loyaltyPointCost"),
                isk_cost,
                req_items,
                name: reward.text("typeName").unwrap_or_default().to_string(),
                blueprint: category.unwrap_or(0) as i32 == CATEGORY_BLUEPRINT,
            }
        })
        .collect()
}

fn refuse(text: impl Into<String>) -> Outcome {
    Outcome::custom(text)
}

/// A refused offer, which undoes the store write it ends.
struct Refused(Outcome);

impl From<StoreError> for Refused {
    fn from(e: StoreError) -> Refused {
        Refused(refuse(format!("The LP store could not finish: {e}")))
    }
}

async fn take_offer(ctx: &Ctx<'_>, corp_id: i32, offer_id: i32) -> Result<(), Outcome> {
    let me = ctx.session.char_id.unwrap_or(0);
    let Some(station) = ctx.session.station_id else {
        return Err(refuse("You must be docked to use the LP store."));
    };
    let Some(offer) = offers(corp_id, Some(offer_id)).into_iter().next() else {
        warn!("TakeOffer: corporation {corp_id} does not sell offer {offer_id}");
        return Err(refuse("That corporation does not make that offer."));
    };

    let failed = |what: &str, e: StoreError| Refused(refuse(format!("The LP store could not {what}: {e}")));
    let (events, balance) = ctx.store.write(|tx| -> Result<_, Refused> {
        let mut txn = ItemTxn::new(tx);

        // Loyalty points.
        let have = points(txn.db(), me as i32, corp_id);
        if have < offer.lp_cost as i64 {
            return Err(Refused(refuse(format!("You need {} LP with this corporation and have {have}.", offer.lp_cost))));
        }
        add_lp(txn.tx(), me as i32, corp_id, -(offer.lp_cost as i64)).map_err(|e| Refused(refuse(e)))?;

        // ISK.
        let mut balance = None;
        if offer.isk_cost > 0 {
            let reason = format!("LP store: {}", offer.name);
            let entry = Entry { ref_type: REF_AGENT_MISCELLANEOUS, payer: me, payee: corp_id as i64, arg_id1: "", reason: &reason };
            balance = Some(match wallet::post(txn.tx(), me, -(offer.isk_cost as f64), &entry) {
                Ok(b) => b,
                Err(Transfer::NotEnough { need, have }) => return Err(Refused(Outcome::not_enough_money(need, have))),
                Err(Transfer::Db(e)) => return Err(failed("take the ISK", e)),
                Err(_) => return Err(Refused(refuse("Your wallet could not be found."))),
            });
        }

        // Required items, from loose stacks in the station hangar (as the store
        // counts them: lpstore.py HaveItem), the biggest first.
        for (type_id, qty) in &offer.req_items {
            let mut stacks: Vec<(i64, i64)> = txn
                .db()
                .table("entity")
                .find(&["ownerID", "locationID", "flag"], &[V::Int(me), V::Int(station), V::Int(FLAG_HANGAR)])
                .filter(|r| r.int("typeID") == Some(i64::from(*type_id)) && r.int("singleton") == Some(0))
                .filter_map(|r| Some((r.int("itemID")?, r.int("quantity")?)))
                .collect();
            stacks.sort_by_key(|(_, q)| std::cmp::Reverse(*q));
            if stacks.iter().map(|(_, q)| q).sum::<i64>() < *qty as i64 {
                return Err(Refused(refuse("The items this offer asks for must be in your hangar in this station.")));
            }
            let mut left = *qty as i64;
            for (id, q) in stacks {
                if left == 0 {
                    break;
                }
                let take = q.min(left);
                if take == q {
                    txn.destroy(id).map_err(|e| failed("take the items", e))?;
                } else {
                    txn.set_quantity(id, q - take).map_err(|e| failed("take the items", e))?;
                }
                left -= take;
            }
        }

        // The reward: a blueprint as one copy with `qty` runs, anything else as a stack.
        if offer.blueprint {
            let copy = NewItem {
                type_id: offer.type_id as i64,
                owner_id: me,
                location_id: station,
                flag: FLAG_HANGAR,
                quantity: 1,
                singleton: true,
            };
            let id = txn.create(copy).map_err(|e| failed("make the blueprint copy", e))?;
            txn.tx()
                .insert(
                    "invBlueprints",
                    &[
                        ("itemID", V::Int(id)),
                        ("copy", V::Int(1)),
                        ("materialLevel", V::Int(0)),
                        ("productivityLevel", V::Int(0)),
                        ("licensedProductionRunsRemaining", V::Int(offer.qty.into())),
                    ],
                )
                .map_err(|e| failed("make the blueprint copy", e))?;
        } else {
            let stack = NewItem {
                type_id: offer.type_id as i64,
                owner_id: me,
                location_id: station,
                flag: FLAG_HANGAR,
                quantity: offer.qty as i64,
                singleton: false,
            };
            txn.stack_onto(stack).map_err(|e| failed("deliver the reward", e))?;
        }
        Ok((txn.events(), balance))
    })
    .map_err(|Refused(why)| why)?;
    crate::items::notify(ctx.world, ctx.client_id, events);
    if let Some(balance) = balance {
        let w = ctx.world.lock();
        let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(me), Value::Double(balance)]);
        w.notify(&w.clients_of(me), "OnAccountChange", cash);
    }
    info!(
        "{me} took LP offer {offer_id} ({} x{}) from {corp_id} for {} LP and {} ISK",
        offer.name, offer.qty, offer.lp_cost, offer.isk_cost
    );
    Ok(())
}
