//! Player-to-player trade (pvptradesvc.py, pvptradewnd.py), and the CEO
//! trade in which one side of a war surrenders.
//!
//! Two pilots docked in the same station open a trade: trademgr.InitiateTrade
//! makes a trade container (a real item in the station, owned by EVE System,
//! so neither hangar lists it) and hands both a bound session on it. What a
//! pilot offers moves into the container, still theirs; any change to either
//! offer clears both acceptances. When both accept, what each offered goes
//! to the other's hangar, the money offered changes wallets, and for a CEO
//! trade the war ends. Either side can abort, and everything goes home.
//!
//! The trades under way live in the world (`WorldState::trades`); a
//! container left over from a restart is emptied back into its owners'
//! hangars when the server starts ([`recover`]).

use eve_marshal::Value;
use std::sync::Arc;
use tokio_postgres::Client;
use tracing::{error, info};

use crate::items::{self, ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::bound;
use crate::svc::inventory::{self, Container, FLAG_HANGAR};
use crate::svc::membership::{db_failed, message, Refused};
use crate::svc::rows::{int, key_val};
use crate::svc::wallet::{self, Entry};
use crate::world::World;

const SESSION: &str = "tradeSession";
const TYPE_CARGO_CONTAINER: i64 = 23;
const OWNER_SYSTEM: i64 = 1;
/// The item name that marks a trade container, for [`recover`].
const CONTAINER_NAME: &str = "Trade";
/// market_refTypes: Player Trading.
const REF_PLAYER_TRADING: i32 = 1;

/// One trade under way.
#[derive(Debug, Clone)]
pub struct Trade {
    /// Serialises offer changes, inventory withdrawals, acceptance and abort.
    pub gate: Arc<tokio::sync::Mutex<()>>,
    pub container: i64,
    pub station: i64,
    pub traders: [i64; 2],
    pub accepted: [bool; 2],
    pub money: [f64; 2],
    /// For a CEO trade: the war it ends.
    pub war: Option<i64>,
}

impl Trade {
    fn side(&self, char_id: i64) -> Option<usize> {
        self.traders.iter().position(|t| *t == char_id)
    }
}

/// trademgr.InitiateTrade(charID, warID=None).
pub async fn trademgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if call.method != "InitiateTrade" {
        return None;
    }
    let war = call.arg_i64(1);
    Some(initiate(ctx, call.arg_id(0), war).await.map_or_else(|refusal| refusal, Outcome::Value))
}

async fn initiate(ctx: &Ctx<'_>, other: i64, war: Option<i64>) -> Refused<Value> {
    let (Some(me), Some(station)) = (ctx.session.char_id, ctx.session.station_id) else {
        return Err(message(if war.is_some() { "MustBeInStationToNegotiate" } else { "OtherCharNotAtStation" }));
    };
    let other_name = name_of(ctx, other).await;
    if other == me {
        return Err(Outcome::custom("You cannot trade with yourself."));
    }
    {
        let w = ctx.world.lock();
        if w.station_of(other) != Some(station) {
            return Err(Outcome::UserError {
                msg: "OtherCharNotAtStation".into(),
                args: vec![("name".into(), Value::text(&other_name))],
            });
        }
        if w.trades.values().any(|t| t.side(me).is_some() || t.side(other).is_some()) {
            return Err(Outcome::custom("One of you is already trading."));
        }
    }
    if let Some(war) = war {
        may_surrender(ctx, war, me, other).await?;
    }

    let result = async {
        let mut client = crate::db::connect(ctx.db_url).await?;
        let mut txn = ItemTxn::begin(&mut client).await?;
        let container = txn
            .create(NewItem { type_id: TYPE_CARGO_CONTAINER, owner_id: OWNER_SYSTEM, location_id: station, flag: 0, quantity: 1, singleton: true })
            .await?;
        txn.tx().execute(r#"UPDATE entity SET "itemName" = $2 WHERE "itemID" = $1"#, &[&(container as i32), &CONTAINER_NAME]).await?;
        txn.commit().await?;
        Ok::<_, tokio_postgres::Error>(container)
    }
    .await;
    let container = result.map_err(|e| db_failed("opening the trade", e))?;
    let trade = Trade { gate: Arc::new(tokio::sync::Mutex::new(())), container, station, traders: [me, other], accepted: [false; 2], money: [0.0; 2], war };
    let session = bound::object_ref(ctx.node_id, SESSION, &[container]);
    let registered = {
        let mut w = ctx.world.lock();
        // Both participants may initiate simultaneously while the DB call yields.
        if w.station_of(me)!=Some(station) || w.station_of(other)!=Some(station) || w.trades.values().any(|t|t.side(me).is_some() || t.side(other).is_some()) { false } else {
            w.trades.insert(container, trade);
            let event = Value::Tuple(vec![Value::bytes("Initiate"), Value::Int(me), session.clone()]);
            w.notify(&w.clients_of(other), "OnTrade", event);
            true
        }
    };
    if !registered {
        ctx.db.execute(r#"DELETE FROM entity WHERE "itemID"=$1"#,&[&(container as i32)]).await.map_err(|e|db_failed("closing the unused trade",e))?;
        return Err(Outcome::custom("One of you has left the station or is already trading."));
    }
    info!("{me} opened a {}trade with {other} ({container})", if war.is_some() { "surrender " } else { "" });
    Ok(session)
}

/// A CEO trade: both are the active CEOs (an alliance's: its executor's) of
/// the two sides of a war that has not ended.
async fn may_surrender(ctx: &Ctx<'_>, war: i64, me: i64, other: i64) -> Refused<()> {
    let Some((declarer, against)) = crate::svc::war::sides(ctx.db, war).await else {
        return Err(message("CanNotSurrenderAsWarIsOver"));
    };
    let mut leaders = Vec::new();
    for side in [declarer, against] {
        let sql = r#"SELECT co."ceoID" FROM "corporation" co
                      WHERE co."corporationID" = COALESCE((SELECT "executorCorpID" FROM alliances WHERE "allianceID" = $1), $1)"#;
        match ctx.query_one(sql, &[&(side as i32)]).await.and_then(|r| int(&r, 0)) {
            Some(ceo) => leaders.push(ceo),
            None => return Err(message("CanNotSurrenderToAllianceInTurmoil")),
        }
    }
    if !(leaders.contains(&me) && leaders.contains(&other)) {
        return Err(message("MustBeActiveCEOToEndWar"));
    }
    Ok(())
}

async fn name_of(ctx: &Ctx<'_>, char_id: i64) -> String {
    let sql = r#"SELECT "characterName" FROM "character_" WHERE "characterID" = $1"#;
    ctx.query_one(sql, &[&(char_id as i32)]).await.and_then(|r| crate::svc::rows::text(&r, 0)).unwrap_or_default()
}

/// The bound trade session.
pub async fn session(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let container = bound::parse(&call.service).and_then(|b| b.params.first().copied()).unwrap_or(0);
    let me = ctx.session.char_id.unwrap_or(0);
    let trade = ctx.world.lock().trades.get(&container).cloned();
    let Some(trade) = trade.filter(|t| t.side(me).is_some() && ctx.session.station_id == Some(t.station)) else {
        return Some(Outcome::custom("That trade is over."));
    };
    let _guard=trade.gate.lock().await;
    let Some(trade)=ctx.world.lock().trades.get(&container).cloned() else {return Some(Outcome::custom("That trade is over."))};
    let ids = |v: Option<&Value>| -> Vec<i64> { v.and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64()).collect() };
    let result = match call.method.as_str() {
        "List" => Ok(key_val(vec![
            ("tradeContainerID", Value::Int(container)),
            ("traders", Value::List(trade.traders.iter().map(|t| Value::Int(*t)).collect())),
            ("items", inventory::list(ctx, Container::Item(container)).await),
            ("money", Value::List(trade.money.iter().map(|m| Value::Double(*m)).collect())),
        ])),
        "IsCEOTrade" => Ok(Value::Bool(trade.war.is_some())),
        "Add" => offer(ctx, &trade, &[call.arg_id(0)], call.arg_i64(1)).await,
        "MultiAdd" => offer(ctx, &trade, &ids(call.args.first()), None).await,
        "MultiMerge" | "StackAll" => Ok(Value::None),
        "OfferMoney" => offer_money(ctx, &trade, call.args.first().and_then(|v| v.as_f64()).unwrap_or(0.0)).await,
        "ToggleAccept" => accept(ctx, container, call.arg_i64(0).unwrap_or(0) != 0).await,
        "Abort" => abort_locked(ctx.db_url, ctx.world, container).await.map(|_| Value::None),
        _ => return Some(Outcome::Unknown(format!("{SESSION}::{}", call.method))),
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

/// Add/MultiAdd: the pilot's own items from the hangar go into the trade.
async fn offer(ctx: &Ctx<'_>, trade: &Trade, ids: &[i64], quantity: Option<i64>) -> Refused<Value> {
    if quantity.is_some_and(|q|q<=0 || q>i32::MAX as i64) {return Err(Outcome::custom("Choose a positive item quantity."))}
    let me = ctx.session.char_id.unwrap_or(0);
    let failed = |e| db_failed("offering the items", e);
    let events = {
        let mut client = crate::db::connect(ctx.db_url).await.map_err(failed)?;
        let mut txn = ItemTxn::begin(&mut client).await.map_err(failed)?;
        for &id in ids {
            let Some(item) = txn.item(id).await.map_err(failed)? else { continue };
            let in_hangar = item.location_id == trade.station && item.flag == i64::from(FLAG_HANGAR);
            if item.owner_id != me || !in_hangar || Some(item.id) == ctx.session.ship_id {
                continue;
            }
            match offer_split_quantity(quantity, item.quantity, item.singleton)? {
                Some(q) => {
                    txn.set_quantity(id, item.quantity - q).await.map_err(failed)?;
                    let new = NewItem { type_id: item.type_id, owner_id: me, location_id: trade.container, flag: 0, quantity: q, singleton: false };
                    txn.create(new).await.map_err(failed)?;
                }
                None => txn.move_to(id, trade.container, 0).await.map_err(failed)?,
            }
        }
        txn.commit().await.map_err(failed)?
    };
    tell_items(ctx.world, trade, events);
    reset(ctx.world, trade.container);
    Ok(Value::None)
}

fn offer_split_quantity(requested: Option<i64>, available: i64, singleton: bool) -> Refused<Option<i64>> {
    let available = if singleton { 1 } else { available };
    let quantity = requested.unwrap_or(available);
    if quantity <= 0 || quantity > available || quantity > i32::MAX as i64 {
        return Err(Outcome::custom("That quantity is not available to trade."));
    }
    Ok((quantity < available).then_some(quantity))
}

async fn offer_money(ctx: &Ctx<'_>, trade: &Trade, amount: f64) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let Some(side) = trade.side(me) else { return Ok(Value::None) };
    let have = ctx
        .query_one(r#"SELECT "balance" FROM "character_" WHERE "characterID" = $1"#, &[&(me as i32)])
        .await
        .and_then(|r| crate::svc::rows::float(&r, 0))
        .unwrap_or(0.0);
    if !amount.is_finite() || amount < 0.0 {
        return Err(Outcome::custom("That is not an amount of money."));
    }
    if amount > have {
        return Err(Outcome::not_enough_money(amount, have));
    }
    let mut w = ctx.world.lock();
    let Some(t) = w.trades.get_mut(&trade.container) else { return Ok(Value::None) };
    t.money[side] = amount;
    t.accepted = [false; 2];
    let money = Value::List(t.money.iter().map(|m| Value::Double(*m)).collect());
    let event = Value::Tuple(vec![Value::bytes("MoneyOffer"), Value::Int(trade.container), money]);
    let traders = t.traders;
    let clients: Vec<i64> = traders.iter().flat_map(|c| w.clients_of(*c)).collect();
    w.notify(&clients, "OnTrade", event);
    Ok(Value::None)
}

/// ToggleAccept(state); both accepting completes the trade.
async fn accept(ctx: &Ctx<'_>, container: i64, state: bool) -> Refused<Value> {
    let me = ctx.session.char_id.unwrap_or(0);
    let both = {
        let mut w = ctx.world.lock();
        let Some(t) = w.trades.get_mut(&container) else { return Ok(Value::None) };
        let Some(side) = t.side(me) else { return Ok(Value::None) };
        t.accepted[side] = state;
        let (accepted, traders) = (t.accepted, t.traders);
        let states = Value::List(accepted.iter().map(|a| Value::Int(i64::from(*a))).collect());
        let clients: Vec<i64> = traders.iter().flat_map(|c| w.clients_of(*c)).collect();
        w.notify(&clients, "OnTrade", Value::Tuple(vec![Value::bytes("StateToggle"), Value::Int(container), states]));
        accepted == [true, true]
    };
    if both {
        complete(ctx, container).await?;
    }
    Ok(Value::None)
}

/// Each side's offer goes to the other's hangar; the money changes hands;
/// a surrender ends its war.
async fn complete(ctx: &Ctx<'_>, container: i64) -> Refused<()> {
    let Some(trade) = ctx.world.lock().trades.get(&container).cloned() else { return Ok(()) };
    let result = async {
        let mut client = crate::db::connect(ctx.db_url).await.map_err(|e| db_failed("connecting", e))?;
        let mut txn = ItemTxn::begin(&mut client).await.map_err(|e| db_failed("BEGIN", e))?;
        let surrender=if let Some(war)=trade.war {
            may_surrender(ctx,war,trade.traders[0],trade.traders[1]).await?;
            Some(crate::svc::war::prepare_surrender(txn.tx(),war).await?)
        } else {None};
        let inside: Vec<(i64, i64)> = txn
            .tx()
            .query(r#"SELECT "itemID", "ownerID" FROM entity WHERE "locationID" = $1"#, &[&(container as i32)])
            .await
            .map_err(|e| db_failed("reading the offers", e))?
            .iter()
            .filter_map(|r| Some((int(r, 0)?, int(r, 1)?)))
            .collect();
        for (item, owner) in inside {
            if !trade.traders.contains(&owner) {return Err(Outcome::custom("The trade contains an item belonging to somebody else."))}
            let to = if owner == trade.traders[0] { trade.traders[1] } else { trade.traders[0] };
            let children=txn.tx().query(r#"WITH RECURSIVE contents AS (SELECT "itemID","ownerID" FROM entity WHERE "locationID"=$1 UNION SELECT e."itemID",e."ownerID" FROM entity e JOIN contents c ON e."locationID"=c."itemID") SELECT "itemID","ownerID" FROM contents ORDER BY "itemID""#,&[&(item as i32)]).await.map_err(|e|db_failed("reading container contents",e))?;
            for child in children {
                if child.get::<_,i32>(1) as i64!=owner {return Err(Outcome::custom("An offered container holds somebody else's property."))}
                txn.set_owner(child.get::<_,i32>(0) as i64,to).await.map_err(|e|db_failed("handing over container contents",e))?;
            }
            txn.move_to(item, trade.station, i64::from(FLAG_HANGAR)).await.map_err(|e| db_failed("handing over", e))?;
            txn.set_owner(item, to).await.map_err(|e| db_failed("handing over", e))?;
        }
        let mut balances = Vec::new();
        for (from, to, amount) in [(0, 1, trade.money[0]), (1, 0, trade.money[1])] {
            if amount <= 0.0 {
                continue;
            }
            let (payer, payee) = (trade.traders[from], trade.traders[to]);
            let entry = Entry { ref_type: REF_PLAYER_TRADING, payer, payee, arg_id1: "", reason: "" };
            let paid = wallet::post(txn.tx(), payer, -amount, &entry).await.map_err(crate::svc::membership::transfer_refused)?;
            let got = wallet::post(txn.tx(), payee, amount, &entry).await.map_err(crate::svc::membership::transfer_refused)?;
            balances.extend([(payer, paid), (payee, got)]);
        }
        txn.destroy(container).await.map_err(|e| db_failed("closing the trade", e))?;
        let events = txn.commit().await.map_err(|e| db_failed("COMMIT", e))?;
        Ok::<_, Outcome>((events, balances,surrender))
    }
    .await;
    let (events, balances,surrender) = match result {
        Ok(v) => v,
        Err(refusal) => {
            // Leave the trade open, unaccepted, for the two to sort out.
            reset(ctx.world, container);
            return Err(refusal);
        }
    };
    tell_items(ctx.world, &trade, events);
    {
        let mut w = ctx.world.lock();
        w.trades.remove(&container);
        for (owner, balance) in balances {
            let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(owner), Value::Double(balance)]);
            w.notify(&w.clients_of(owner), "OnAccountChange", cash);
        }
        let clients: Vec<i64> = trade.traders.iter().flat_map(|c| w.clients_of(*c)).collect();
        w.notify(&clients, "OnTrade", Value::Tuple(vec![Value::bytes("TradeComplete"), Value::Int(container)]));
    }
    info!("trade {container} between {} and {} completed", trade.traders[0], trade.traders[1]);
    if let (Some(war),Some(event)) = (trade.war,surrender) {
        crate::svc::war::announce_surrender(ctx.world, ctx.db, war,event).await;
    }
    Ok(())
}

/// Abort: everything goes back to its owner's hangar.
pub async fn abort(db_url: &str, world: &World, container: i64) -> Refused<()> {
    let Some(trade)=world.lock().trades.get(&container).cloned() else {return Ok(())};
    let _guard=trade.gate.lock().await;
    abort_locked(db_url,world,container).await
}

async fn abort_locked(db_url: &str, world: &World, container: i64) -> Refused<()> {
    let Some(trade) = world.lock().trades.get(&container).cloned() else { return Ok(()) };
    let result = async {
        let mut client = crate::db::connect(db_url).await?;
        let mut txn = ItemTxn::begin(&mut client).await?;
        let inside: Vec<i64> = txn
            .tx()
            .query(r#"SELECT "itemID" FROM entity WHERE "locationID" = $1"#, &[&(container as i32)])
            .await?
            .iter()
            .filter_map(|r| int(r, 0))
            .collect();
        for item in inside {
            txn.move_to(item, trade.station, i64::from(FLAG_HANGAR)).await?;
        }
        txn.destroy(container).await?;
        txn.commit().await
    }
    .await;
    let events = result.map_err(|e| db_failed("returning the offers", e))?;
    world.lock().trades.remove(&container);
    tell_items(world, &trade, events);
    let w = world.lock();
    let clients: Vec<i64> = trade.traders.iter().flat_map(|c| w.clients_of(*c)).collect();
    w.notify(&clients, "OnTrade", Value::Tuple(vec![Value::bytes("Cancel"), Value::Int(container)]));
    info!("trade {container} aborted");
    Ok(())
}

/// Inventory withdrawals must use the same gate as offer and acceptance RPCs.
pub async fn inventory_guard(world:&World,char_id:i64)->Option<tokio::sync::OwnedMutexGuard<()>> {
    let gate=world.lock().trades.values().find(|t|t.side(char_id).is_some()).map(|t|Arc::clone(&t.gate));
    match gate {Some(gate)=>Some(gate.lock_owned().await),None=>None}
}

/// Both acceptances clear when either offer changes.
fn reset(world: &World, container: i64) {
    let mut w = world.lock();
    let Some(t) = w.trades.get_mut(&container) else { return };
    t.accepted = [false; 2];
    let traders = t.traders;
    let clients: Vec<i64> = traders.iter().flat_map(|c| w.clients_of(*c)).collect();
    let states = Value::List(vec![Value::Int(0), Value::Int(0)]);
    w.notify(&clients, "OnTrade", Value::Tuple(vec![Value::bytes("StateToggle"), Value::Int(container), states]));
}

/// Item changes go to both traders: each watches the other's offer.
fn tell_items(world: &World, trade: &Trade, events: Vec<Value>) {
    let clients: Vec<i64> = {
        let w = world.lock();
        trade.traders.iter().flat_map(|c| w.clients_of(*c)).collect()
    };
    for client in clients {
        items::notify(world, client, events.clone());
    }
}

/// Whether `item` is the caller's own offer in a trade it is part of: it
/// may take it back out (moves.rs).
pub fn my_offer(ctx: &Ctx<'_>, owner: i64, location: i64) -> bool {
    let me = ctx.session.char_id.unwrap_or(0);
    owner == me && ctx.world.lock().trades.get(&location).is_some_and(|t| t.side(me).is_some())
}

/// Something left a trade container (taken back out): the acceptances clear.
pub fn offers_changed(world: &World, origins: &[i64]) {
    let changed: Vec<i64> = {
        let w = world.lock();
        origins.iter().copied().filter(|o| w.trades.contains_key(o)).collect()
    };
    for container in changed {
        reset(world, container);
    }
}

/// Trades a pilot is part of end when they leave the station.
pub async fn left_station(db_url: &str, world: &World, char_id: i64) {
    let mine: Vec<i64> = world.lock().trades.values().filter(|t| t.side(char_id).is_some()).map(|t| t.container).collect();
    for container in mine {
        if let Err(Outcome::UserError { msg, .. }) = abort(db_url, world, container).await {
            error!("aborting trade {container}: {msg}");
        }
    }
}

/// At startup: trade containers left over from before are emptied back into
/// their owners' hangars, and go.
pub async fn recover(db: &Client) {
    let sql = r#"WITH box AS (
                     SELECT "itemID", "locationID" FROM entity
                      WHERE "typeID" = $1 AND "ownerID" = $2 AND "itemName" = $3),
                 back AS (
                     UPDATE entity e SET "locationID" = box."locationID", "flag" = $4
                       FROM box WHERE e."locationID" = box."itemID"
                     RETURNING e."itemID")
                 DELETE FROM entity WHERE "itemID" IN (SELECT "itemID" FROM box)
                 RETURNING (SELECT count(*) FROM back)"#;
    match db.query(sql, &[&(TYPE_CARGO_CONTAINER as i32), &(OWNER_SYSTEM as i32), &CONTAINER_NAME, &FLAG_HANGAR]).await {
        Ok(rows) if !rows.is_empty() => info!("{} trade container(s) from before were emptied back into hangars", rows.len()),
        Ok(_) => {}
        Err(e) => error!("recovering trade containers: {}", crate::logging::chain(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offers_reject_unavailable_quantities_instead_of_moving_the_whole_stack() {
        assert!(offer_split_quantity(Some(6), 5, false).is_err());
        assert!(offer_split_quantity(Some(2), 1, true).is_err());
        assert!(offer_split_quantity(Some(0), 5, false).is_err());
        assert!(matches!(offer_split_quantity(Some(3), 5, false), Ok(Some(3))));
        assert!(matches!(offer_split_quantity(Some(5), 5, false), Ok(None)));
        assert!(matches!(offer_split_quantity(None, 1, true), Ok(None)));
    }

    #[test]
    fn a_trader_has_a_side_and_a_stranger_has_none() {
        let t = Trade { gate:Arc::new(tokio::sync::Mutex::new(())), container: 1, station: 2, traders: [10, 20], accepted: [false; 2], money: [0.0; 2], war: None };
        assert_eq!(t.side(20), Some(1));
        assert_eq!(t.side(30), None);
    }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn concurrent_acceptance_settles_once_and_transfers_container_contents() {
        let fixture=crate::test_support::Database::load().await;
        let a=fixture.id().await; let b=fixture.id().await;
        for id in [a,b] {fixture.db.execute(r#"INSERT INTO "character_" ("characterID","characterName",balance) VALUES ($1,$2,1000)"#,&[&id,&format!("Trade race {id}")]).await.unwrap();}
        let container=fixture.db.query_one(r#"INSERT INTO entity ("typeID","ownerID","locationID",quantity) VALUES (23,1,60000004,1) RETURNING "itemID""#,&[]).await.unwrap().get::<_,i32>(0) as i64;
        let offered=fixture.db.query_one(r#"INSERT INTO entity ("typeID","ownerID","locationID",quantity,singleton) VALUES (23,$1,$2,1,1) RETURNING "itemID""#,&[&a,&(container as i32)]).await.unwrap().get::<_,i32>(0);
        let child=fixture.db.query_one(r#"INSERT INTO entity ("typeID","ownerID","locationID",quantity,flag) VALUES (34,$1,$2,5,5) RETURNING "itemID""#,&[&a,&offered]).await.unwrap().get::<_,i32>(0);
        fixture.world.lock().trades.insert(container,Trade {gate:Arc::new(tokio::sync::Mutex::new(())),container,station:60000004,traders:[a as i64,b as i64],accepted:[true,false],money:[100.0,0.0],war:None});
        let s=crate::session::Session {char_id:Some(b as i64),station_id:Some(60000004),..Default::default()}; let ctx=fixture.ctx(&s);
        let call=Call {service:bound::object_id(1,SESSION,&[container]),method:"ToggleAccept".into(),args:vec![Value::Bool(true)],kwargs:vec![]};
        let (first,second)=tokio::join!(session(&call,&ctx),session(&call,&ctx));
        assert_eq!([first,second].iter().filter(|r|matches!(r,Some(Outcome::Value(_)))).count(),1);
        let balance:f64=fixture.db.query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,&[&a]).await.unwrap().get(0); assert_eq!(balance,900.0);
        let owner:i32=fixture.db.query_one(r#"SELECT "ownerID" FROM entity WHERE "itemID"=$1"#,&[&child]).await.unwrap().get(0); assert_eq!(owner,b);
        assert!(!fixture.world.lock().trades.contains_key(&container));
    }
}
