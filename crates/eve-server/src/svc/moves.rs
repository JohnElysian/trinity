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
use crate::store::{self, Db, StoreError};
use crate::svc::fitting::{self, Snapshot};
use crate::svc::inventory::{Container, CONTAINER_HANGAR, FLAG_HANGAR};

pub const FLAG_CARGO: i64 = 5;
pub const FLAG_DRONE_BAY: i64 = 87;
const CATEGORY_SHIP: i64 = 6;
const ATTR_DRONE_CAPACITY: i32 = 283;

/// Why a move did not happen.
pub enum MoveError {
    Refused(String),
    Db(StoreError),
    Reactivation {item:i64,delay:i64},
}

pub fn reactivation_error(item:i64,delay:i64)->Outcome{
    Outcome::UserError{msg:"CannotRemoveModuleReactivationDelayed".into(),args:vec![("item".into(),Value::Int(item)),("delay".into(),Value::Double(delay as f64/1000.0))]}
}

impl From<StoreError> for MoveError {
    fn from(e: StoreError) -> Self {
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
    let result: Result<(Vec<Value>, Vec<i64>), MoveError> = ctx.store.write(|tx| {
        let mut txn = ItemTxn::new(tx);
        let Some(pilot)=ctx.session.char_id.filter(|id|*id>0 && i32::try_from(*id).is_ok()) else{return refuse("No active character.");};
        if txn.db().table("character_").get_id(pilot).is_none(){return refuse("That character is unavailable.");}
        apply(ctx, &mut txn, container, op, &mut fitting_before,&mut maintenance)?;
        maintenance.validate(ctx)?;
        let origins = txn.origins();
        Ok((txn.events(), origins))
    });
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
                ctx.store.read(|db| crate::concord::took_from(db, ctx.world, system, me(ctx), corp_id, &origins));
                crate::loot::clear_emptied(ctx.store, ctx.world, system, &origins).await;
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

fn apply(
    ctx: &Ctx<'_>,
    txn: &mut ItemTxn<'_, '_>,
    container: Container,
    op: Op,
    fitting_before: &mut Option<Snapshot>,
    maintenance:&mut super::maintenance::InventoryWrite,
) -> Result<(), MoveError> {
    match op {
        Op::Add { items, quantity, flag } => {
            if items.is_empty() || items.len()>100 || items.iter().copied().collect::<std::collections::BTreeSet<_>>().len()!=items.len(){return refuse("Select between one and one hundred distinct items.");}
            if items.iter().any(|id|!(1..=i64::from(i32::MAX)).contains(id)){return refuse("Invalid item identifier.");}
            let dest = destination(ctx, txn, container, flag,maintenance)?;
            for id in items {
                add(ctx, txn, &dest, id, quantity, fitting_before,maintenance)?;
            }
        }
        Op::Merge(ops) => {
            if ops.is_empty() || ops.len()>100{return refuse("Select between one and one hundred stack merges.");}
            if ops.iter().any(|(a,b,_)|!(1..=i64::from(i32::MAX)).contains(a)||!(1..=i64::from(i32::MAX)).contains(b)){return refuse("Invalid stack identifier.");}
            for (source, target, quantity) in ops {
                // MergeItems has no flag argument; use the target's actual
                // bay/division, then recheck it in merge.
                let Some(row) = txn.item(target)? else {
                    return refuse("That item no longer exists.");
                };
                let dest = destination(ctx, txn, container, Some(row.flag),maintenance)?;
                merge(ctx, txn, &dest, source, target, quantity,maintenance)?;
            }
        }
        Op::StackAll { flag } => {
            let dest = destination(ctx, txn, container, flag,maintenance)?;
            stack_all(ctx, txn, &dest, flag.is_some(),maintenance)?;
        }
        Op::ReplaceCharges { flag, item, quantity } => {
            let dest = destination(ctx, txn, container, flag,maintenance)?;
            let Some(ship) = &dest.fitting else { return refuse("Charges can only be loaded into a module.") };
            if fitting_before.is_none() {
                *fitting_before = Some(fitting::snapshot(txn.db(), ctx.dogma, me(ctx), ctx.session.ship_id)?);
            }
            // The old charges go back to the hold, then the new ones load as a reload would.
            fitting::unload(txn, ship.id, dest.flag, ship.id, FLAG_CARGO)?;
            add(ctx, txn, &dest, item, quantity, fitting_before,maintenance)?;
        }
        Op::Assemble(ids) => assemble(ctx, txn, &ids)?,
        Op::DestroyFitting(id) => {
            let Container::Item(ship_id) = container else { return refuse("Open your ship's fitting first.") };
            if ctx.session.station_id.is_none() || ctx.session.ship_id != Some(ship_id) {
                return refuse("Dock in your active ship before removing a rig.");
            }
            let Some(item) = txn.item(id)? else { return refuse("That rig no longer exists.") };
            super::maintenance::pending_mutation(ctx,txn,&item)?;
            if item.owner_id != me(ctx) || item.location_id != ship_id || !(92..=99).contains(&item.flag)
                || item.category_id != category::MODULE
            {
                return refuse("That is not a rig fitted to your ship.");
            }
            *fitting_before = Some(fitting::snapshot(txn.db(), ctx.dogma, me(ctx), Some(ship_id))?);
            txn.destroy(id)?;
        }
    }
    Ok(())
}

fn assemble(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_, '_>, ids: &[i64]) -> Result<(), MoveError> {
    let Some(station) = ctx.session.station_id else { return refuse("You must be docked.") };
    for item in txn.items(ids)? {
        super::maintenance::pending_mutation(ctx,txn,&item)?;
        if item.category_id != CATEGORY_SHIP || item.singleton {
            continue;
        }
        if item.owner_id != me(ctx) || item.location_id != station {
            return refuse("You can only assemble your own ships in your hangar.");
        }
        if item.quantity > 1 {
            txn.set_quantity(item.id, item.quantity - 1)?;
            txn.create(NewItem { quantity: 1, singleton: true, ..new_like(&item) })?;
        } else {
            txn.set_singleton(item.id, true)?;
        }
    }
    Ok(())
}

fn destination(
    ctx: &Ctx<'_>,
    txn: &mut ItemTxn<'_, '_>,
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
            let Some(holder) = txn.item(id)? else { return refuse("That container no longer exists.") };
            super::maintenance::pending_mutation(ctx,txn,&holder)?;
            if let Some(bay)=maintenance.destination(ctx,txn,&holder,flag,crate::svc::starbase::Access::ViewPut)? {
                return Ok(Destination{location:id,owner:me(ctx),flag:bay.flag,capacity:Some(bay.capacity),fitting:None,bay:Some(bay)});
            }
            if !reachable(ctx, txn, &holder)? {
                return refuse("That container is out of reach.");
            }
            // A corporation office's divisions hold as much as anyone puts in.
            if let Some((corp, _)) = crate::svc::corphangar::office(txn.db(), id) {
                let flag = flag.unwrap_or(crate::svc::corphangar::FLAG_HANGAR);
                if crate::svc::corphangar::division(flag).is_none() {
                    return refuse("Items go into one of the office's divisions.");
                }
                return Ok(Destination { location: id, owner: corp, flag, capacity: None, fitting: None,bay:None });
            }
            // The holder's custody has been checked. Deposits into
            // corporation-owned containers remain corporation property.
            let owner = if Some(holder.owner_id) == ctx.session.corp_id { holder.owner_id } else { me(ctx) };
            if holder.category_id == CATEGORY_SHIP {
                let flag = flag.unwrap_or(FLAG_CARGO);
                if fitting::is_fitting_flag(flag) {
                    return Ok(Destination { location: id, owner, flag, capacity: None, fitting: Some(holder),bay:None });
                }
                let capacity = match flag {
                    FLAG_CARGO => type_capacity(holder.type_id),
                    FLAG_DRONE_BAY => drone_capacity(holder.type_id),
                    _ => return refuse("Items cannot be put there."),
                };
                Ok(Destination { location: id, owner, flag, capacity: Some(capacity), fitting: None,bay:None })
            } else {
                if !holder.singleton {
                    return refuse("Assemble the container before putting things in it.");
                }
                let capacity = match crate::svc::moonmining::silo_capacity(txn.db(), holder.id)? {
                    Some(silo) => silo,
                    None => type_capacity(holder.type_id),
                };
                if capacity <= 0.0 {
                    return refuse("That item cannot hold anything.");
                }
                Ok(Destination { location: id, owner, flag: flag.unwrap_or(0), capacity: Some(capacity), fitting: None,bay:None })
            }
        }
    }
}

fn type_capacity(type_id: i64) -> f64 {
    crate::sde::table("invTypes").get("typeID", type_id).and_then(|r| r.float("capacity")).unwrap_or(0.0)
}

fn drone_capacity(type_id: i64) -> f64 {
    crate::sde::get().type_attribute(type_id, ATTR_DRONE_CAPACITY).unwrap_or(0.0)
}

/// Volume already in a destination, leaving out `except`.
fn used(txn: &ItemTxn<'_, '_>, dest: &Destination, except: i64) -> DbResult<f64> {
    let entity = txn.db().table("entity");
    // Items, and wrapped contents, of a type the static data does not know
    // take no room.
    let types = crate::sde::table("invTypes");
    let volume = |type_id: i64| types.get("typeID", type_id).map(|t| (t.int("groupID"), t.float("volume").unwrap_or(0.0)));
    Ok(entity
        .find(&["locationID", "flag"], &[store::Value::Int(dest.location), store::Value::Int(dest.flag)])
        .filter(|e| e.int("itemID") != Some(except))
        .filter_map(|e| {
            let type_id = e.int("typeID")?;
            let (group, unit) = volume(type_id)?;
            let group = group? as i32;
            if type_id == crate::svc::missions::TYPE_PLASTIC_WRAP {
                // A courier package takes the room of what is wrapped in it.
                let mut contents: Vec<_> = entity.find(&["locationID"], std::slice::from_ref(e.value("itemID"))).collect();
                contents.sort_by_key(|c| c.int("itemID"));
                return Some(
                    contents
                        .iter()
                        .filter_map(|c| volume(c.int("typeID")?).map(|(_, v)| v * c.int("quantity").unwrap_or(0) as f64))
                        .sum::<f64>(),
                );
            }
            Some(super::maintenance::unit_volume(type_id, group, e.int("singleton").unwrap_or(0) != 0, unit) * e.int("quantity").unwrap_or(0) as f64)
        })
        .sum())
}

fn check_room(txn: &ItemTxn<'_, '_>, dest: &Destination, item: &Item, quantity: i64) -> Result<(), MoveError> {
    let Some(capacity) = dest.capacity else { return Ok(()) };
    let free = capacity - used(txn, dest, item.id)?;
    // A courier package takes the room of what is wrapped in it.
    let needed = if item.type_id == crate::svc::missions::TYPE_PLASTIC_WRAP {
        crate::svc::missions::package_volume(txn.db(), item.id)?
    } else {
        let Some(group)=crate::sde::get().type_(item.type_id).map(|t|t.group_id) else {return refuse("That item's type is unknown.")};
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
pub(crate) fn reachable(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_, '_>, item: &Item) -> DbResult<bool> {
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
    use crate::svc::starbase::Access;
    if let Some(ok) = structure_allows(ctx, txn.db(), item.id, Access::ViewPut) {
        return Ok(ok);
    }
    if let Some(ok) = structure_allows(ctx, txn.db(), item.location_id, Access::Take) {
        return Ok(ok);
    }
    // A corporation office: open to its members there, and what is in its
    // divisions to those whose roles let them take it.
    if let Some(office) = crate::svc::corphangar::office(txn.db(), item.id) {
        return Ok(crate::svc::corphangar::mine(ctx, office));
    }
    if let Some(office) = crate::svc::corphangar::office(txn.db(), item.location_id) {
        return Ok(crate::svc::corphangar::mine(ctx, office) && crate::svc::corphangar::may_take(ctx, item.flag));
    }
    if Some(item.owner_id) == ctx.session.corp_id {
        // Nested cargo inherits the outer office division's permissions,
        // read from every ancestor in this same write.
        let mut current = item.clone();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..8 {
            if !seen.insert(current.id) || current.owner_id != item.owner_id || matches!(current.flag, 7 | 36 | 61 | 88 | 89) {
                return Ok(false);
            }
            if let Some(office) = crate::svc::corphangar::office(txn.db(), current.location_id) {
                return Ok(crate::svc::corphangar::mine(ctx, office) && crate::svc::corphangar::may_take(ctx, current.flag));
            }
            let Some(parent) = txn.item(current.location_id)? else { return Ok(false) };
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
        match txn.item(location)? {
            Some(parent) if parent.owner_id == me(ctx) => location = parent.location_id,
            _ => return Ok(false),
        }
    }
    Ok(false)
}

/// What a structure within reach lets the caller do (`starbase::may`), its
/// usage setting read in the move's own store write.
fn structure_allows(ctx: &Ctx<'_>, db: &Db, id: i64, access: crate::svc::starbase::Access) -> Option<bool> {
    use crate::svc::starbase::Access;
    let (system, ship_id) = (ctx.session.solar_system_id?, ctx.session.ship_id?);
    let (s, distance) = {
        let world = ctx.world.lock();
        let park = world.parks.get(&system)?;
        let s = park.structures.get(&id)?.clone();
        park.ships.get(&ship_id)?;
        let distance = park.surface_distance(ship_id, id)?;
        (s, distance)
    };
    if distance > crate::loot::TRANSFER_RANGE * 1.25 {
        return Some(false);
    }
    // Putting a reaction into a reactor is using it.
    let access = if access == Access::ViewPut && s.group_id == crate::svc::moonmining::GROUP_REACTOR { Access::Use } else { access };
    let column = match access {
        Access::ViewPut => "useViewPut",
        Access::Take => "useViewPutTake",
        Access::Use => "useUse",
    };
    let deploy = db.table("posStructures").get_id(s.id).and_then(|r| r.int(column)).unwrap_or(0) as i16;
    Some(crate::svc::starbase::allowed(ctx, &s, deploy))
}

fn add(
    ctx: &Ctx<'_>,
    txn: &mut ItemTxn<'_, '_>,
    dest: &Destination,
    id: i64,
    quantity: Option<i64>,
    fitting_before: &mut Option<Snapshot>,
    maintenance:&mut super::maintenance::InventoryWrite,
) -> Result<(), MoveError> {
    let Some(item) = txn.item(id)? else { return refuse("That item no longer exists.") };
    let source_bay=maintenance.source(ctx,txn,&item)?;
    if source_bay.is_none() && !reachable(ctx, txn, &item)? {
        return refuse("That item is out of reach.");
    }
    let quantity = quantity.unwrap_or(item.quantity);
    if quantity<=0 || quantity>item.quantity || (item.singleton && quantity!=1){return refuse("The requested quantity is not available.");}
    if item.location_id == dest.location && item.flag == dest.flag && quantity==item.quantity {
        if let Some(bay)=dest.bay{maintenance.accepts(ctx,txn,bay,&item,item.quantity)?;}
        return Ok(());
    }
    if fitting::is_slot(item.flag) {
        let now=crate::combat::now_ms();
        let live=crate::cloak::reactivation_remaining(&ctx.world.lock(),item.id,now);
        let durable=crate::cloak::durability::module_reactivation_remaining(txn.db(),&item,now)?;
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
            let Some(holder) = txn.item(parent)? else { break };
            parent = holder.location_id;
        }
    }
    if item.flag == 7 || item.id == me(ctx) {
        return refuse("That cannot be moved.");
    }
    if crate::svc::votes::locked(txn.db(), item.id) {
        return refuse("That item is locked down by a corporation vote.");
    }
    if Some(item.id) == ctx.session.ship_id && dest.capacity.is_some() {
        return refuse("You cannot put your active ship in a container.");
    }
    if item.category_id == CATEGORY_SHIP && item.singleton && dest.capacity.is_some() && dest.bay.is_none() {
        return refuse("Assembled ships cannot be put in a cargo hold; repackage it first.");
    }
    if (fitting::is_slot(item.flag) || dest.fitting.is_some()) && fitting_before.is_none() {
        *fitting_before = Some(fitting::snapshot(txn.db(), ctx.dogma, me(ctx), ctx.session.ship_id)?);
    }
    if let Some(ship) = &dest.fitting {
        return fitting::fit(ctx, txn, ship, &item, dest.flag, Some(quantity));
    }
    if fitting::is_slot(item.flag) && item.category_id == category::MODULE {
        if ctx.session.station_id.is_none() {
            return refuse("Modules can only be unfitted while docked.");
        }
        fitting::unfit(txn, &item, dest.location, dest.flag)?;
    }
    if let Some(why) = crate::svc::moonmining::refuses(txn.db(), dest.location, item.id, item.type_id)? {
        return refuse(why);
    }
    if let Some(bay)=dest.bay {maintenance.accepts(ctx,txn,bay,&item,quantity)?;}
    else{check_room(txn, dest, &item, quantity)?;}
    // Loot taken from a can becomes the taker's; an office or its nested
    // containers hold corporation property, including partial stacks.
    let owner = if dest.bay.is_some(){item.owner_id}else{dest.owner};
    if source_bay.is_some() && owner!=item.owner_id{return refuse("Moving a stored ship does not transfer its ownership.");}
    if quantity==item.quantity && owner!=item.owner_id {super::maintenance::pending_tree_mutation(ctx,txn,item.id)?;}
    if quantity < item.quantity {
        txn.set_quantity(item.id, item.quantity - quantity)?;
        let new = NewItem { location_id: dest.location, flag: dest.flag, quantity, singleton: false, owner_id: owner, ..new_like(&item) };
        txn.create(new)?;
    } else {
        txn.move_to(item.id, dest.location, dest.flag)?;
        if item.owner_id != owner {
            if !txn.transfer_tree(item.id,item.owner_id,owner)? {
                return refuse("That container holds property which cannot be transferred. Active ships, installed items and another owner's property cannot be handed over.");
            }
        }
    }
    Ok(())
}

fn merge(
    ctx: &Ctx<'_>,
    txn: &mut ItemTxn<'_, '_>,
    dest: &Destination,
    source: i64,
    target: i64,
    quantity: Option<i64>,
    maintenance:&mut super::maintenance::InventoryWrite,
) -> Result<(), MoveError> {
    let (Some(from), Some(onto)) = (txn.item(source)?, txn.item(target)?) else {
        return refuse("That item no longer exists.");
    };
    let from_bay=maintenance.source(ctx,txn,&from)?;
    let onto_bay=maintenance.source(ctx,txn,&onto)?;
    if (from_bay.is_none() && !reachable(ctx, txn, &from)?) || (onto_bay.is_none() && !reachable(ctx, txn, &onto)?) {
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
        maintenance.accepts(ctx,txn,bay,&from,quantity)?;
        maintenance.accepts(ctx,txn,bay,&onto,onto.quantity)?;
    }
    if from.location_id != onto.location_id || from.flag != onto.flag {
        if dest.bay.is_none(){check_room(txn, dest, &from, quantity)?;}
    }
    let total=onto.quantity.checked_add(quantity).filter(|n|*n<=i64::from(i32::MAX)).ok_or_else(||MoveError::Refused("The resulting stack is too large.".into()))?;
    txn.set_quantity(onto.id, total)?;
    txn.set_quantity(from.id, from.quantity - quantity)?;
    Ok(())
}

/// Merge every stack of a type in the destination into its oldest stack.
fn stack_all(ctx: &Ctx<'_>, txn: &mut ItemTxn<'_, '_>, dest: &Destination, one_flag: bool,maintenance:&mut super::maintenance::InventoryWrite) -> Result<(), MoveError> {
    let mut owners=vec![dest.owner];
    if dest.bay.is_some_and(|b|b.flag==0){if let Some(corp)=ctx.session.corp_id{owners.push(corp);}}
    let mut stacks: Vec<(Option<i64>, Option<i64>, Option<i64>, i64)> = txn
        .db()
        .table("entity")
        .find(&["locationID"], &[store::Value::Int(dest.location)])
        .filter(|e| {
            e.int("ownerID").is_some_and(|o| owners.contains(&o)) && e.int("singleton") == Some(0) && (!one_flag || e.int("flag") == Some(dest.flag))
        })
        .filter_map(|e| Some((e.int("typeID"), e.int("flag"), e.int("ownerID"), e.int("itemID")?)))
        .collect();
    stacks.sort_unstable();
    let ids: Vec<i64> = stacks.into_iter().map(|(.., id)| id).collect();
    let mut keep: Option<Item> = None;
    for item in txn.items(&ids)? {
        if !owners.contains(&item.owner_id) || item.location_id != dest.location || item.singleton || (one_flag && item.flag != dest.flag) {
            continue;
        }
        let bay=maintenance.source(ctx,txn,&item)?;
        if let Some(bay)=bay{maintenance.accepts(ctx,txn,bay,&item,item.quantity)?;}
        else if !reachable(ctx,txn,&item)?{continue;}
        match &mut keep {
            Some(k) if k.type_id == item.type_id && k.flag == item.flag && k.owner_id==item.owner_id => {
                k.quantity=k.quantity.checked_add(item.quantity).filter(|n|*n<=i64::from(i32::MAX)).ok_or_else(||MoveError::Refused("The resulting stack is too large.".into()))?;
                txn.set_quantity(k.id, k.quantity)?;
                txn.destroy(item.id)?;
            }
            _ => keep = Some(item),
        }
    }
    Ok(())
}

#[cfg(test)]
mod custody_tests {
    use super::*;
    use crate::test_support::Database;
    use store::Value::Int;

    fn success(result:&Outcome)->bool { matches!(result,Outcome::Value(Value::None)) }

    /// Test rows, inserted as they are given.
    fn insert(db:&Database,table:&str,sets:&[(&str,store::Value)]) {
        db.store.write(|tx|tx.insert(table,sets)).unwrap();
    }
    fn update(db:&Database,table:&str,id:i64,sets:&[(&str,store::Value)]) {
        db.store.write(|tx|{let row=tx.table(table).get_id(id).unwrap().id;tx.update(table,row,sets)}).unwrap();
    }
    fn item(db:&Database,id:i64)->Option<(i64,i64,i64)> {
        db.store.read(|d|d.table("entity").get_id(id).map(|e|(e.int("ownerID").unwrap(),e.int("locationID").unwrap(),e.int("quantity").unwrap())))
    }
    fn owners(db:&Database,ids:&[i64])->Vec<i64> {
        ids.iter().filter_map(|id|item(db,*id)).map(|(owner,..)|owner).collect()
    }
    fn first_station(db:&Database)->i64 {
        db.store.read(|d|d.table("staStations").rows().filter_map(|r|r.int("stationID")).min()).unwrap()
    }
    fn text(s:String)->store::Value { store::Value::Text(s.into()) }

    #[tokio::test]
    async fn nested_corporation_deposits_withdrawals_and_merges_inherit_custody() {
        let db=Database::load().await;
        let mut ids=Vec::new();for _ in 0..13 {ids.push(i64::from(db.id().await));}
        let [owner,corp,office,outer,inner,ore,existing,box_id,contents,other,division_stack,restricted_a,restricted_b]=ids[..] else {unreachable!()};
        let station=first_station(&db);
        insert(&db,"corporation",&[("corporationID",Int(corp)),("corporationName",text(format!("Nested deposit {corp}")))]);
        insert(&db,"character_",&[("characterID",Int(owner)),("characterName",text(format!("Nested depositor {owner}"))),("corporationID",Int(corp)),("stationID",Int(station))]);
        insert(&db,"crpOffices",&[("corporationID",Int(corp)),("stationID",Int(station)),("officeFolderID",Int(office)),("itemID",Int(office))]);
        for (id,ty,who,location,flag,quantity,singleton) in [
            (office,27,corp,station,0,1,1),(outer,3293,corp,office,116,1,1),(inner,3293,corp,outer,0,1,1),
            (ore,34,owner,station,4,100,0),(existing,34,corp,inner,0,2,0),
            (box_id,3293,owner,station,4,1,1),(contents,34,owner,box_id,0,3,0),(other,34,owner,station,4,1,0),
            (division_stack,34,corp,office,116,5,0),(restricted_a,34,corp,office,4,2,0),(restricted_b,34,corp,office,4,3,0),
        ] {
            insert(&db,"entity",&[("itemID",Int(id)),("typeID",Int(ty)),("ownerID",Int(who)),("locationID",Int(location)),("flag",Int(flag)),("quantity",Int(quantity)),("singleton",Int(singleton))]);
        }
        // Only division two, not director: inner flags must not select roles.
        let session=crate::session::Session {char_id:Some(owner),corp_id:Some(corp),station_id:Some(station),roles_at_all:16_384|2_097_152,..Default::default()};
        let ctx=db.ctx(&session);
        let nested=Container::Item(inner);
        let hangar=Container::Special {container_id:CONTAINER_HANGAR,owner_id:None};
        let add=|id:i64,quantity|Op::Add {items:vec![id],quantity,flag:None};
        assert!(success(&run(&ctx,nested,add(ore,Some(7))).await));
        let deposited=db.store.read(|d|d.table("entity").find(&["locationID"],&[Int(inner)]).filter_map(|e|e.int("itemID")).find(|id|*id!=existing)).unwrap();
        let (deposited_owner,_,deposited_quantity)=item(&db,deposited).unwrap();
        assert_eq!((deposited_owner,deposited_quantity),(corp,7));
        assert_eq!(item(&db,ore).unwrap().2,93);
        assert!(success(&run(&ctx,nested,add(box_id,None)).await));
        assert!(owners(&db,&[box_id,contents]).iter().all(|o|*o==corp));
        assert!(success(&run(&ctx,hangar,add(box_id,None)).await));
        assert!(owners(&db,&[box_id,contents]).iter().all(|o|*o==owner));

        let no_take=crate::session::Session {roles_at_all:2_097_152,..session.clone()};
        assert!(!success(&run(&db.ctx(&no_take),hangar,add(deposited,None)).await));
        let wrong_division=crate::session::Session {roles_at_all:8_192|2_097_152,..session.clone()};
        assert!(!success(&run(&db.ctx(&wrong_division),nested,add(ore,Some(1))).await));
        assert!(success(&run(&ctx,hangar,add(deposited,Some(3))).await));
        assert_eq!(item(&db,deposited).unwrap().2,4);
        assert!(success(&run(&ctx,nested,Op::StackAll{flag:None}).await));
        let inside:Vec<(i64,i64)>=db.store.read(|d|d.table("entity").find(&["locationID"],&[Int(inner)]).map(|e|(e.int("ownerID").unwrap(),e.int("quantity").unwrap())).collect());
        assert_eq!(inside,vec![(corp,6)]);
        assert!(!success(&run(&ctx,nested,Op::Merge(vec![(existing,other,Some(1))])).await),"a target outside the bound container is refused");
        assert!(success(&run(&ctx,nested,Op::Merge(vec![(ore,existing,Some(1))])).await));
        assert_eq!(item(&db,existing).unwrap().2,7);
        assert!(success(&run(&ctx,Container::Item(office),Op::Merge(vec![(ore,division_stack,Some(1))])).await),"MergeItems derives division two from the target despite lacking a flag argument");
        assert_eq!(item(&db,division_stack).unwrap().2,6);
        assert!(success(&run(&ctx,Container::Item(office),Op::StackAll{flag:None}).await));
        assert_eq!(owners(&db,&[restricted_a,restricted_b]).len(),2,"StackAll cannot mutate another division");
        assert!(!success(&run(&ctx,nested,add(outer,None)).await),"an ancestor cannot become its own descendant");

        // The destination's owner is read as it stands when the move runs,
        // never inherited from an earlier container snapshot.
        update(&db,"entity",inner,&[("ownerID",Int(corp+1000))]);
        assert!(!success(&run(&ctx,nested,add(ore,Some(1))).await));
        assert_eq!(item(&db,ore).unwrap(),(owner,station,91));
    }

    #[tokio::test]
    async fn corporation_hull_and_nested_cargo_transfer_is_atomic_and_revalidates_children() {
        let db=Database::load().await;
        let mut ids=Vec::new();for _ in 0..8 {ids.push(i64::from(db.id().await));}
        let [owner,corp,office,active,hull,fitting,container,cargo]=ids[..] else {unreachable!()};
        let station=first_station(&db);
        let module=i64::from(*crate::sde::get().types_in_category(7).iter()
            .find(|t|crate::sde::table("invTypes").get("typeID",**t as i64).and_then(|r|r.int("published"))==Some(1)).unwrap());
        insert(&db,"corporation",&[("corporationID",Int(corp)),("corporationName",text(format!("Nested custody {corp}")))]);
        insert(&db,"character_",&[("characterID",Int(owner)),("characterName",text(format!("Nested owner {owner}"))),("corporationID",Int(corp)),("shipID",Int(active)),("stationID",Int(station))]);
        insert(&db,"crpOffices",&[("corporationID",Int(corp)),("stationID",Int(station)),("officeFolderID",Int(office)),("itemID",Int(office))]);
        for (id,ty,who,location,flag,quantity) in [(office,27,corp,station,0,1),(active,587,owner,station,4,1),(hull,587,owner,station,4,1),(fitting,module,owner,hull,11,1),(container,3293,owner,hull,5,1),(cargo,34,owner,container,5,42)] {
            insert(&db,"entity",&[("itemID",Int(id)),("typeID",Int(ty)),("ownerID",Int(who)),("locationID",Int(location)),("flag",Int(flag)),("quantity",Int(quantity)),("singleton",Int(1))]);
        }
        insert(&db,"entity_attributes",&[("itemID",Int(fitting)),("attributeID",Int(3)),("valueInt",Int(17))]);
        let session=crate::session::Session {char_id:Some(owner),corp_id:Some(corp),ship_id:Some(active),station_id:Some(station),roles_at_all:crate::session::CORP_ROLE_DIRECTOR,..Default::default()};
        let ctx=db.ctx(&session);
        let deposit=||Op::Add {items:vec![hull],quantity:None,flag:Some(4)};
        let hangar=Container::Special {container_id:CONTAINER_HANGAR,owner_id:None};
        assert!(success(&run(&ctx,Container::Item(office),deposit()).await));
        let ids=[hull,fitting,container,cargo];
        let held=owners(&db,&ids);
        assert_eq!(held.len(),4);assert!(held.iter().all(|o|*o==corp));
        assert!(success(&run(&ctx,hangar,deposit()).await));
        assert!(owners(&db,&ids).iter().all(|o|*o==owner));
        assert_eq!(db.store.read(|d|d.table("entity_attributes").get(&[Int(fitting),Int(3)]).and_then(|a|a.int("valueInt"))),Some(17));

        // A refused child handover undoes even the parent already moved: an
        // active ship nested in the cargo cannot be handed over.
        update(&db,"character_",owner,&[("shipID",Int(cargo))]);
        let failed=run(&ctx,Container::Item(office),deposit()).await;
        update(&db,"character_",owner,&[("shipID",Int(active))]);
        assert!(!success(&failed));
        let (original_owner,original_location,_)=item(&db,hull).unwrap();
        assert_eq!((original_owner,original_location),(owner,station));
        assert!(owners(&db,&ids).iter().all(|o|*o==owner));

        // Somebody's active hull cannot be handed over as ordinary property.
        update(&db,"character_",owner,&[("shipID",Int(hull))]);
        assert!(!success(&run(&ctx,Container::Item(office),deposit()).await));
        update(&db,"character_",owner,&[("shipID",Int(active))]);

        // A child moved out before the handover stays with its owner.
        db.store.write(|tx|ItemTxn::new(tx).move_to(container,station,4)).unwrap();
        assert!(success(&run(&ctx,Container::Item(office),deposit()).await));
        assert_eq!(item(&db,hull).unwrap().0,corp);
        assert!(owners(&db,&[container,cargo]).iter().all(|o|*o==owner),"the child and its subtree moved away before the handover");

        // Even while still attached, a child whose owner changed must never
        // be stolen based on an earlier traversal.
        assert!(success(&run(&ctx,hangar,deposit()).await));
        update(&db,"entity",container,&[("locationID",Int(hull)),("flag",Int(5))]);
        db.store.write(|tx|ItemTxn::new(tx).set_owner(container,corp+1000)).unwrap();
        assert!(!success(&run(&ctx,Container::Item(office),deposit()).await));
        assert_eq!(item(&db,hull).unwrap().0,owner);
        assert_eq!(item(&db,container).unwrap().0,corp+1000);
    }
}
