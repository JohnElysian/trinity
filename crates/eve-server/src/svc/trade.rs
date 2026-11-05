//! Buying and selling on the market: marketProxy PlaceCharOrder,
//! CancelCharOrder and ModifyCharOrder (marketsvc.py:236-250).
//!
//! An order first trades against what is already on the market at its price or
//! better; with a duration of 0 ("immediate") it must fill completely or nothing
//! happens, otherwise what is left is listed. Buyers pay the seller's price.
//! NPC orders (owned by an NPC corporation) never run out of money: their buy
//! orders hold no escrow, and what they buy leaves the game.
//!
//! A corporation order (useCorp) is placed by a member with the Trader or
//! Accountant role (or a director) on the corporation's behalf: the
//! corporation's wallet pays and is paid, what it sells comes out of its
//! office's divisions (as the member's roles allow), and what it buys is
//! delivered to its deliveries hangar at the station.

use std::collections::HashMap;

use eve_marshal::Value;
use tokio_postgres::Row;
use tracing::{error, info};

use crate::items::{ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::inventory::FLAG_HANGAR;
use crate::svc::market::{order_line, ORDER_HEADER, ORDER_SELECT};
use crate::svc::rows::row;
use crate::svc::wallet::{post_account, Entry, Transfer};

const RANGE_STATION: i64 = -1;
const ORDER_OPEN: i32 = 0;
/// const.mktOrderExpired, also used for an order that has run out.
const ORDER_DONE: i32 = 2;
const ORDER_CANCELLED: i32 = 3;
/// wallet.py:1045: 0 is money in (a sale), 1 money out (a purchase).
const TRANSACTION_SALE: i32 = 0;
const TRANSACTION_PURCHASE: i32 = 1;

const REF_MARKET_TRANSACTION: i32 = 2;
const REF_MARKET_ESCROW: i32 = 42;
const REF_BROKER_FEE: i32 = 46;
const REF_TRANSACTION_TAX: i32 = 54;
/// const.marketCommissionPercentage, const.mktTransactionTax, const.mktMinimumFee.
const BROKER_FEE: f64 = 0.01;
const TRANSACTION_TAX: f64 = 0.01;
const MINIMUM_FEE: f64 = 100.0;
const SKILL_BROKER_RELATIONS: i32 = 3446;
const SKILL_ACCOUNTING: i32 = 16622;
const FIRST_PLAYER_ID: i64 = 100_000_000;
const CORP_ROLE_TRADER: i64 = 18_014_398_509_481_984;
/// Whose wallet an order uses: its pilot's, or for a corporation order the
/// pilot's corporation's.
const WALLET: &str = r#"COALESCE(NULLIF("walletOwnerID",0),"charID")"#;

pub async fn ensure(db:&tokio_postgres::Client)->Result<(),tokio_postgres::Error> {
    db.batch_execute(r#"ALTER TABLE "character_" ADD COLUMN IF NOT EXISTS "corpAccountKey" integer NOT NULL DEFAULT 1000;
        CREATE TABLE IF NOT EXISTS corporation_wallets (
            "corporationID" integer NOT NULL, "accountKey" integer NOT NULL CHECK("accountKey" BETWEEN 1001 AND 1006),
            balance double precision NOT NULL DEFAULT 0 CHECK(balance>=0), PRIMARY KEY("corporationID","accountKey"));"#).await?;
    db.batch_execute(r#"ALTER TABLE market_orders ADD COLUMN IF NOT EXISTS "walletOwnerID" integer NOT NULL DEFAULT 0;
        UPDATE market_orders SET "walletOwnerID"=CASE WHEN "isCorp"=1 THEN COALESCE((SELECT c."corporationID" FROM "character_" c WHERE c."characterID"=market_orders."charID"),"charID") ELSE "charID" END WHERE "walletOwnerID"=0;"#).await?;
    db.batch_execute(include_str!("market_seed.sql")).await
}

/// Why an order was not placed.
#[derive(Debug)]
enum Refusal {
    Text(String),
    Money { need: f64, have: f64 },
    Db(tokio_postgres::Error),
}

impl From<tokio_postgres::Error> for Refusal {
    fn from(e: tokio_postgres::Error) -> Self {
        Refusal::Db(e)
    }
}

impl From<Transfer> for Refusal {
    fn from(t: Transfer) -> Self {
        match t {
            Transfer::NotEnough { need, have } => Refusal::Money { need, have },
            Transfer::NoSuchOwner => Refusal::Text("That wallet does not exist.".into()),
            Transfer::Db(e) => Refusal::Db(e),
        }
    }
}

fn refuse<T>(why: impl Into<String>) -> Result<T, Refusal> {
    Err(Refusal::Text(why.into()))
}

/// What the client is told once a trade commits.
#[derive(Default)]
struct Effects {
    item_events: Vec<Value>,
    /// Owners whose wallets changed, with their new balances.
    balances: HashMap<(i64,i32), f64>,
    /// Orders that changed, with the reason the client is given.
    orders: Vec<(i64, &'static str)>,
    refresh: Vec<(i64, i64, bool)>,
}

pub async fn market_write(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let Some(me) = ctx.session.char_id else { return Some(Outcome::custom("Select a character first.")) };
    let arity=match call.method.as_str() {"PlaceCharOrder"=>11,"CancelCharOrder"=>2,"ModifyCharOrder"=>8,_=>return None};
    if call.args.len()!=arity || !call.kwargs.is_empty() {return Some(Outcome::custom("Invalid market request arguments."));}
    let result = match call.method.as_str() {
        // PlaceCharOrder(stationID, typeID, price, quantity, bid, range, itemID,
        //                minVolume, duration, useCorp, located)
        "PlaceCharOrder" => {
            let order = NewOrder {
                station: call.arg_id(0),
                type_id: call.arg_id(1),
                price: (call.args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0)*100.0).round()/100.0,
                quantity: call.arg_id(3),
                bid: call.arg_id(4) != 0,
                range: call.arg_i64(5).unwrap_or(RANGE_STATION),
                item_id: call.arg_i64(6),
                min_volume: call.arg_i64(7).unwrap_or(1),
                duration: call.arg_id(8),
                corp: call.arg_id(9) != 0,
                existing: None,
                account: if call.arg_id(9)!=0 {ctx.session.corp_account_key.unwrap_or(1000)} else {1000},
            };
            let s = ctx.session;
            if let Err(why) = access(ctx, order.station, if order.bid {16594} else {16598}, order.bid && order.duration==0,
                (order.bid && s.station_id!=Some(order.station)).then_some(order.range)).await {
                return Some(Outcome::custom(why));
            }
            let roles = (s.corp_id, s.roles_at_all | if Some(order.station)==s.hq_id {s.roles_at_hq}
                else if Some(order.station)==s.base_id {s.roles_at_base} else {s.roles_at_other});
            transact(ctx, |txn, fx| Box::pin(place(txn, fx, me, order, roles))).await
        }
        // CancelCharOrder(orderID, regionID)
        "CancelCharOrder" => {
            let order_id = call.arg_id(0);
            if let Err(why)=order_access(ctx,order_id).await {return Some(Outcome::custom(why));}
            let roles=(ctx.session.corp_id,ctx.session.corp_role());
            transact(ctx, |txn, fx| Box::pin(cancel(txn, fx, me, order_id,roles))).await
        }
        // ModifyCharOrder(orderID, newPrice, bid, stationID, solarSystemID, price, volRemaining, issued)
        "ModifyCharOrder" => {
            let order_id = call.arg_id(0);
            if let Err(why)=order_access(ctx,order_id).await {return Some(Outcome::custom(why));}
            if let Ok(Some(r))=ctx.db.query_opt(r#"SELECT issued FROM market_orders WHERE "orderID"=$1"#,&[&(order_id as i32)]).await {
                if crate::bulkdata::filetime_now()-r.get::<_,i64>(0)<3_000_000_000 {return Some(Outcome::custom("Please wait five minutes after placing or modifying an order before modifying it again."));}
            }
            let price = call.args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let roles=(ctx.session.corp_id,ctx.session.corp_role());
            transact(ctx, |txn, fx| Box::pin(modify(txn, fx, me, order_id, price,roles))).await
        }
        _ => return None,
    };
    Some(result)
}

type Work<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Refusal>> + Send + 'a>>;

fn skill_distance(level: i64) -> i64 { [-1,0,5,10,20,50][level.clamp(0,5) as usize] }

async fn access(ctx: &Ctx<'_>, station: i64, skill: i32, immediate_buy: bool, visibility: Option<i64>) -> Result<(),String> {
    let station_id=i32::try_from(station).map_err(|_|"Invalid station.")?;
    let r=ctx.db.query_opt(r#"SELECT "solarSystemID","regionID" FROM "staStations" WHERE "stationID"=$1"#,&[&station_id])
        .await.map_err(|_|"The station could not be checked.")?.ok_or("Unknown station.")?;
    if ctx.session.region_id!=Some(r.get::<_,i32>(1) as i64) {return Err("You can only trade in your current region.".into());}
    if immediate_buy {return Ok(());}
    let skills=crate::svc::skills::levels(ctx.db,ctx.session.char_id.unwrap_or(0),&[skill,3447]).await.map_err(|_|"Trade skills could not be checked.")?;
    let distance=if ctx.session.station_id==Some(station) {-1} else {
        crate::gates::jumps(ctx.db,ctx.session.solar_system_id2.unwrap_or(0),r.get::<_,i32>(0) as i64,50).await.unwrap_or(32767)
    };
    if distance>skill_distance(skills[&skill]) {return Err("Your trade skills do not allow trading at that distance.".into());}
    if visibility.is_some_and(|range|range>skill_distance(skills[&3447])) {return Err("Your Visibility skill does not allow that remote order range.".into());}
    Ok(())
}

async fn order_access(ctx: &Ctx<'_>, id: i64) -> Result<(),String> {
    let id=i32::try_from(id).map_err(|_|"Invalid order.")?;
    let r=ctx.db.query_opt(r#"SELECT "stationID" FROM market_orders WHERE "orderID"=$1"#,&[&id]).await
        .map_err(|_|"The order could not be checked.")?.ok_or("Unknown order.")?;
    access(ctx,r.get::<_,i32>(0) as i64,16595,false,None).await
}

async fn transact<F>(ctx: &Ctx<'_>, work: F) -> Outcome
where
    F: for<'a> FnOnce(&'a mut ItemTxn<'_>, &'a mut Effects) -> Work<'a>,
{
    let mut fx = Effects::default();
    let result: Result<(), Refusal> = async {
        let mut client = crate::db::connect(ctx.db_url).await?;
        let mut txn = ItemTxn::begin(&mut client).await?;
        work(&mut txn, &mut fx).await?;
        fx.item_events = txn.commit().await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => {
            tell(ctx, fx).await;
            Outcome::Value(Value::None)
        }
        Err(Refusal::Text(why)) => {
            info!("market order refused: {why}");
            Outcome::custom(why)
        }
        Err(Refusal::Money { need, have }) => Outcome::not_enough_money(need, have),
        Err(Refusal::Db(e)) => {
            error!("market order: {}", crate::logging::chain(&e));
            Outcome::custom("The market could not process that order.")
        }
    }
}

/// Send item, wallet and order changes to whoever they concern.
async fn tell(ctx: &Ctx<'_>, fx: Effects) {
    let ids: Vec<i32> = fx.orders.iter().map(|(id, _)| *id as i32)
        .chain(fx.refresh.iter().map(|(_,id,_)|*id as i32)).collect();
    let sql = format!(r#"{ORDER_SELECT} WHERE o."orderID" = ANY($1)"#);
    let rows: HashMap<i64, Row> =
        ctx.query(&sql, &[&ids]).await.into_iter().map(|r| (r.get::<_, i32>(0) as i64, r)).collect();

    let owners: Vec<i32> = fx.balances.keys().map(|(id,_)| *id as i32).collect();
    let corporations: std::collections::HashSet<i64> = ctx.query(r#"SELECT "corporationID" FROM corporation WHERE "corporationID"=ANY($1)"#,
        &[&owners]).await.iter().map(|r|r.get::<_,i32>(0) as i64).collect();
    let order_owners: HashMap<i64,i64> = ctx.query(r#"SELECT "orderID","walletOwnerID" FROM market_orders WHERE "orderID"=ANY($1)"#,
        &[&ids]).await.iter().map(|r|(r.get::<_,i32>(0) as i64,r.get::<_,i32>(1) as i64)).collect();
    let world = ctx.world.lock();
    for event in fx.item_events {
        let owner = event.seq().and_then(|args|args.first()).and_then(|item| {
            let Value::Instance {state,..}=item else {return None};
            let Value::Dict(fields)=state.as_ref() else {return None};
            fields.iter().find(|(k,_)|*k==Value::bytes("line"))?.1.seq()?.get(2)?.as_i64()
        });
        if let Some(owner)=owner {
            let mut clients=world.clients_of(owner);
            clients.extend(world.corp_members(owner));
            clients.sort_unstable(); clients.dedup();
            world.notify(&clients,"OnItemChange",event);
        }
    }
    for ((owner,key), balance) in fx.balances {
        let event = Value::Tuple(vec![Value::bytes(if key==1000 {"cash".into()} else {format!("cash{}",key-999)}), Value::Int(owner), Value::Double(balance)]);
        let clients=if corporations.contains(&owner) {world.corp_members(owner)} else {world.clients_of(owner)};
        world.notify(&clients, "OnAccountChange", event);
    }
    for (order_id, reason) in fx.orders {
        let Some(r) = rows.get(&order_id) else { continue };
        let owner = r.get::<_, i32>(2) as i64;
        let corp = r.get::<_, i16>(16) != 0;
        let event = Value::Tuple(vec![row(ORDER_HEADER, order_line(r)), Value::bytes(reason), Value::Bool(corp)]);
        let clients=if corp {world.corp_members(order_owners.get(&order_id).copied().unwrap_or(owner))} else {world.clients_of(owner)};
        world.notify(&clients, "OnOwnOrderChanged", event);
    }
    for (actor,id,corp) in fx.refresh {
        if let Some(r)=rows.get(&id) {
            world.notify(&world.clients_of(actor),"OnOwnOrderChanged",Value::Tuple(vec![
                row(ORDER_HEADER,order_line(r)),Value::bytes("Modify"),Value::Bool(corp)]));
        }
    }
}

struct NewOrder {
    station: i64,
    type_id: i64,
    price: f64,
    quantity: i64,
    bid: bool,
    range: i64,
    item_id: Option<i64>,
    min_volume: i64,
    duration: i64,
    corp: bool,
    existing: Option<i64>,
    account: i32,
}

/// An order on the other side of a trade.
struct Match {
    id: i64,
    /// Whose wallet and goods: a pilot's, or a corporation's.
    owner: i64,
    issuer: i64,
    account: i32,
    station: i64,
    price: f64,
    remaining: i64,
    escrow: f64,
    corp: bool,
    minimum: i64,
    range: i64,
    system: i64,
}

/// Where goods bought for an owner go: a corporation's deliveries, or a
/// pilot's hangar.
fn delivery_flag(corp: bool) -> i64 {
    if corp { crate::svc::inventory::FLAG_CORP_MARKET as i64 } else { FLAG_HANGAR as i64 }
}

struct Where {
    solar_system: i64,
    region: i64,
}

async fn place(txn: &mut ItemTxn<'_>, fx: &mut Effects, me: i64, order: NewOrder, ctx_roles: (Option<i64>, i64)) -> Result<(), Refusal> {
    let who = if order.corp {
        let (corp, roles) = ctx_roles;
        let Some(corp) = corp else { return refuse("You are not in a corporation.") };
        if roles & (crate::session::CORP_ROLE_DIRECTOR | crate::session::CORP_ROLE_ACCOUNTANT | CORP_ROLE_TRADER) == 0 {
            return refuse("Placing orders for your corporation needs the Trader or Accountant role.");
        }
        if !crate::svc::wallet::can_take(roles,order.account) {return refuse("You may not spend from that corporation wallet division.");}
        corp
    } else {
        me
    };
    if order.quantity <= 0 || order.quantity>i32::MAX as i64 || !order.price.is_finite() || order.price < 0.01 || order.price > 9_223_372_036_854.0 || !(order.price*order.quantity as f64).is_finite() {
        return refuse("Enter a quantity and a price.");
    }
    if order.min_volume<1 || order.min_volume>order.quantity || ![0,1,3,7,14,30,90].contains(&order.duration) || !(-1..=32767).contains(&order.range) {return refuse("Invalid minimum volume, duration or order range.")}
    if i32::try_from(order.station).is_err() || i32::try_from(order.type_id).is_err() {
        return refuse("Invalid station or item type.");
    }
    if order.item_id.is_some_and(|id|id<=0 || i32::try_from(id).is_err()) {return refuse("Invalid item ID.");}
    if txn.tx().query_opt(r#"SELECT 1 FROM "invTypes" WHERE "typeID"=$1 AND "marketGroupID" IS NOT NULL"#,
        &[&(order.type_id as i32)]).await?.is_none() {
        return refuse("This item cannot be traded on the market.");
    }
    // Serialize matching and repricing with cancellation/expiry. This prevents
    // two orders missing each other, and keeps wallet locks in a stable order.
    txn.tx().execute("SELECT pg_advisory_xact_lock(45017, 2)", &[]).await?;
    let r = txn
        .tx()
        .query_opt(
            r#"SELECT "solarSystemID","regionID" FROM "staStations" WHERE "stationID" = $1"#,
            &[&(order.station as i32)],
        )
        .await?;
    let Some(r) = r else { return refuse("That station has no market.") };
    let at = Where { solar_system: r.get::<_, i32>(0) as i64, region: r.get::<_, i32>(1) as i64 };
    if order.bid {
        buy(txn, fx, me, who, &order, &at).await
    } else {
        sell(txn, fx, me, who, &order, &at, ctx_roles.1).await
    }
}

async fn buy(txn: &mut ItemTxn<'_>, fx: &mut Effects, me: i64, who: i64, order: &NewOrder, at: &Where) -> Result<(), Refusal> {
    // Sell orders inside the buy order's range, cheapest first.
    let sql = format!(
        r#"SELECT "orderID",{WALLET},"stationID","price","volRemaining","escrow","isCorp","minVolume","range","solarSystemID","charID","accountID"
                 FROM market_orders
                WHERE "typeID" = $1 AND "regionID" = $2 AND "bid" = 0 AND "orderState" = 0
                  AND "volRemaining" > 0 AND "price" <= $3 AND {WALLET} <> $4
                  AND ($5 > 0 OR ($5 = -1 AND "stationID" = $6) OR ($5 = 0 AND "solarSystemID" = $7))
                ORDER BY "price", "issued"
                  FOR UPDATE"#
    );
    let rows = txn
        .tx()
        .query(
            sql.as_str(),
            &[
                &(order.type_id as i32),
                &(at.region as i32),
                &(order.price + 0.005),
                &(who as i32),
                &(order.range.max(-1) as i32),
                &(order.station as i32),
                &(at.solar_system as i32),
            ],
        )
        .await?;
    let reachable:Option<std::collections::HashSet<i64>>=if order.range>0 && order.range<32767 {
        Some(crate::gates::within(txn.tx(),at.solar_system,order.range).await.into_iter().map(|(id,_)|id).collect())
    } else {None};
    let fills = take(rows.iter().map(as_match).filter(|m|reachable.as_ref().map_or(true,|systems|systems.contains(&m.system))), order.quantity);
    let filled: i64 = fills.iter().map(|(_, n)| n).sum();
    if order.duration == 0 && filled < order.quantity {
        return refuse("There is not enough for sale at that price within that range.");
    }

    for (m, n) in &fills {
        let amount = m.price * *n as f64;
        let entry = Entry { ref_type: REF_MARKET_TRANSACTION, payer: who, payee: m.owner, arg_id1: "", reason: "" };
        fx.balances.insert((who,order.account), post_account(txn.tx(), who, order.account, -amount, &entry).await?);
        if m.owner >= FIRST_PLAYER_ID {
            fx.balances.insert((m.owner,m.account), post_account(txn.tx(), m.owner, m.account, amount, &entry).await?);
            pay_tax(txn, fx, m.owner, m.issuer, m.account, amount).await?;
            record(txn, m.issuer, m.owner, m.account, who, order.type_id, *n, m.price, TRANSACTION_SALE, m.station, at.region).await?;
        }
        record(txn, me, who, order.account, m.owner, order.type_id, *n, m.price, TRANSACTION_PURCHASE, m.station, at.region).await?;
        let stack = NewItem { type_id: order.type_id, owner_id: who, location_id: m.station, flag: delivery_flag(order.corp), quantity: *n, singleton: false };
        txn.stack_onto(stack).await?;
        history(txn, at.region, order.type_id, m.price, *n).await?;
        fill(txn, fx, m, *n, None).await?;
        fx.refresh.push((me,m.id,order.corp));
    }

    let left = order.quantity - filled;
    if left > 0 {
        let value = order.price * left as f64;
        let levels=crate::svc::skills::levels(txn.tx(),me,&[16597]).await?;
        let escrow = (value * 0.75f64.powi(levels[&16597].clamp(0,5) as i32)*100.0).ceil()/100.0;
        if order.existing.is_none() { broker_fee(txn, fx, me, who, order.account, order.station, value).await?; }
        let entry = Entry { ref_type: REF_MARKET_ESCROW, payer: who, payee: 0, arg_id1: "", reason: "" };
        fx.balances.insert((who,order.account), post_account(txn.tx(), who, order.account, -escrow, &entry).await?);
        let id = list(txn, me, who, order, at, left, escrow).await?;
        fx.orders.push((id, if order.existing.is_some() { "Modify" } else { "Add" }));
    }
    info!("bought {filled} of type {} (listed {left})", order.type_id);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn sell(txn: &mut ItemTxn<'_>, fx: &mut Effects, me: i64, who: i64, order: &NewOrder, at: &Where, roles: i64) -> Result<(), Refusal> {
    let item = if order.existing.is_some() { None } else {
    let Some(item_id) = order.item_id else { return refuse("Choose the item to sell.") };
    let Some(item) = txn.item(item_id).await? else { return refuse("That item no longer exists.") };
    if order.corp {
        // Out of the corporation's office there, from a division the seller may take from.
        let office = crate::svc::corphangar::office(txn.tx(), item.location_id).await;
        let divisions = roles & crate::session::CORP_ROLE_DIRECTOR != 0 || crate::svc::corphangar::division(item.flag).is_some_and(|d| roles & (8_192 << (d - 1)) != 0);
        if item.owner_id != who || office != Some((who, order.station)) || !divisions {
            return refuse("Corporation items must be in a division of its office at that station that you may take from.");
        }
    } else if item.owner_id != me || item.location_id != order.station || item.flag != FLAG_HANGAR as i64 {
        return refuse("Items must be in your hangar at that station to be sold there.");
    }
    if item.type_id != order.type_id || order.quantity > item.quantity {
        return refuse("You do not have that many to sell.");
    }
    if item.singleton {
        return refuse("Repackage the item before selling it. Blueprint copies and assembled items cannot be sold on the market.");
    }
    let holds = txn.tx().query_opt(r#"SELECT 1 FROM entity WHERE "locationID" = $1 LIMIT 1"#, &[&(item_id as i32)]).await?;
    if holds.is_some() {
        return refuse("Empty the item before selling it.");
    }

        Some(item)
    };

    // Buy orders whose range reaches this station, best price first.
    let sql = format!(
        r#"SELECT "orderID",{WALLET},"stationID","price","volRemaining","escrow","isCorp","minVolume","range","solarSystemID","charID","accountID"
                 FROM market_orders
                WHERE "typeID" = $1 AND "regionID" = $2 AND "bid" = 1 AND "orderState" = 0
                  AND "volRemaining" > 0 AND "price" >= $3 AND {WALLET} <> $4
                  AND LEAST("minVolume","volRemaining") <= $5
                  AND ("range" > 0 OR ("range" = -1 AND "stationID" = $6)
                       OR ("range" = 0 AND "solarSystemID" = $7))
                ORDER BY "price" DESC, "issued"
                  FOR UPDATE"#
    );
    let rows = txn
        .tx()
        .query(
            sql.as_str(),
            &[
                &(order.type_id as i32),
                &(at.region as i32),
                &(order.price - 0.005),
                &(who as i32),
                &(order.quantity as i32),
                &(order.station as i32),
                &(at.solar_system as i32),
            ],
        )
        .await?;
    let mut eligible=Vec::new();
    let mut pending=order.quantity;
    for m in rows.iter().map(as_match) {
        if m.range>0 && m.range<32767 && crate::gates::jumps(txn.tx(),at.solar_system,m.system,m.range).await.is_none() {continue}
        let quantity=pending.min(m.remaining);
        if quantity<m.minimum.min(m.remaining) || quantity==0 {continue;}
        let gap=(m.price*quantity as f64-m.escrow).max(0.0);
        if m.owner>=FIRST_PLAYER_ID && gap>0.0 {
            let r=if m.account!=1000 {
                txn.tx().query_opt(r#"SELECT balance FROM corporation_wallets WHERE "corporationID"=$1 AND "accountKey"=$2 FOR UPDATE"#,&[&(m.owner as i32),&m.account]).await?
            } else if m.corp {
                txn.tx().query_opt(r#"SELECT balance FROM corporation WHERE "corporationID"=$1 FOR UPDATE"#,&[&(m.owner as i32)]).await?
            } else {
                txn.tx().query_opt(r#"SELECT balance FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#,&[&(m.owner as i32)]).await?
            };
            if r.is_none_or(|r|r.get::<_,f64>(0)<gap) {continue;}
        }
        pending-=quantity;
        eligible.push(m);
    }
    let fills = take(eligible.into_iter(), order.quantity);
    let sold: i64 = fills.iter().map(|(_, n)| n).sum();
    if order.duration == 0 && sold < order.quantity {
        return refuse("Nobody is buying that many at that price within reach.");
    }

    for (m, n) in &fills {
        let amount = m.price * *n as f64;
        if m.owner>=FIRST_PLAYER_ID && amount>m.escrow {
            let entry=Entry{ref_type:REF_MARKET_TRANSACTION,payer:m.owner,payee:who,arg_id1:"",reason:""};
            fx.balances.insert((m.owner,m.account),post_account(txn.tx(),m.owner,m.account,-(amount-m.escrow),&entry).await?);
        }
        // Paid out of the buyer's escrow, or by an NPC buyer.
        let entry = Entry { ref_type: REF_MARKET_TRANSACTION, payer: m.owner, payee: who, arg_id1: "", reason: "" };
        fx.balances.insert((who,order.account), post_account(txn.tx(), who, order.account, amount, &entry).await?);
        pay_tax(txn, fx, who, me, order.account, amount).await?;
        record(txn, me, who, order.account, m.owner, order.type_id, *n, m.price, TRANSACTION_SALE, order.station, at.region).await?;
        if m.owner >= FIRST_PLAYER_ID {
            record(txn, m.issuer, m.owner, m.account, who, order.type_id, *n, m.price, TRANSACTION_PURCHASE, order.station, at.region).await?;
            let stack = NewItem { type_id: order.type_id, owner_id: m.owner, location_id: order.station, flag: delivery_flag(m.corp), quantity: *n, singleton: false };
            txn.stack_onto(stack).await?;
        }
        history(txn, at.region, order.type_id, m.price, *n).await?;
        fill(txn, fx, m, *n, Some(amount.min(m.escrow))).await?;
        fx.refresh.push((me,m.id,order.corp));
    }

    let left = order.quantity - sold;
    if left > 0 {
        if order.existing.is_none() { broker_fee(txn, fx, me, who, order.account, order.station, order.price * left as f64).await?; }
        let id = list(txn, me, who, order, at, left, 0.0).await?;
        fx.orders.push((id, if order.existing.is_some() { "Modify" } else { "Add" }));
    }
    if let Some(item) = item { txn.set_quantity(item.id, item.quantity - order.quantity).await?; }
    info!("sold {sold} of type {} (listed {left})", order.type_id);
    Ok(())
}

fn as_match(r: &Row) -> Match {
    Match {
        id: r.get::<_, i32>(0) as i64,
        owner: r.get::<_, i32>(1) as i64,
        issuer: r.get::<_, i32>("charID") as i64,
        account: r.get::<_,i32>("accountID"),
        station: r.get::<_, i32>(2) as i64,
        price: r.get(3),
        remaining: r.get::<_, i32>(4) as i64,
        escrow: r.get(5),
        corp: r.try_get::<_, i16>(6).is_ok_and(|c| c != 0),
        minimum:r.try_get::<_,i32>("minVolume").unwrap_or(1) as i64,
        range:r.try_get::<_,i32>("range").unwrap_or(-1) as i64,
        system:r.try_get::<_,i32>("solarSystemID").unwrap_or(0) as i64,
    }
}

/// Walk `orders` until `wanted` units are covered.
fn take(orders: impl Iterator<Item = Match>, wanted: i64) -> Vec<(Match, i64)> {
    let mut left = wanted;
    let mut out = Vec::new();
    for m in orders {
        if left == 0 {
            break;
        }
        let n = left.min(m.remaining);
        if n<m.minimum.min(m.remaining) {continue}
        left -= n;
        out.push((m, n));
    }
    out
}

/// Take `n` units off an order; a buy order also gives up `spent` escrow.
async fn fill(txn: &ItemTxn<'_>, fx: &mut Effects, m: &Match, n: i64, spent: Option<f64>) -> Result<(), Refusal> {
    if m.owner < FIRST_PLAYER_ID && txn.tx().query_one(r#"SELECT "starterSeed" FROM market_orders WHERE "orderID"=$1"#, &[&(m.id as i32)]).await?.get::<_,bool>(0) {
        return Ok(());
    }
    if m.owner>=FIRST_PLAYER_ID && spent.is_some_and(|s|s>m.escrow+0.005) {return refuse("That buy order no longer has enough money in escrow.")}
    let remaining = m.remaining - n;
    let escrow = spent.map_or(m.escrow, |s| (m.escrow - s).max(0.0));
    let state = if remaining == 0 { ORDER_DONE } else { ORDER_OPEN };
    txn.tx()
        .execute(
            r#"UPDATE market_orders SET "volRemaining" = $2, "orderState" = $3, "escrow" = $4
                WHERE "orderID" = $1"#,
            &[&(m.id as i32), &(remaining as i32), &state, &escrow],
        )
        .await?;
    if m.owner >= FIRST_PLAYER_ID {
        fx.orders.push((m.id, if remaining == 0 { "Expiry" } else { "Modify" }));
    }
    Ok(())
}

async fn list(txn: &ItemTxn<'_>, me: i64, who:i64, order: &NewOrder, at: &Where, quantity: i64, escrow: f64) -> Result<i64, Refusal> {
    let range = if order.bid { order.range } else { RANGE_STATION };
    if let Some(id) = order.existing {
        txn.tx().execute(r#"UPDATE market_orders SET price=$2,"volRemaining"=$3,escrow=$4,
            "orderState"=0,issued=$5 WHERE "orderID"=$1"#,
            &[&(id as i32), &order.price, &(quantity as i32), &escrow, &crate::bulkdata::filetime_now()]).await?;
        return Ok(id);
    }
    let levels=crate::svc::skills::levels(txn.tx(),me,&[3443,3444,16596,18580]).await?;
    let maximum=5+levels[&3443]*4+levels[&3444]*8+levels[&16596]*16+levels[&18580]*32;
    let count:i64=txn.tx().query_one(r#"SELECT count(*) FROM market_orders WHERE "charID"=$1 AND "orderState"=0"#,&[&(me as i32)]).await?.get(0);
    if count>=maximum {return refuse("Your Trade skills do not allow any more open orders.");}

    let r = txn
        .tx()
        .query_one(
            r#"INSERT INTO market_orders
                   ("typeID","charID","regionID","stationID","range","bid","price","volEntered",
                    "volRemaining","issued","orderState","minVolume","contraband","accountID",
                    "duration","isCorp","solarSystemID","escrow","jumps","walletOwnerID")
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$8,$9,0,$10,0,$16,$11,$14,$12,$13,1,$15)
               RETURNING "orderID""#,
            &[
                &(order.type_id as i32),
                &(me as i32),
                &(at.region as i32),
                &(order.station as i32),
                &(range as i32),
                &i16::from(order.bid),
                &order.price,
                &(quantity as i32),
                &crate::bulkdata::filetime_now(),
                &(order.min_volume as i32),
                &(order.duration as i32),
                &(at.solar_system as i32),
                &escrow,
                &i16::from(order.corp),
                &(who as i32),
                &order.account,
            ],
        )
        .await?;
    Ok(r.get::<_, i32>(0) as i64)
}

/// The broker's commission for listing an order worth `value`, by the
/// lister's skill, from the wallet the order uses.
async fn broker_fee(txn: &ItemTxn<'_>, fx: &mut Effects, me: i64, who: i64, account: i32, station: i64, value: f64) -> Result<(), Refusal> {
    let level = crate::svc::skills::levels(txn.tx(), me, &[SKILL_BROKER_RELATIONS]).await?;
    let standings=txn.tx().query_one(r#"SELECT
        COALESCE((SELECT standing FROM "chrNPCStandings" WHERE "characterID"=$1 AND "fromID"=n."factionID"),0)::float8,
        COALESCE((SELECT standing FROM "chrNPCStandings" WHERE "characterID"=$1 AND "fromID"=s."corporationID"),0)::float8
        FROM "staStations" s LEFT JOIN "crpNPCCorporations" n ON n."corporationID"=s."corporationID" WHERE s."stationID"=$2"#,
        &[&(me as i32),&(station as i32)]).await?;
    let weighted=(0.7*standings.get::<_,f64>(0)+0.3*standings.get::<_,f64>(1))/10.0;
    let rate = BROKER_FEE * (1.0 - 0.05 * level[&SKILL_BROKER_RELATIONS].clamp(0,5) as f64) * 2.0f64.powf(-2.0*weighted);
    let fee = (value * rate).max(MINIMUM_FEE);
    let entry = Entry { ref_type: REF_BROKER_FEE, payer: who, payee: 0, arg_id1: "", reason: "" };
    fx.balances.insert((who,account), post_account(txn.tx(), who, account, -fee, &entry).await?);
    Ok(())
}

/// Sales tax on `gross` earned by `seller`.
async fn pay_tax(txn: &ItemTxn<'_>, fx: &mut Effects, seller: i64, issuer: i64, account: i32, gross: f64) -> Result<(), Refusal> {
    let level = crate::svc::skills::levels(txn.tx(), issuer, &[SKILL_ACCOUNTING]).await?;
    let tax = gross * TRANSACTION_TAX * (1.0 - 0.1 * level[&SKILL_ACCOUNTING] as f64);
    if tax >= 0.01 {
        let entry = Entry { ref_type: REF_TRANSACTION_TAX, payer: seller, payee: 0, arg_id1: "", reason: "" };
        fx.balances.insert((seller,account), post_account(txn.tx(), seller, account, -tax, &entry).await?);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn record(
    txn: &ItemTxn<'_>,
    who: i64,
    wallet_owner: i64,
    account: i32,
    other: i64,
    type_id: i64,
    quantity: i64,
    price: f64,
    kind: i32,
    station: i64,
    region: i64,
) -> Result<(), Refusal> {
    txn.tx()
        .execute(
            r#"INSERT INTO market_transactions
                   ("transactionDateTime","typeID","quantity","price","transactionType","clientID",
                    "regionID","stationID","characterID","isCorp","accountKey","corporationID")
               VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$12,$11)"#,
            &[
                &crate::bulkdata::filetime_now(),
                &(type_id as i32),
                &(quantity as i32),
                &price,
                &kind,
                &(other as i32),
                &(region as i32),
                &(station as i32),
                &(who as i32),
                &i16::from(wallet_owner != who),
                &(if wallet_owner != who { wallet_owner as i32 } else { 0 }),
                &account,
            ],
        )
        .await?;
    Ok(())
}

async fn history(txn: &ItemTxn<'_>, region: i64, type_id: i64, price: f64, quantity: i64) -> Result<(), Refusal> {
    let day = crate::bulkdata::filetime_now() / 864_000_000_000 * 864_000_000_000;
    let n = txn.tx().execute(r#"UPDATE market_history SET
        "lowPrice"=LEAST("lowPrice",$4), "highPrice"=GREATEST("highPrice",$4),
        "avgPrice"=("avgPrice"*volume+$4*$5::bigint)/(volume+$5::bigint),
        volume=volume+$5, orders=orders+1
        WHERE "regionID"=$1 AND "typeID"=$2 AND "historyDate"=$3"#,
        &[&(region as i32), &(type_id as i32), &day, &price, &quantity]).await?;
    if n == 0 {
        txn.tx().execute(r#"INSERT INTO market_history
            ("regionID","typeID","historyDate","lowPrice","highPrice","avgPrice",volume,orders)
            VALUES ($1,$2,$3,$4,$4,$4,$5,1)"#,
            &[&(region as i32), &(type_id as i32), &day, &price, &quantity]).await?;
    }
    Ok(())
}

/// Refund escrow and return unsold goods exactly once, under the matching lock.
async fn expire_orders(txn: &mut ItemTxn<'_>, fx: &mut Effects) -> Result<(), Refusal> {
    txn.tx().execute("SELECT pg_advisory_xact_lock(45017, 2)", &[]).await?;
    let sql = format!(r#"SELECT "orderID",{WALLET},"stationID",price,"volRemaining",escrow,
        "isCorp",bid,"typeID","charID","accountID" FROM market_orders WHERE "orderState"=0
        AND "charID">=100000000 AND issued+duration::bigint*864000000000 <= $1
        ORDER BY "orderID" FOR UPDATE"#);
    for r in txn.tx().query(&sql, &[&crate::bulkdata::filetime_now()]).await? {
        let order = as_match(&r);
        close_order(txn, fx, &order, r.get::<_,i16>(7)!=0, r.get::<_,i32>(8) as i64, ORDER_DONE).await?;
    }
    Ok(())
}

pub async fn expire(ctx: &Ctx<'_>) -> Outcome {
    transact(ctx, |txn, fx| Box::pin(expire_orders(txn, fx))).await
}

/// An open order of the caller's, locked.
async fn own_order(txn: &ItemTxn<'_>, me: i64, order_id: i64,corp:Option<i64>) -> Result<(Match, bool, i64), Refusal> {
    if i32::try_from(order_id).is_err() { return refuse("Invalid order ID."); }
    txn.tx().execute("SELECT pg_advisory_xact_lock(45017, 2)", &[]).await?;
    let sql = format!(
        r#"SELECT "orderID",{WALLET},"stationID","price","volRemaining","escrow","isCorp","bid","typeID","charID","accountID"
             FROM market_orders WHERE "orderID" = $1 AND ("charID" = $2 OR ("isCorp"=1 AND "walletOwnerID"=$3)) AND "orderState" = 0
              FOR UPDATE"#
    );
    let r = txn.tx().query_opt(sql.as_str(), &[&(order_id as i32), &(me as i32),&corp.map(|id|id as i32)]).await?;
    let Some(r) = r else { return refuse("That order is no longer open.") };
    Ok((as_match(&r), r.get::<_, i16>(7) != 0, r.get::<_, i32>(8) as i64))
}

/// Cancel: a buy order's escrow comes back, a sell order's goods go back to
/// the hangar where it was listed.
async fn cancel(txn: &mut ItemTxn<'_>, fx: &mut Effects, me: i64, order_id: i64,roles:(Option<i64>,i64)) -> Result<(), Refusal> {
    let (order, bid, type_id) = own_order(txn, me, order_id,roles.0).await?;
    authorize_order(&order,roles)?;
    close_order(txn, fx, &order, bid, type_id, ORDER_CANCELLED).await
}

async fn close_order(txn: &mut ItemTxn<'_>, fx: &mut Effects, order: &Match, bid: bool, type_id: i64, state: i32) -> Result<(), Refusal> {
    let order_id = order.id;
    let who = order.owner;
    if bid {
        if order.escrow > 0.0 {
            let entry = Entry { ref_type: REF_MARKET_ESCROW, payer: 0, payee: who, arg_id1: "", reason: "" };
            fx.balances.insert((who,order.account), post_account(txn.tx(), who, order.account, order.escrow, &entry).await?);
        }
    } else if order.remaining > 0 {
        let stack = NewItem { type_id, owner_id: who, location_id: order.station, flag: delivery_flag(order.corp), quantity: order.remaining, singleton: false };
        txn.stack_onto(stack).await?;
    }
    txn.tx()
        .execute(
            r#"UPDATE market_orders SET "orderState" = $2, "escrow" = 0 WHERE "orderID" = $1"#,
            &[&(order_id as i32), &state],
        )
        .await?;
    fx.orders.push((order_id, "Expiry"));
    info!("cancelled order {order_id}");
    Ok(())
}

/// Reprice: a buy order's escrow follows its price.
async fn modify(txn: &mut ItemTxn<'_>, fx: &mut Effects, me: i64, order_id: i64, price: f64, roles: (Option<i64>,i64)) -> Result<(), Refusal> {
    if !price.is_finite() || price < 0.01 || price > 9_223_372_036_854.0 {
        return refuse("Enter a valid price.");
    }
    let (old, bid, type_id) = own_order(txn, me, order_id, roles.0).await?;
    authorize_order(&old, roles)?;
    let r = txn.tx().query_one(r#"SELECT range,"minVolume",duration,"solarSystemID","regionID"
        FROM market_orders WHERE "orderID"=$1"#, &[&(order_id as i32)]).await?;
    let order = NewOrder { station: old.station, type_id, price, quantity: old.remaining, bid,
        range: r.get::<_,i32>(0) as i64, min_volume: (r.get::<_,i32>(1) as i64).min(old.remaining),
        duration: r.get::<_,i32>(2) as i64, corp: old.corp, item_id: None, existing: Some(order_id), account: old.account };
    let at = Where { solar_system: r.get::<_,i32>(3) as i64, region: r.get::<_,i32>(4) as i64 };
    // Release the old reservation inside the same transaction. The replacement
    // immediately matches crossing orders and reserves only its remainder.
    if bid && old.escrow > 0.0 {
        let entry = Entry { ref_type: REF_MARKET_ESCROW, payer: 0, payee: old.owner, arg_id1: "", reason: "" };
        fx.balances.insert((old.owner,old.account), post_account(txn.tx(), old.owner, old.account, old.escrow, &entry).await?);
    }
    broker_fee(txn, fx, me, old.owner, old.account, old.station, ((price-old.price)*old.remaining as f64).max(0.0)).await?;
    txn.tx().execute(r#"UPDATE market_orders SET "orderState"=2,"volRemaining"=0,escrow=0,price=$2 WHERE "orderID"=$1"#,
        &[&(order_id as i32), &price]).await?;
    fx.orders.push((order_id,"Expiry"));
    if bid { buy(txn, fx, old.issuer, old.owner, &order, &at).await?; }
    else { sell(txn, fx, old.issuer, old.owner, &order, &at, roles.1).await?; }
    // One final notification, rather than an expiry followed by a modification.
    if fx.orders.iter().filter(|(id,_)| *id==order_id).count()>1 {
        let first=fx.orders.iter().position(|(id,_)|*id==order_id).unwrap();
        fx.orders.remove(first);
    }
    Ok(())
}

fn authorize_order(order:&Match,roles:(Option<i64>,i64))->Result<(),Refusal> {
    if order.corp && !crate::svc::wallet::can_take(roles.1,order.account) {return refuse("You may not manage orders in that corporation wallet division.");}
    if order.corp && (roles.0!=Some(order.owner) || roles.1 & (crate::session::CORP_ROLE_DIRECTOR | crate::session::CORP_ROLE_ACCOUNTANT | CORP_ROLE_TRADER)==0) {return refuse("You no longer have permission to manage that corporation's order.")}
    Ok(())
}

#[cfg(test)]
#[path = "trade_tests.rs"]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matching_honours_each_orders_minimum_after_prior_fills() {
        let order=|id,remaining,minimum|Match {id,owner:1,issuer:1,account:1000,station:1,price:1.0,remaining,escrow:100.0,corp:false,minimum,range:-1,system:1};
        let filled=take(vec![order(1,8,1),order(2,100,5),order(3,1,5)].into_iter(),10);
        assert_eq!(filled.iter().map(|(m,n)|(m.id,*n)).collect::<Vec<_>>(),vec![(1,8),(3,1)]);
    }
    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn corporation_escrow_stays_with_original_corporation_after_member_leaves() {
        let fixture=crate::test_support::Database::load().await;
        let me=fixture.id().await as i64; let corp=fixture.id().await as i64; let next=fixture.id().await as i64;
        fixture.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","corporationID",balance) VALUES ($1,$2,$3,10000000)"#,&[&(me as i32),&format!("Market issuer {me}"),&(corp as i32)]).await.unwrap();
        for id in [corp,next] {fixture.db.execute(r#"INSERT INTO corporation ("corporationID","corporationName",balance) VALUES ($1,$2,10000000)"#,&[&(id as i32),&format!("Market corp {id}")]).await.unwrap();}
        let mut client=crate::db::connect(&fixture.url).await.unwrap();
        let mut tx=ItemTxn::begin(&mut client).await.unwrap(); let mut fx=Effects::default();
        let order=NewOrder {station:60000004,type_id:34,price:0.01,quantity:10,bid:true,range:-1,item_id:None,min_volume:1,duration:1,corp:true,existing:None,account:1000};
        assert!(place(&mut tx,&mut fx,me,order,(Some(corp),1)).await.is_ok());
        let id=fx.orders[0].0; tx.commit().await.unwrap();
        fixture.db.execute(r#"UPDATE "character_" SET "corporationID"=$2 WHERE "characterID"=$1"#,&[&(me as i32),&(next as i32)]).await.unwrap();
        let mut tx=ItemTxn::begin(&mut client).await.unwrap(); let (read,_,_)=own_order(&tx,me,id,Some(next)).await.ok().unwrap(); assert_eq!(read.owner,corp);
        assert!(cancel(&mut tx,&mut Effects::default(),me,id,(Some(next),1)).await.is_err());
        drop(tx);
        let mut tx=ItemTxn::begin(&mut client).await.unwrap(); assert!(cancel(&mut tx,&mut Effects::default(),me+1,id,(Some(corp),1)).await.is_ok()); tx.commit().await.unwrap();
        let balance:f64=fixture.db.query_one(r#"SELECT balance FROM corporation WHERE "corporationID"=$1"#,&[&(next as i32)]).await.unwrap().get(0); assert_eq!(balance,10_000_000.0);
    }
}
