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
//! corporation. See [`OFFER_FACTION`].

use eve_marshal::Value;
use tracing::{info, warn};

use crate::items::{ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::key_val;
use crate::svc::wallet::{self, Entry, Transfer};

/// `invTypes` 29, Credits: an offer asking for these asks for ISK.
const TYPE_CREDITS: i32 = 29;
const CATEGORY_BLUEPRINT: i32 = 9;
const FLAG_HANGAR: i64 = 4;
/// "Agent Miscellaneous": Trinity has no journal type of its own for the store.
const REF_AGENT_MISCELLANEOUS: i32 = 25;

/// Which faction sells an offer, from the first word of its reward's name; NULL
/// for offers every corporation sells. Joined as `f` on `o."contentID"`.
const OFFER_FACTION: &str = r#"
    SELECT o."contentID",
           CASE split_part(t."typeName", ' ', 1)
                WHEN 'Caldari' THEN 500001
                WHEN 'Republic' THEN 500002 WHEN 'Minmatar' THEN 500002
                WHEN 'Amarr' THEN 500003 WHEN 'Imperial' THEN 500003
                WHEN 'Gallente' THEN 500004 WHEN 'Federation' THEN 500004
                WHEN 'Ammatar' THEN 500007
                WHEN 'Khanid' THEN 500008
                WHEN 'Syndicate' THEN 500009
                WHEN 'Guristas' THEN 500010 WHEN 'Dread' THEN 500010
                WHEN 'Arch' THEN 500011 WHEN 'Domination' THEN 500011
                WHEN 'Blood' THEN 500012 WHEN 'Dark' THEN 500012
                WHEN 'ORE' THEN 500014
                WHEN 'Thukker' THEN 500015
                WHEN 'Sisters' THEN 500016
                WHEN 'Legion' THEN 500018
                WHEN 'Sanshas' THEN 500019 WHEN 'True' THEN 500019
                WHEN 'Shadow' THEN 500020
           END AS "factionID"
      FROM "agtOfferDetails" o JOIN "invTypes" t ON t."typeID" = o."agentTypeID1""#;

pub async fn lp_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let me = ctx.session.char_id.unwrap_or(0) as i32;
    Some(match call.method.as_str() {
        // lpstore.py GetMyLPs: the LP shown in the store's header.
        "GetLPForCharacterCorp" => Outcome::Value(Value::Int(lp(ctx, me, call.arg_id(0) as i32).await)),
        // journal.py GetMyLoyaltyPoints: row[0] corporation, row[1] points.
        "GetLPsForCharacter" => {
            let sql = r#"SELECT "corporationID","loyaltyPoints" FROM "chrLoyaltyPoints"
                          WHERE "characterID" = $1 AND "loyaltyPoints" > 0 ORDER BY "corporationID""#;
            let rows = ctx.query(sql, &[&me]).await;
            Outcome::Value(Value::List(
                rows.iter()
                    .map(|r| Value::Tuple(vec![Value::Int(r.get::<_, i32>(0) as i64), Value::Int(r.get::<_, i32>(1) as i64)]))
                    .collect(),
            ))
        }
        "GetAvailableOffersFromCorp" => Outcome::Value(Value::List(
            offers(ctx, call.arg_id(0) as i32, None).await.iter().map(Offer::to_value).collect(),
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
    let sql = r#"SELECT "loyaltyPoints" FROM "chrLoyaltyPoints" WHERE "characterID" = $1 AND "corporationID" = $2"#;
    ctx.query_one(sql, &[&char_id, &corp_id]).await.map_or(0, |r| r.get::<_, i32>(0) as i64)
}

/// Add (or, negative, take) loyalty points, inside the caller's transaction.
/// Refuses to go below zero; returns the new total.
pub async fn add_lp(tx: &tokio_postgres::Transaction<'_>, char_id: i32, corp_id: i32, delta: i64) -> Result<i64, String> {
    let sql = r#"INSERT INTO "chrLoyaltyPoints" ("characterID","corporationID","loyaltyPoints") VALUES ($1,$2,GREATEST($3,0))
                 ON CONFLICT ("characterID","corporationID")
                 DO UPDATE SET "loyaltyPoints" = "chrLoyaltyPoints"."loyaltyPoints" + $3
                 WHERE "chrLoyaltyPoints"."loyaltyPoints" + $3 >= 0
                 RETURNING "loyaltyPoints""#;
    match tx.query_opt(sql, &[&char_id, &corp_id, &(delta as i32)]).await {
        Ok(Some(r)) => Ok(r.get::<_, i32>(0) as i64),
        Ok(None) => Err("not enough loyalty points".into()),
        Err(e) => Err(e.to_string()),
    }
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

/// The offers a corporation sells (or, with `only`, that one of them).
async fn offers(ctx: &Ctx<'_>, corp_id: i32, only: Option<i32>) -> Vec<Offer> {
    let sql = format!(
        r#"WITH f AS ({OFFER_FACTION})
           SELECT o.*, t."typeName", g."categoryID"
             FROM "agtOfferDetails" o
             JOIN f ON f."contentID" = o."contentID"
             JOIN "invTypes" t ON t."typeID" = o."agentTypeID1"
             JOIN "invGroups" g ON g."groupID" = t."groupID"
             JOIN "crpNPCCorporations" c ON c."corporationID" = $1
            WHERE (f."factionID" IS NULL OR f."factionID" = c."factionID")
              AND ($2::integer IS NULL OR o."contentID" = $2)
            ORDER BY o."loyaltyPointCost", o."contentID""#
    );
    let rows = ctx.query(&sql, &[&corp_id, &only]).await;
    rows.iter()
        .map(|r| {
            let int = |c: &str| r.try_get::<_, Option<i32>>(c).ok().flatten().unwrap_or(0);
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
                name: r.try_get::<_, String>("typeName").unwrap_or_default(),
                blueprint: int("categoryID") == CATEGORY_BLUEPRINT,
            }
        })
        .collect()
}

fn refuse(text: impl Into<String>) -> Outcome {
    Outcome::custom(text)
}

async fn take_offer(ctx: &Ctx<'_>, corp_id: i32, offer_id: i32) -> Result<(), Outcome> {
    let me = ctx.session.char_id.unwrap_or(0);
    let Some(station) = ctx.session.station_id else {
        return Err(refuse("You must be docked to use the LP store."));
    };
    let Some(offer) = offers(ctx, corp_id, Some(offer_id)).await.into_iter().next() else {
        warn!("TakeOffer: corporation {corp_id} does not sell offer {offer_id}");
        return Err(refuse("That corporation does not make that offer."));
    };

    let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| refuse(e.to_string()))?;
    let mut txn = ItemTxn::begin(&mut client).await.map_err(|e| refuse(e.to_string()))?;
    let failed = |what: &str, e: tokio_postgres::Error| refuse(format!("The LP store could not {what}: {e}"));

    // Loyalty points.
    let have = lp(ctx, me as i32, corp_id).await;
    if have < offer.lp_cost as i64 {
        return Err(refuse(format!("You need {} LP with this corporation and have {have}.", offer.lp_cost)));
    }
    add_lp(txn.tx(), me as i32, corp_id, -(offer.lp_cost as i64)).await.map_err(refuse)?;

    // ISK.
    let mut balance = None;
    if offer.isk_cost > 0 {
        let reason = format!("LP store: {}", offer.name);
        let entry = Entry { ref_type: REF_AGENT_MISCELLANEOUS, payer: me, payee: corp_id as i64, arg_id1: "", reason: &reason };
        balance = Some(match wallet::post(txn.tx(), me, -(offer.isk_cost as f64), &entry).await {
            Ok(b) => b,
            Err(Transfer::NotEnough { need, have }) => return Err(Outcome::not_enough_money(need, have)),
            Err(Transfer::Db(e)) => return Err(failed("take the ISK", e)),
            Err(_) => return Err(refuse("Your wallet could not be found.")),
        });
    }

    // Required items, from loose stacks in the station hangar (as the store
    // counts them: lpstore.py HaveItem).
    for (type_id, qty) in &offer.req_items {
        let sql = r#"SELECT "itemID","quantity" FROM entity
                      WHERE "ownerID" = $1 AND "locationID" = $2 AND "flag" = $3 AND "typeID" = $4 AND "singleton" = 0
                      ORDER BY "quantity" DESC FOR UPDATE"#;
        let rows = txn
            .tx()
            .query(sql, &[&(me as i32), &(station as i32), &(FLAG_HANGAR as i32), type_id])
            .await
            .map_err(|e| failed("look in your hangar", e))?;
        let stacks: Vec<(i64, i64)> = rows.iter().map(|r| (r.get::<_, i32>(0) as i64, r.get::<_, i32>(1) as i64)).collect();
        if stacks.iter().map(|(_, q)| q).sum::<i64>() < *qty as i64 {
            return Err(refuse("The items this offer asks for must be in your hangar in this station."));
        }
        let mut left = *qty as i64;
        for (id, q) in stacks {
            if left == 0 {
                break;
            }
            let take = q.min(left);
            if take == q {
                txn.destroy(id).await.map_err(|e| failed("take the items", e))?;
            } else {
                txn.set_quantity(id, q - take).await.map_err(|e| failed("take the items", e))?;
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
        let id = txn.create(copy).await.map_err(|e| failed("make the blueprint copy", e))?;
        let sql = r#"INSERT INTO "invBlueprints" ("itemID","copy","materialLevel","productivityLevel","licensedProductionRunsRemaining")
                     VALUES ($1, 1, 0, 0, $2)"#;
        txn.tx().execute(sql, &[&(id as i32), &offer.qty]).await.map_err(|e| failed("make the blueprint copy", e))?;
    } else {
        let stack = NewItem {
            type_id: offer.type_id as i64,
            owner_id: me,
            location_id: station,
            flag: FLAG_HANGAR,
            quantity: offer.qty as i64,
            singleton: false,
        };
        txn.stack_onto(stack).await.map_err(|e| failed("deliver the reward", e))?;
    }

    let events = txn.commit().await.map_err(|e| failed("finish", e))?;
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
