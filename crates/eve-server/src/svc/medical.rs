//! The medical bay (medical.py): a character's clone and where it waits.
//!
//! The clone is an item of the Clone group the character owns, located on
//! the character. Its type is the grade (how many skill points it keeps);
//! its customInfo names the home station, where the pilot wakes when the
//! capsule is lost. Without one the pilot has an Alpha clone at their
//! corporation's station.

use eve_marshal::Value;
use tracing::{error, info};

use crate::services::{Ctx, Outcome};
use crate::store::{self, Db, Row, StoreError, Tx};

pub const GROUP_CLONE: i32 = 23;
pub const TYPE_CLONE_ALPHA: i64 = 164;
/// market_refTypes: "Clone Activation".
const REF_CLONE: i32 = 8;

/// The station the clone waits in, if one was chosen.
pub fn home_station(db: &Db, char_id: i64) -> Result<Option<i64>, StoreError> {
    Ok(latest_clone(db, char_id).and_then(|r| r.text("customInfo")).and_then(|s| s.trim().parse().ok()))
}

/// Gives the character a clone of `type_id` waiting at `home`, in place of
/// whatever clone it had.
pub fn replace_clone(tx: &mut Tx<'_>, char_id: i64, type_id: i64, home: Option<i64>) -> Result<(), StoreError> {
    let old: Vec<store::RowId> = clones(tx.db(), char_id).map(|r| r.id).collect();
    for id in old {
        tx.delete("entity", id)?;
    }
    tx.insert(
        "entity",
        &[
            ("itemName", store::Value::Text("".into())),
            ("typeID", store::Value::Int(type_id)),
            ("ownerID", store::Value::Int(char_id)),
            ("locationID", store::Value::Int(char_id)),
            ("flag", store::Value::Int(0)),
            ("singleton", store::Value::Int(1)),
            ("quantity", store::Value::Int(1)),
            ("customInfo", home.map_or(store::Value::Null, |h| store::Value::Text(h.to_string().into()))),
        ],
    )?;
    Ok(())
}

/// The character's items of the Clone group.
fn clones(db: &Db, char_id: i64) -> impl Iterator<Item = Row<'_>> {
    let types = crate::sde::get().types_in_group(GROUP_CLONE as i64);
    db.table("entity")
        .find(&["ownerID"], &[store::Value::Int(char_id)])
        .filter(move |r| r.int("typeID").is_some_and(|t| types.contains(&(t as i32))))
}

/// The character's newest clone (the highest itemID).
fn latest_clone(db: &Db, char_id: i64) -> Option<Row<'_>> {
    clones(db, char_id).max_by_key(|r| r.int("itemID"))
}

fn clone_type(db: &Db, char_id: i64) -> Option<i64> {
    latest_clone(db, char_id).and_then(|r| r.int("typeID"))
}

/// character.GetHomeStation: the clone's station, else the corporation's.
pub async fn get_home_station(ctx: &Ctx<'_>) -> Value {
    let chosen = match ctx.session.char_id {
        Some(char_id) => ctx.store.read(|db| home_station(db, char_id)).unwrap_or_else(|e| {
            error!("character.GetHomeStation: {}", crate::logging::chain(&e));
            None
        }),
        None => None,
    };
    Value::int_or_none(chosen.or(ctx.session.hq_id).or(ctx.session.station_id))
}

/// corpStationMgr.SetHomeStation(stationID): move the clone to the station
/// the pilot is docked in.
pub async fn set_home_station(ctx: &Ctx<'_>, station_id: i64) -> Outcome {
    let Some(char_id) = ctx.session.char_id else { return Outcome::custom("No character.") };
    if ctx.session.station_id != Some(station_id) {
        return Outcome::custom("You can only move your clone to the station you are in.");
    }
    let result = ctx.store.write(|tx| {
        let grade = clone_type(tx.db(), char_id).unwrap_or(TYPE_CLONE_ALPHA);
        replace_clone(tx, char_id, grade, Some(station_id))
    });
    match result {
        Ok(()) => {
            info!("{char_id} moved their clone to {station_id}");
            Outcome::Value(Value::None)
        }
        Err(e) => {
            error!("SetHomeStation({station_id}): {}", crate::logging::chain(&e));
            Outcome::custom("Your clone could not be moved.")
        }
    }
}

/// corpStationMgr.SetCloneTypeID(typeID): buy a better clone; it keeps its
/// home station.
pub async fn set_clone_type(ctx: &Ctx<'_>, type_id: i64) -> Outcome {
    use crate::svc::wallet::{move_cash, Payment, Transfer};
    let Some(char_id) = ctx.session.char_id else { return Outcome::custom("No character.") };
    let clone = crate::sde::table("invTypes")
        .get("typeID", type_id as i32 as i64)
        .filter(|t| t.int("groupID") == Some(GROUP_CLONE as i64));
    // Every clone grade has a price (the Alpha's is 0).
    let Some(price) = clone.map(|t| t.float("basePrice").expect("a clone grade without a basePrice")) else {
        return Outcome::custom("That is not a clone.");
    };
    let arg = type_id.to_string();
    let payment = Payment { payer: char_id, payee: None, amount: price, ref_type: REF_CLONE, arg_id1: &arg, reason: "" };
    match move_cash(ctx.store, payment).await {
        Ok(()) => {}
        Err(Transfer::NotEnough { need, have }) => return Outcome::not_enough_money(need, have),
        Err(Transfer::NoSuchOwner) => return Outcome::custom("There is no wallet to pay from."),
        Err(Transfer::Db(e)) => {
            error!("buying clone {type_id}: {}", crate::logging::chain(&e));
            return Outcome::custom("The clone could not be paid for.");
        }
    }
    let result = ctx.store.write(|tx| {
        let home = home_station(tx.db(), char_id)?;
        replace_clone(tx, char_id, type_id, home)
    });
    if let Err(e) = result {
        error!("installing clone {type_id} for {char_id}: {}", crate::logging::chain(&e));
        return Outcome::custom("The clone could not be installed.");
    }
    info!("{char_id} bought clone {type_id} for {price} ISK");
    Outcome::Value(Value::None)
}
