//! Moving items between containers: the invcontainer writes Add, MultiAdd,
//! MergeItems, MultiMerge and StackAll (invCache.py:621-763), ReplaceCharges
//! (shipmodulebutton.py:311), and ship.AssembleShip (menusvc.py:2452).
//!
//! A character can reach its own items in the station it is docked in (on the
//! hangar floor, or one or two containers deep) or, in space, in its ship.

use eve_marshal::Value;
use tracing::{error, info};

use crate::dogma::category;
use crate::items::{DbResult, Item, ItemTxn, NewItem};
use crate::services::{Call, Ctx, Outcome};
use crate::svc::fitting::{self, Snapshot};
use crate::svc::inventory::{Container, CONTAINER_HANGAR, FLAG_HANGAR};

pub const FLAG_CARGO: i64 = 5;
pub const FLAG_DRONE_BAY: i64 = 87;
const CATEGORY_SHIP: i64 = 6;
const ATTR_DRONE_CAPACITY: i32 = 283;

/// Why a move did not happen.
pub enum MoveError {
    Refused(String),
    Db(tokio_postgres::Error),
    Reactivation {item:i64,delay:i64},
}

pub fn reactivation_error(item:i64,delay:i64)->Outcome{
    Outcome::UserError{msg:"CannotRemoveModuleReactivationDelayed".into(),args:vec![("item".into(),Value::Int(item)),("delay".into(),Value::Double(delay as f64/1000.0))]}
}

impl From<tokio_postgres::Error> for MoveError {
    fn from(e: tokio_postgres::Error) -> Self {
        MoveError::Db(e)
    }
}

pub fn refuse<T>(why: impl Into<String>) -> Result<T, MoveError> {
    Err(MoveError::Refused(why.into()))
}

/// The writes on a bound container; None for anything else.
pub async fn container_write(ctx: &Ctx<'_>, container: Container, call: &Call) -> Option<Outcome> {
    if let Container::Item(id)=container{if !(1..=i64::from(i32::MAX)).contains(&id){return Some(Outcome::custom("Invalid container identifier."));}}
    let flag = |i: usize| call.kwarg("flag").or(call.args.get(i)).and_then(|v| v.as_i64());
    let ids = |v: Option<&Value>| -> Vec<i64> {
        v.and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64()).collect()
    };
    let removing=match call.method.as_str(){"Add"=>vec![call.arg_id(0)],"MultiAdd"=>ids(call.args.first()),_=>vec![]};
    for item in removing {
        if let Some(delay)=crate::cloak::reactivation_remaining(&ctx.world.lock(),item,crate::combat::now_ms()) {
            return Some(reactivation_error(item,delay));
        }
    }
    let op = match call.method.as_str() {
        // Add(itemID, quantity=None, flag=None)
        "Add" => Op::Add {
            items: vec![call.arg_id(0)],
            quantity: call.kwarg("qty").or(call.args.get(1)).and_then(|v| v.as_i64()),
            flag: flag(2),
        },
        // MultiAdd(itemIDs, quantity, flag): always whole stacks.
        "MultiAdd" => {
            let Some(items)=call.args.first().and_then(Value::seq).and_then(|v|v.iter().map(Value::as_i64).collect::<Option<Vec<_>>>()) else{return Some(Outcome::custom("Invalid item list."));};
            Op::Add { items, quantity: None, flag: flag(2) }
        },
        "MergeItems" => Op::Merge(vec![(call.arg_id(0), call.arg_id(1), call.arg_i64(2))]),
        // MultiMerge([(sourceID, destID, quantity)])
        "MultiMerge" => {
            let Some(ops)=call.args.first().and_then(Value::seq).and_then(|ops|ops.iter().map(|op|{
                let op=op.seq()?;if op.len()!=3{return None;}
                let quantity=if matches!(op[2],Value::None){None}else{Some(op[2].as_i64()?)};
                Some((op[0].as_i64()?,op[1].as_i64()?,quantity))
            }).collect::<Option<Vec<_>>>()) else{return Some(Outcome::custom("Invalid stack merge list."));};
            Op::Merge(ops)
        },
        // StackAll(flag=None, securityCode=None)
        "StackAll" => Op::StackAll { flag: flag(0) },
        // ReplaceCharges(flag, itemID, quantity): change ammo (shipmodulebutton.py:311).
        "ReplaceCharges" => Op::ReplaceCharges { flag: flag(0), item: call.arg_id(1), quantity: call.arg_i64(2) },
        // Trinity fitting/slot.py removes permanent rigs through this call.
        "DestroyFitting" => Op::DestroyFitting(call.arg_id(0)),
        _ => return None,
    };
    Some(run(ctx, container, op).await)
}

/// ship.AssembleShip(itemIDs): packaged ships in the hangar become usable ones.
pub async fn assemble_ship(ctx: &Ctx<'_>, item_ids: Vec<i64>) -> Outcome {
    let hangar = Container::Special { container_id: CONTAINER_HANGAR, owner_id: None };
    run(ctx, hangar, Op::Assemble(item_ids)).await
}

enum Op {
    Add { items: Vec<i64>, quantity: Option<i64>, flag: Option<i64> },
    Merge(Vec<(i64, i64, Option<i64>)>),
    StackAll { flag: Option<i64> },
    ReplaceCharges { flag: Option<i64>, item: i64, quantity: Option<i64> },
    Assemble(Vec<i64>),
    DestroyFitting(i64),
}

/// Run `op` in one item transaction and report the result to the client.
async fn run(ctx: &Ctx<'_>, container: Container, op: Op) -> Outcome {
    let _trade_guard=crate::svc::pvptrade::inventory_guard(ctx.world,ctx.session.char_id.unwrap_or(0)).await;
    // The loadout before any fitting change, to report what it did.
    let mut fitting_before: Option<Snapshot> = None;
    let mut maintenance=super::maintenance::InventoryWrite::default();
    let result: Result<(Vec<Value>, Vec<i64>), MoveError> = async {
        let mut client = crate::db::connect(ctx.db_url).await?;
        let mut txn = ItemTxn::begin(&mut client).await?;
        // Arrival/boarding takes this lock before any hull row. Keep the same
        // order even when maintenance custody is discovered through a source
        // item later, rather than waiting for lifecycle while holding its hull.
        let Some(pilot)=ctx.session.char_id.and_then(|id|i32::try_from(id).ok()).filter(|id|*id>0) else{return refuse("No active character.");};
        if !super::login::lock_character_lifecycle(txn.tx(),pilot).await?{return refuse("That character is unavailable.");}
        apply(ctx, &mut txn, container, op, &mut fitting_before,&mut maintenance).await?;
        maintenance.validate(ctx)?;
        let origins = txn.origins();
        Ok((txn.commit().await?, origins))
    }
    .await;
    match result {
        Ok((events, origins)) => {
            maintenance.recipients.insert(ctx.client_id);
            {let w=ctx.world.lock();let recipients:Vec<_>=maintenance.recipients.iter().copied().collect();for event in events{w.notify(&recipients,"OnItemChange",event);}}
            // Keep host reservations through accepted custody and notifications.
            drop(maintenance);
            // Taking from another pilot's can is theft; taking the last thing
            // out of a can in space ends the can.
            if let Some(system) = ctx.session.solar_system_id {
                let corp_id = ctx.session.corp_id.unwrap_or(0);
                crate::concord::took_from(ctx.db, ctx.world, system, me(ctx), corp_id, &origins).await;
                crate::loot::clear_emptied(ctx.db, ctx.world, system, &origins).await;
            }
            crate::svc::pvptrade::offers_changed(ctx.world, &origins);
            if let Some(before) = fitting_before {
                fitting::push_attribute_changes(ctx, &before).await;
            }
            Outcome::Value(Value::None)
        }
        Err(MoveError::Refused(why)) => {
            info!("move refused: {why}");
            Outcome::custom(why)
        }
        Err(MoveError::Db(e)) => {
            error!("item move: {}", crate::logging::chain(&e));
            Outcome::custom("The items could not be moved.")
        }
        Err(MoveError::Reactivation{item,delay})=>reactivation_error(item,delay),
    }
}

fn me(ctx: &Ctx<'_>) -> i64 {
    ctx.session.char_id.unwrap_or(0)
}

fn new_like(item: &Item) -> NewItem {
    NewItem {
        type_id: item.type_id,
        owner_id: item.owner_id,
        location_id: item.location_id,
        flag: item.flag,
        quantity: item.quantity,
        singleton: item.singleton,
    }
}

/// Where things put into a container go.
struct Destination {
    location: i64,
    owner: i64,
    flag: i64,
    /// m3, or None for unlimited.
    capacity: Option<f64>,
    /// The ship, when this is one of its slots.
    fitting: Option<Item>,
    bay: Option<super::maintenance::Bay>,
}

async fn apply(
    ctx: &Ctx<'_>,
    txn: &mut ItemTxn<'_>,
    container: Container,
    op: Op,
    fitting_before: &mut Option<Snapshot>,
    maintenance:&mut super::maintenance::InventoryWrite,
) -> Result<(), MoveError> {
    match op {
        Op::Add { items, quantity, flag } => {
            if items.is_empty() || items.len()>100 || items.iter().copied().collect::<std::collections::BTreeSet<_>>().len()!=items.len(){return refuse("Select between one and one hundred distinct items.");}
            if items.iter().any(|id|!(1..=i64::from(i32::MAX)).contains(id)){return refuse("Invalid item identifier.");}
            let dest = destination(ctx, txn, container, flag,maintenance).await?;
            for id in items {
                add(ctx, txn, &dest, id, quantity, fitting_before,maintenance).await?;
            }
        }
        Op::Merge(ops) => {
            if ops.is_empty() || ops.len()>100{return refuse("Select between one and one hundred stack merges.");}
            if ops.iter().any(|(a,b,_)|!(1..=i64::from(i32::MAX)).contains(a)||!(1..=i64::from(i32::MAX)).contains(b)){return refuse("Invalid stack identifier.");}
            let mut locked=std::collections::BTreeSet::new();for (a,b,_) in &ops{locked.insert(*a);locked.insert(*b);}
            if let Container::Item(id)=container{locked.insert(id);}
            for id in locked{txn.item(id).await?;}
            for (source, target, quantity) in ops {
                // MergeItems has no flag argument; use the target's actual
                // bay/division, then recheck it under its row lock in merge.
                let Some(row) = txn.item(target).await? else {
                    return refuse("That item no longer exists.");
                };
                let dest = destination(ctx, txn, container, Some(row.flag),maintenance).await?;
                merge(ctx, txn, &dest, source, target, quantity,maintenance).await?;
            }
        }
        Op::StackAll { flag } => {
            let dest = destination(ctx, txn, container, flag,maintenance).await?;
            stack_all(ctx, txn, &dest, flag.is_some(),maintenance).await?;
        }
        Op::ReplaceCharges { flag, item, quantity } => {
            let dest = destination(ctx, txn, container, flag,maintenance).await?;
            let Some(ship) = &dest.fitting else { return refuse("Charges can only be loaded into a module.") };
            if fitting_before.is_none() {
                *fitting_before = Some(fitting::snapshot(txn.tx(), ctx.dogma, me(ctx), ctx.session.ship_id).await?);
            }
            // The old charges go back to the hold, then the new ones load as a reload would.
            fitting::unload(txn, ship.id, dest.flag, ship.id, FLAG_CARGO).await?;
            add(ctx, txn, &dest, item, quantity, fitting_before,maintenance).await?;
        }
        Op::Assemble(ids) => assemble(ctx, txn, &ids).await?,
        Op::DestroyFitting(id) => {
            let Container::Item(ship_id) = container else { return refuse("Open your ship's fitting first.") };
            if ctx.session.station_id.is_none() || ctx.session.ship_id != Some(ship_id) {
                return refuse("Dock in your active ship before removing a rig.");
            }
            let Some(item) = txn.item(id).await? else { return refuse("That rig no longer exists.") };
            super::maintenance::pending_mutation(ctx,txn,&item).await?;
            if item.owner_id != me(ctx) || item.location_id != ship_id || !(92..=99).contains(&item.flag)
                || item.category_id != category::MODULE
            {
                return refuse("That is not a rig fitted to your ship.");
            }
            *fitting_before = Some(fitting::snapshot(txn.tx(), ctx.dogma, me(ctx), Some(ship_id)).await?);
            txn.destroy(id).await?;
        }
    }
    Ok(())
}

async fn assemble(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_>, ids: &[i64]) -> Result<(), MoveError> {
    let Some(station) = ctx.session.station_id else { return refuse("You must be docked.") };
    for item in txn.items(ids).await? {
        super::maintenance::pending_mutation(ctx,txn,&item).await?;
        if item.category_id != CATEGORY_SHIP || item.singleton {
            continue;
        }
        if item.owner_id != me(ctx) || item.location_id != station {
            return refuse("You can only assemble your own ships in your hangar.");
        }
        if item.quantity > 1 {
            txn.set_quantity(item.id, item.quantity - 1).await?;
            txn.create(NewItem { quantity: 1, singleton: true, ..new_like(&item) }).await?;
        } else {
            txn.set_singleton(item.id, true).await?;
        }
    }
    Ok(())
}

async fn destination(
    ctx: &Ctx<'_>,
    txn: &mut ItemTxn<'_>,
    container: Container,
    flag: Option<i64>,
    maintenance:&mut super::maintenance::InventoryWrite,
) -> Result<Destination, MoveError> {
    match container {
        Container::Special { container_id: CONTAINER_HANGAR, owner_id } => {
            let Some(station) = ctx.session.station_id else { return refuse("You must be docked to use the hangar.") };
            if owner_id.is_some_and(|o| o != me(ctx)) {
                return refuse("You cannot move items into that hangar.");
            }
            Ok(Destination { location: station, owner: me(ctx), flag: FLAG_HANGAR as i64, capacity: None, fitting: None,bay:None })
        }
        Container::Special { .. } => refuse("Items cannot be moved there."),
        Container::Item(id) => {
            let Some(holder) = txn.item(id).await? else { return refuse("That container no longer exists.") };
            super::maintenance::pending_mutation(ctx,txn,&holder).await?;
            if let Some(bay)=maintenance.destination(ctx,txn,&holder,flag,crate::svc::starbase::Access::ViewPut).await? {
                return Ok(Destination{location:id,owner:me(ctx),flag:bay.flag,capacity:Some(bay.capacity),fitting:None,bay:Some(bay)});
            }
            if !reachable(ctx, txn, &holder).await? {
                return refuse("That container is out of reach.");
            }
            // A corporation office's divisions hold as much as anyone puts in.
            if let Some((corp, _)) = crate::svc::corphangar::office(txn.tx(), id).await {
                let flag = flag.unwrap_or(crate::svc::corphangar::FLAG_HANGAR);
                if crate::svc::corphangar::division(flag).is_none() {
                    return refuse("Items go into one of the office's divisions.");
                }
                return Ok(Destination { location: id, owner: corp, flag, capacity: None, fitting: None,bay:None });
            }
            // The holder is locked and its custody has been checked. Deposits
            // into corporation-owned containers remain corporation property.
            let owner = if Some(holder.owner_id) == ctx.session.corp_id { holder.owner_id } else { me(ctx) };
            if holder.category_id == CATEGORY_SHIP {
                let flag = flag.unwrap_or(FLAG_CARGO);
                if fitting::is_fitting_flag(flag) {
                    return Ok(Destination { location: id, owner, flag, capacity: None, fitting: Some(holder),bay:None });
                }
                let capacity = match flag {
                    FLAG_CARGO => type_capacity(txn, holder.type_id).await?,
                    FLAG_DRONE_BAY => drone_capacity(txn, holder.type_id).await?,
                    _ => return refuse("Items cannot be put there."),
                };
                Ok(Destination { location: id, owner, flag, capacity: Some(capacity), fitting: None,bay:None })
            } else {
                if !holder.singleton {
                    return refuse("Assemble the container before putting things in it.");
                }
                let capacity = match crate::svc::moonmining::silo_capacity(txn.tx(), holder.id).await? {
                    Some(silo) => silo,
                    None => type_capacity(txn, holder.type_id).await?,
                };
                if capacity <= 0.0 {
                    return refuse("That item cannot hold anything.");
                }
                Ok(Destination { location: id, owner, flag: flag.unwrap_or(0), capacity: Some(capacity), fitting: None,bay:None })
            }
        }
    }
}

async fn type_capacity(txn: &ItemTxn<'_>, type_id: i64) -> DbResult<f64> {
    let r = txn
        .tx()
        .query_opt(r#"SELECT "capacity" FROM "invTypes" WHERE "typeID" = $1"#, &[&(type_id as i32)])
        .await?;
    Ok(r.and_then(|r| r.get::<_, Option<f64>>(0)).unwrap_or(0.0))
}

async fn drone_capacity(txn: &ItemTxn<'_>, type_id: i64) -> DbResult<f64> {
    let r = txn
        .tx()
        .query_opt(
            r#"SELECT coalesce("valueFloat", "valueInt"::float8) FROM "dgmTypeAttributes"
                WHERE "typeID" = $1 AND "attributeID" = $2"#,
            &[&(type_id as i32), &ATTR_DRONE_CAPACITY],
        )
        .await?;
    Ok(r.and_then(|r| r.get::<_, Option<f64>>(0)).unwrap_or(0.0))
}

/// Volume already in a destination, leaving out `except`.
async fn used(txn: &ItemTxn<'_>, dest: &Destination, except: i64) -> DbResult<f64> {
    let rows = txn
        .tx()
        .query(
            r#"SELECT e."typeID",t."groupID",e.singleton,e.quantity,COALESCE(t.volume,0)::float8,
                       CASE WHEN e."typeID" = $4
                                        THEN (SELECT coalesce(sum(ct."volume" * c."quantity"), 0) FROM entity c
                                                JOIN "invTypes" ct ON ct."typeID" = c."typeID" WHERE c."locationID" = e."itemID")
                                        ELSE NULL END::float8
                 FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                WHERE e."locationID" = $1 AND e."flag" = $2 AND e."itemID" <> $3"#,
            &[&(dest.location as i32), &(dest.flag as i32), &(except as i32), &(crate::svc::missions::TYPE_PLASTIC_WRAP as i32)],
        )
        .await?;
    Ok(rows.iter().map(|r|r.get::<_,Option<f64>>(5).unwrap_or_else(||super::maintenance::unit_volume(i64::from(r.get::<_,i32>(0)),r.get(1),r.get::<_,i32>(2)!=0,r.get(4))*f64::from(r.get::<_,i32>(3)))).sum())
}

async fn check_room(txn: &ItemTxn<'_>, dest: &Destination, item: &Item, quantity: i64) -> Result<(), MoveError> {
    let Some(capacity) = dest.capacity else { return Ok(()) };
    let free = capacity - used(txn, dest, item.id).await?;
    // A courier package takes the room of what is wrapped in it.
    let needed = if item.type_id == crate::svc::missions::TYPE_PLASTIC_WRAP {
        crate::svc::missions::package_volume(txn.tx(), item.id).await?
    } else {
        let group:i32=txn.tx().query_one(r#"SELECT "groupID" FROM "invTypes" WHERE "typeID"=$1"#,&[&(item.type_id as i32)]).await?.get(0);
        super::maintenance::unit_volume(item.type_id,group,item.singleton,item.volume)*quantity as f64
    };
    if needed > free + 1e-6 {
        return refuse(format!("Not enough room: that needs {needed:.2} m3 and only {free:.2} m3 is free."));
    }
    Ok(())
}

/// Whether `item` sits in a can floating within reach of the caller's ship.
fn in_reachable_can(ctx: &Ctx<'_>, item: &Item) -> bool {
    match (ctx.session.solar_system_id, ctx.session.ship_id) {
        (Some(system), Some(ship)) => crate::loot::in_reach(ctx.world, system, ship, item.location_id),
        _ => false,
    }
}

/// Whether the caller can take `item`: its own, and here; or anything in a
/// can it is close to.
pub(crate) async fn reachable(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_>, item: &Item) -> DbResult<bool> {
    // Skills and installed implants/boosters belong to the character (or a
    // stored clone), not to ordinary cargo. Their own services manage them.
    if matches!(item.flag, 7 | 36 | 61 | 88 | 89) {
        return Ok(false);
    }
    if in_reachable_can(ctx, item) {
        return Ok(true);
    }
    // One's own offer in a trade, taken back out.
    if crate::svc::pvptrade::my_offer(ctx, item.owner_id, item.location_id) {
        return Ok(true);
    }
    // A structure within reach (to put into), and what it holds (to take),
    // as its usage settings allow.
    use crate::svc::starbase::{may, Access};
    if let Some(ok) = may(ctx, item.id, Access::ViewPut).await {
        return Ok(ok);
    }
    if let Some(ok) = may(ctx, item.location_id, Access::Take).await {
        return Ok(ok);
    }
    // A corporation office: open to its members there, and what is in its
    // divisions to those whose roles let them take it.
    if let Some(office) = crate::svc::corphangar::office(txn.tx(), item.id).await {
        return Ok(crate::svc::corphangar::mine(ctx, office));
    }
    if let Some(office) = crate::svc::corphangar::office(txn.tx(), item.location_id).await {
        return Ok(crate::svc::corphangar::mine(ctx, office) && crate::svc::corphangar::may_take(ctx, item.flag));
    }
    if Some(item.owner_id) == ctx.session.corp_id {
        // Nested cargo inherits the outer office division's permissions. Lock
        // every ancestor so a concurrent handover cannot change this custody.
        let mut current = item.clone();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            if !seen.insert(current.id) || current.owner_id != item.owner_id || matches!(current.flag, 7 | 36 | 61 | 88 | 89) {
                return Ok(false);
            }
            if let Some(office) = crate::svc::corphangar::office(txn.tx(), current.location_id).await {
                return Ok(crate::svc::corphangar::mine(ctx, office) && crate::svc::corphangar::may_take(ctx, current.flag));
            }
            let Some(parent) = txn.item(current.location_id).await? else { return Ok(false) };
            current = parent;
        }
        return Ok(false);
    }
    if item.owner_id != me(ctx) {
        return Ok(false);
    }
    let here = match (ctx.session.station_id, ctx.session.ship_id) {
        (Some(station), _) => station,
        (None, Some(ship)) => {
            if item.id == ship || item.location_id == ship {
                return Ok(true);
            }
            ship
        }
        (None, None) => return Ok(false),
    };
    // Up to two containers deep: a can in a ship's hold in the station.
    let mut location = item.location_id;
    for _ in 0..3 {
        if location == here {
            return Ok(true);
        }
        match txn.item(location).await? {
            Some(parent) if parent.owner_id == me(ctx) => location = parent.location_id,
            _ => return Ok(false),
        }
    }
    Ok(false)
}

async fn add(
    ctx: &Ctx<'_>,
    txn: &mut ItemTxn<'_>,
    dest: &Destination,
    id: i64,
    quantity: Option<i64>,
    fitting_before: &mut Option<Snapshot>,
    maintenance:&mut super::maintenance::InventoryWrite,
) -> Result<(), MoveError> {
    let Some(item) = txn.item(id).await? else { return refuse("That item no longer exists.") };
    let source_bay=maintenance.source(ctx,txn,&item).await?;
    if source_bay.is_none() && !reachable(ctx, txn, &item).await? {
        return refuse("That item is out of reach.");
    }
    let quantity = quantity.unwrap_or(item.quantity);
    if quantity<=0 || quantity>item.quantity || (item.singleton && quantity!=1){return refuse("The requested quantity is not available.");}
    if item.location_id == dest.location && item.flag == dest.flag && quantity==item.quantity {
        if let Some(bay)=dest.bay{maintenance.accepts(ctx,txn,bay,&item,item.quantity).await?;}
        return Ok(());
    }
    if fitting::is_slot(item.flag) {
        let now=crate::combat::now_ms();
        let live=crate::cloak::reactivation_remaining(&ctx.world.lock(),item.id,now);
        let durable=crate::cloak::durability::module_reactivation_remaining(txn.tx(),&item,now).await?;
        if let Some(delay)=live.into_iter().chain(durable).max(){return Err(MoveError::Reactivation{item:item.id,delay});}
    }
    if item.id == dest.location {
        return refuse("An item cannot be put inside itself.");
    }
    if item.singleton {
        let mut parent = dest.location;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            if parent == item.id || !seen.insert(parent) {
                return refuse("A container cannot be put inside its own contents.");
            }
            let Some(holder) = txn.item(parent).await? else { break };
            parent = holder.location_id;
        }
    }
    if item.flag == 7 || item.id == me(ctx) {
        return refuse("That cannot be moved.");
    }
    if crate::svc::votes::locked(txn.tx(), item.id).await {
        return refuse("That item is locked down by a corporation vote.");
    }
    if Some(item.id) == ctx.session.ship_id && dest.capacity.is_some() {
        return refuse("You cannot put your active ship in a container.");
    }
    if item.category_id == CATEGORY_SHIP && item.singleton && dest.capacity.is_some() && dest.bay.is_none() {
        return refuse("Assembled ships cannot be put in a cargo hold; repackage it first.");
    }
    if (fitting::is_slot(item.flag) || dest.fitting.is_some()) && fitting_before.is_none() {
        *fitting_before = Some(fitting::snapshot(txn.tx(), ctx.dogma, me(ctx), ctx.session.ship_id).await?);
    }
    if let Some(ship) = &dest.fitting {
        return fitting::fit(ctx, txn, ship, &item, dest.flag, Some(quantity)).await;
    }
    if fitting::is_slot(item.flag) && item.category_id == category::MODULE {
        if ctx.session.station_id.is_none() {
            return refuse("Modules can only be unfitted while docked.");
        }
        fitting::unfit(txn, &item, dest.location, dest.flag).await?;
    }
    if let Some(why) = crate::svc::moonmining::refuses(txn.tx(), dest.location, item.id, item.type_id).await? {
        return refuse(why);
    }
    if let Some(bay)=dest.bay {maintenance.accepts(ctx,txn,bay,&item,quantity).await?;}
    else{check_room(txn, dest, &item, quantity).await?;}
    // Loot taken from a can becomes the taker's; an office or its nested
    // containers hold corporation property, including partial stacks.
    let owner = if dest.bay.is_some(){item.owner_id}else{dest.owner};
    if source_bay.is_some() && owner!=item.owner_id{return refuse("Moving a stored ship does not transfer its ownership.");}
    if quantity==item.quantity && owner!=item.owner_id {super::maintenance::pending_tree_mutation(ctx,txn,item.id).await?;}
    if quantity < item.quantity {
        txn.set_quantity(item.id, item.quantity - quantity).await?;
        let new = NewItem { location_id: dest.location, flag: dest.flag, quantity, singleton: false, owner_id: owner, ..new_like(&item) };
        txn.create(new).await?;
    } else {
        txn.move_to(item.id, dest.location, dest.flag).await?;
        if item.owner_id != owner {
            if !txn.transfer_tree(item.id,item.owner_id,owner).await? {
                return refuse("That container holds property which cannot be transferred. Active ships, installed items and another owner's property cannot be handed over.");
            }
        }
    }
    Ok(())
}

async fn merge(
    ctx: &Ctx<'_>,
    txn: &mut ItemTxn<'_>,
    dest: &Destination,
    source: i64,
    target: i64,
    quantity: Option<i64>,
    maintenance:&mut super::maintenance::InventoryWrite,
) -> Result<(), MoveError> {
    let (Some(from), Some(onto)) = (txn.item(source).await?, txn.item(target).await?) else {
        return refuse("That item no longer exists.");
    };
    let from_bay=maintenance.source(ctx,txn,&from).await?;
    let onto_bay=maintenance.source(ctx,txn,&onto).await?;
    if (from_bay.is_none() && !reachable(ctx, txn, &from).await?) || (onto_bay.is_none() && !reachable(ctx, txn, &onto).await?) {
        return refuse("That item is out of reach.");
    }
    if onto.location_id != dest.location || onto.flag != dest.flag || (dest.bay.is_none() && onto.owner_id != dest.owner) {
        return refuse("The destination stack is not in that container.");
    }
    if from.id == onto.id {
        return Ok(());
    }
    if from.type_id != onto.type_id || from.singleton || onto.singleton {
        return refuse("Only stacks of the same kind can be merged.");
    }
    if (from_bay.is_some() || onto_bay.is_some() || dest.bay.is_some()) && from.owner_id!=onto.owner_id{return refuse("Stored ship stacks belonging to different owners cannot be merged.");}
    let quantity = quantity.unwrap_or(from.quantity);
    if quantity<=0 || quantity>from.quantity{return refuse("The requested quantity is not available.");}
    if let Some(bay)=dest.bay{
        maintenance.accepts(ctx,txn,bay,&from,quantity).await?;
        maintenance.accepts(ctx,txn,bay,&onto,onto.quantity).await?;
    }
    if from.location_id != onto.location_id || from.flag != onto.flag {
        if dest.bay.is_none(){check_room(txn, dest, &from, quantity).await?;}
    }
    let total=onto.quantity.checked_add(quantity).filter(|n|*n<=i64::from(i32::MAX)).ok_or_else(||MoveError::Refused("The resulting stack is too large.".into()))?;
    txn.set_quantity(onto.id, total).await?;
    txn.set_quantity(from.id, from.quantity - quantity).await?;
    Ok(())
}

/// Merge every stack of a type in the destination into its oldest stack.
async fn stack_all(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_>, dest: &Destination, one_flag: bool,maintenance:&mut super::maintenance::InventoryWrite) -> Result<(), MoveError> {
    let mut owners=vec![dest.owner as i32];
    if dest.bay.is_some_and(|b|b.flag==0){if let Some(corp)=ctx.session.corp_id{owners.push(corp as i32);}}
    let rows = txn
        .tx()
        .query(
            r#"SELECT "itemID" FROM entity
                WHERE "locationID" = $1 AND "ownerID" = ANY($2) AND "singleton" = 0
                  AND ($3 = false OR "flag" = $4)
                ORDER BY "typeID", "flag", "ownerID", "itemID""#,
            &[&(dest.location as i32), &owners, &one_flag, &(dest.flag as i32)],
        )
        .await?;
    let ids: Vec<i64> = rows.iter().map(|r| r.get::<_, i32>(0) as i64).collect();
    let mut keep: Option<Item> = None;
    for item in txn.items(&ids).await? {
        if !owners.contains(&(item.owner_id as i32)) || item.location_id != dest.location || item.singleton || (one_flag && item.flag != dest.flag) {
            continue;
        }
        let bay=maintenance.source(ctx,txn,&item).await?;
        if let Some(bay)=bay{maintenance.accepts(ctx,txn,bay,&item,item.quantity).await?;}
        else if !reachable(ctx,txn,&item).await?{continue;}
        match &mut keep {
            Some(k) if k.type_id == item.type_id && k.flag == item.flag && k.owner_id==item.owner_id => {
                k.quantity=k.quantity.checked_add(item.quantity).filter(|n|*n<=i64::from(i32::MAX)).ok_or_else(||MoveError::Refused("The resulting stack is too large.".into()))?;
                txn.set_quantity(k.id, k.quantity).await?;
                txn.destroy(item.id).await?;
            }
            _ => keep = Some(item),
        }
    }
    Ok(())
}

#[cfg(test)]
mod custody_tests {
    use super::*;

    fn success(result:&Outcome)->bool { matches!(result,Outcome::Value(Value::None)) }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn nested_corporation_deposits_withdrawals_and_merges_inherit_locked_custody() {
        let db=crate::test_support::Database::load().await;
        let owner=db.id().await;let corp=db.id().await;let office=db.id().await;
        let outer=db.id().await;let inner=db.id().await;let ore=db.id().await;let existing=db.id().await;
        let box_id=db.id().await;let contents=db.id().await;let other=db.id().await;
        let division_stack=db.id().await;let restricted_a=db.id().await;let restricted_b=db.id().await;
        let station:i32=db.db.query_one(r#"SELECT "stationID" FROM "staStations" ORDER BY "stationID" LIMIT 1"#,&[]).await.unwrap().get(0);
        db.db.execute(r#"INSERT INTO corporation ("corporationID","corporationName") VALUES($1,$2)"#,&[&corp,&format!("Nested deposit {corp}")]).await.unwrap();
        db.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","corporationID","stationID") VALUES($1,$2,$3,$4)"#,&[&owner,&format!("Nested depositor {owner}"),&corp,&station]).await.unwrap();
        db.db.execute(r#"INSERT INTO "crpOffices" ("corporationID","stationID","officeFolderID","itemID") VALUES($1,$2,$3,$3)"#,&[&corp,&station,&office]).await.unwrap();
        for (id,ty,who,location,flag,quantity,singleton) in [
            (office,27,corp,station,0,1,1),(outer,3293,corp,office,116,1,1),(inner,3293,corp,outer,0,1,1),
            (ore,34,owner,station,4,100,0),(existing,34,corp,inner,0,2,0),
            (box_id,3293,owner,station,4,1,1),(contents,34,owner,box_id,0,3,0),(other,34,owner,station,4,1,0),
            (division_stack,34,corp,office,116,5,0),(restricted_a,34,corp,office,4,2,0),(restricted_b,34,corp,office,4,3,0),
        ] {
            db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,$2,$3,$4,$5,$6,$7)"#,&[&id,&ty,&who,&location,&flag,&quantity,&singleton]).await.unwrap();
        }
        // Only division two, not director: inner flags must not select roles.
        let session=crate::session::Session {char_id:Some(owner as i64),corp_id:Some(corp as i64),station_id:Some(station as i64),roles_at_all:16_384|2_097_152,..Default::default()};
        let ctx=db.ctx(&session);
        let nested=Container::Item(inner as i64);
        let hangar=Container::Special {container_id:CONTAINER_HANGAR,owner_id:None};
        let add=|id:i32,quantity|Op::Add {items:vec![id as i64],quantity,flag:None};
        assert!(success(&run(&ctx,nested,add(ore,Some(7))).await));
        let deposited:i32=db.db.query_one(r#"SELECT "itemID" FROM entity WHERE "locationID"=$1 AND "itemID"<>$2"#,&[&inner,&existing]).await.unwrap().get(0);
        let row=db.db.query_one(r#"SELECT "ownerID",quantity FROM entity WHERE "itemID"=$1"#,&[&deposited]).await.unwrap();
        assert_eq!((row.get::<_,i32>(0),row.get::<_,i32>(1)),(corp,7));
        assert_eq!(db.db.query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#,&[&ore]).await.unwrap().get::<_,i32>(0),93);
        assert!(success(&run(&ctx,nested,add(box_id,None)).await));
        assert!(db.db.query(r#"SELECT "ownerID" FROM entity WHERE "itemID"=ANY($1)"#,&[&vec![box_id,contents]]).await.unwrap().iter().all(|r|r.get::<_,i32>(0)==corp));
        assert!(success(&run(&ctx,hangar,add(box_id,None)).await));
        assert!(db.db.query(r#"SELECT "ownerID" FROM entity WHERE "itemID"=ANY($1)"#,&[&vec![box_id,contents]]).await.unwrap().iter().all(|r|r.get::<_,i32>(0)==owner));

        let no_take=crate::session::Session {roles_at_all:2_097_152,..session.clone()};
        assert!(!success(&run(&db.ctx(&no_take),hangar,add(deposited,None)).await));
        let wrong_division=crate::session::Session {roles_at_all:8_192|2_097_152,..session.clone()};
        assert!(!success(&run(&db.ctx(&wrong_division),nested,add(ore,Some(1))).await));
        assert!(success(&run(&ctx,hangar,add(deposited,Some(3))).await));
        assert_eq!(db.db.query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#,&[&deposited]).await.unwrap().get::<_,i32>(0),4);
        assert!(success(&run(&ctx,nested,Op::StackAll{flag:None}).await));
        let row=db.db.query_one(r#"SELECT "ownerID",quantity FROM entity WHERE "locationID"=$1"#,&[&inner]).await.unwrap();
        assert_eq!((row.get::<_,i32>(0),row.get::<_,i32>(1)),(corp,6));
        assert!(!success(&run(&ctx,nested,Op::Merge(vec![(existing as i64,other as i64,Some(1))])).await),"a target outside the bound container is refused");
        assert!(success(&run(&ctx,nested,Op::Merge(vec![(ore as i64,existing as i64,Some(1))])).await));
        assert_eq!(db.db.query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#,&[&existing]).await.unwrap().get::<_,i32>(0),7);
        assert!(success(&run(&ctx,Container::Item(office as i64),Op::Merge(vec![(ore as i64,division_stack as i64,Some(1))])).await),"MergeItems derives division two from the target despite lacking a flag argument");
        assert_eq!(db.db.query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#,&[&division_stack]).await.unwrap().get::<_,i32>(0),6);
        assert!(success(&run(&ctx,Container::Item(office as i64),Op::StackAll{flag:None}).await));
        assert_eq!(db.db.query_one(r#"SELECT count(*) FROM entity WHERE "itemID"=ANY($1)"#,&[&vec![restricted_a,restricted_b]]).await.unwrap().get::<_,i64>(0),2,"StackAll cannot mutate another division");
        assert!(!success(&run(&ctx,nested,add(outer,None)).await),"an ancestor cannot become its own descendant");

        // Destination owner is read after its lock is acquired, rather than
        // inherited from a pre-wait container snapshot.
        let mut changing_db=crate::db::connect(&db.url).await.unwrap();
        let blocker:i32=changing_db.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
        let mut changing=ItemTxn::begin(&mut changing_db).await.unwrap();
        changing.set_owner(inner as i64,corp as i64+1000).await.unwrap();
        let release=async {
            tokio::time::timeout(std::time::Duration::from_secs(10),async {
                loop {
                    if db.db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity a WHERE $1=ANY(pg_blocking_pids(a.pid)))",&[&blocker]).await.unwrap().get::<_,bool>(0) {break;}
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }).await.expect("deposit waits for destination custody");
            changing.commit().await.unwrap();
        };
        let (result,())=tokio::join!(run(&ctx,nested,add(ore,Some(1))),release);
        assert!(!success(&result));
        let row=db.db.query_one(r#"SELECT "ownerID","locationID",quantity FROM entity WHERE "itemID"=$1"#,&[&ore]).await.unwrap();
        assert_eq!((row.get::<_,i32>(0),row.get::<_,i32>(1),row.get::<_,i32>(2)),(owner,station,91));
    }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn corporation_hull_and_nested_cargo_transfer_is_atomic_and_revalidates_waiting_children() {
        let db=crate::test_support::Database::load().await;
        let owner=db.id().await;let corp=db.id().await;let office=db.id().await;
        let active=db.id().await;let hull=db.id().await;let fitting=db.id().await;
        let container=db.id().await;let cargo=db.id().await;
        let station:i32=db.db.query_one(r#"SELECT "stationID" FROM "staStations" ORDER BY "stationID" LIMIT 1"#,&[]).await.unwrap().get(0);
        let module:i32=db.db.query_one(r#"SELECT t."typeID" FROM "invTypes" t JOIN "invGroups" g ON g."groupID"=t."groupID" WHERE g."categoryID"=7 AND t.published=1 ORDER BY t."typeID" LIMIT 1"#,&[]).await.unwrap().get(0);
        db.db.execute(r#"INSERT INTO corporation ("corporationID","corporationName") VALUES ($1,$2)"#,&[&corp,&format!("Nested custody {corp}")]).await.unwrap();
        db.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","corporationID","shipID","stationID") VALUES ($1,$2,$3,$4,$5)"#,&[&owner,&format!("Nested owner {owner}"),&corp,&active,&station]).await.unwrap();
        db.db.execute(r#"INSERT INTO "crpOffices" ("corporationID","stationID","officeFolderID","itemID") VALUES ($1,$2,$3,$3)"#,&[&corp,&station,&office]).await.unwrap();
        for (id,ty,who,location,flag,quantity) in [(office,27,corp,station,0,1),(active,587,owner,station,4,1),(hull,587,owner,station,4,1),(fitting,module,owner,hull,11,1),(container,3293,owner,hull,5,1),(cargo,34,owner,container,5,42)] {
            db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,$2,$3,$4,$5,$6,1)"#,&[&id,&ty,&who,&location,&flag,&quantity]).await.unwrap();
        }
        db.db.execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt") VALUES ($1,3,17)"#,&[&fitting]).await.unwrap();
        let session=crate::session::Session {char_id:Some(owner as i64),corp_id:Some(corp as i64),ship_id:Some(active as i64),station_id:Some(station as i64),roles_at_all:crate::session::CORP_ROLE_DIRECTOR,..Default::default()};
        let ctx=db.ctx(&session);
        let deposit=||Op::Add {items:vec![hull as i64],quantity:None,flag:Some(4)};
        let hangar=Container::Special {container_id:CONTAINER_HANGAR,owner_id:None};
        assert!(success(&run(&ctx,Container::Item(office as i64),deposit()).await));
        let ids=vec![hull,fitting,container,cargo];
        let held=db.db.query(r#"SELECT "ownerID" FROM entity WHERE "itemID"=ANY($1)"#,&[&ids]).await.unwrap();
        assert_eq!(held.len(),4);assert!(held.iter().all(|r|r.get::<_,i32>(0)==corp));
        assert!(success(&run(&ctx,hangar,deposit()).await));
        assert!(db.db.query(r#"SELECT "ownerID" FROM entity WHERE "itemID"=ANY($1)"#,&[&ids]).await.unwrap().iter().all(|r|r.get::<_,i32>(0)==owner));
        assert_eq!(db.db.query_one(r#"SELECT "valueInt" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3"#,&[&fitting]).await.unwrap().get::<_,i64>(0),17);

        // A rejected child write rolls back even the parent already moved.
        let constraint=format!("nested_custody_failure_{owner}");
        db.db.batch_execute(&format!(r#"ALTER TABLE entity ADD CONSTRAINT {constraint} CHECK ("itemID"<>{cargo} OR "ownerID"<>{corp}) NOT VALID"#)).await.unwrap();
        let failed=run(&ctx,Container::Item(office as i64),deposit()).await;
        db.db.batch_execute(&format!("ALTER TABLE entity DROP CONSTRAINT {constraint}")).await.unwrap();
        assert!(!success(&failed));
        let original=db.db.query_one(r#"SELECT "ownerID","locationID" FROM entity WHERE "itemID"=$1"#,&[&hull]).await.unwrap();
        assert_eq!((original.get::<_,i32>(0),original.get::<_,i32>(1)),(owner,station));
        assert!(db.db.query(r#"SELECT "ownerID" FROM entity WHERE "itemID"=ANY($1)"#,&[&ids]).await.unwrap().iter().all(|r|r.get::<_,i32>(0)==owner));

        // Somebody's active hull cannot be handed over as ordinary property.
        db.db.execute(r#"UPDATE "character_" SET "shipID"=$2 WHERE "characterID"=$1"#,&[&owner,&hull]).await.unwrap();
        assert!(!success(&run(&ctx,Container::Item(office as i64),deposit()).await));
        db.db.execute(r#"UPDATE "character_" SET "shipID"=$2 WHERE "characterID"=$1"#,&[&owner,&active]).await.unwrap();

        let mut moving_db=crate::db::connect(&db.url).await.unwrap();
        let blocker:i32=moving_db.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
        let mut moving=ItemTxn::begin(&mut moving_db).await.unwrap();
        moving.move_to(container as i64,station as i64,4).await.unwrap();
        let release=async {
            tokio::time::timeout(std::time::Duration::from_secs(10),async {
                loop {
                    if db.db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity a WHERE $1=ANY(pg_blocking_pids(a.pid)))",&[&blocker]).await.unwrap().get::<_,bool>(0) {break;}
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }).await.expect("handover waits on the moving child");
            moving.commit().await.unwrap();
        };
        let (result,())=tokio::join!(run(&ctx,Container::Item(office as i64),deposit()),release);
        assert!(success(&result));
        assert_eq!(db.db.query_one(r#"SELECT "ownerID" FROM entity WHERE "itemID"=$1"#,&[&hull]).await.unwrap().get::<_,i32>(0),corp);
        let retained=db.db.query(r#"SELECT "ownerID" FROM entity WHERE "itemID"=ANY($1)"#,&[&vec![container,cargo]]).await.unwrap();
        assert!(retained.iter().all(|r|r.get::<_,i32>(0)==owner),"the child and its subtree moved away before handover acquired their lock");

        // Even while still attached, a child whose owner changed during the
        // wait must never be stolen based on the original traversal snapshot.
        assert!(success(&run(&ctx,hangar,deposit()).await));
        db.db.execute(r#"UPDATE entity SET "locationID"=$2,flag=5 WHERE "itemID"=$1"#,&[&container,&hull]).await.unwrap();
        let mut changing_db=crate::db::connect(&db.url).await.unwrap();
        let blocker:i32=changing_db.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
        let mut changing=ItemTxn::begin(&mut changing_db).await.unwrap();
        changing.set_owner(container as i64,corp as i64+1000).await.unwrap();
        let release=async {
            tokio::time::timeout(std::time::Duration::from_secs(10),async {
                loop {
                    if db.db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity a WHERE $1=ANY(pg_blocking_pids(a.pid)))",&[&blocker]).await.unwrap().get::<_,bool>(0) {break;}
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }).await.expect("handover waits on the child's owner change");
            changing.commit().await.unwrap();
        };
        let (result,())=tokio::join!(run(&ctx,Container::Item(office as i64),deposit()),release);
        assert!(!success(&result));
        assert_eq!(db.db.query_one(r#"SELECT "ownerID" FROM entity WHERE "itemID"=$1"#,&[&hull]).await.unwrap().get::<_,i32>(0),owner);
        assert_eq!(db.db.query_one(r#"SELECT "ownerID" FROM entity WHERE "itemID"=$1"#,&[&container]).await.unwrap().get::<_,i32>(0),corp+1000);
    }
}
