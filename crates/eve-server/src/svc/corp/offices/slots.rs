//! Stable original rentalInfo slot numbers shared by renting and management.
use crate::store::{StoreError,Tx,Value};

/// The rental slot column and its index are part of the store's schema, so
/// there is nothing left to add.
pub(crate) fn ensure_schema(_tx:&mut Tx<'_>)->Result<(),StoreError> {
    Ok(())
}

/// Runs inside the caller's write. Do not rewrite historical rental prices,
/// dates, visibility or slot identities when reconciling older offices.
/// An old office without billing history gets no invented historical dates.
pub(crate) fn ensure_slots_locked(tx:&mut Tx<'_>,station:i64)->Result<(),StoreError> {
    let station_type=tx.table("staStations").get_id(station).and_then(|row|row.int("stationTypeID"));
    let slots=station_type.and_then(|t|crate::sde::table("staStationTypes").get("stationTypeID",t))
        .map_or(0,|t|t.int("officeSlots").unwrap_or(0).max(0));
    for n in 1..=slots {
        tx.insert_or_ignore("rentalInfo",&[("stationID",Value::Int(station)),("slotNumber",Value::Int(n)),("typeID",Value::Int(26)),("publiclyAvailable",Value::Int(1))])?;
    }
    let mut offices:Vec<(i64,i64)>=tx.table("crpOffices").find(&["stationID"],&[Value::Int(station)])
        .filter(|o|o.value("rentalSlot").is_null())
        .map(|o|(o.int("corporationID").unwrap_or(0),o.int("officeFolderID").unwrap_or(0))).collect();
    offices.sort_by_key(|(_,folder)|*folder);
    for (corporation,folder) in offices {
        let db=tx.db();
        // The corporation's own reserved slot first, then the lowest free one.
        let slot=db.table("rentalInfo").find(&["stationID"],&[Value::Int(station)])
            .filter(|r|r.int("typeID")==Some(26))
            .filter(|r|{let renter=r.int("renterID");let n=r.int("slotNumber").unwrap_or(0);
                renter==Some(corporation) || (renter==Some(0) && (1..=slots).contains(&n))})
            .filter(|r|db.table("crpOffices").find_one(&["stationID","rentalSlot"],&[Value::Int(station),r.value("slotNumber").clone()]).is_none())
            .min_by_key(|r|(r.int("renterID")!=Some(corporation),r.int("slotNumber").unwrap_or(0)))
            .map(|r|(r.id,r.int("renterID"),r.int("slotNumber").unwrap_or(0)));
        let Some((rental,renter,slot))=slot else {continue;};
        if renter==Some(0) {tx.update("rentalInfo",rental,&[("renterID",Value::Int(corporation))])?;}
        let office=tx.table("crpOffices").get(&[Value::Int(corporation),Value::Int(folder)])
            .filter(|o|o.int("stationID")==Some(station)).map(|o|o.id);
        if let Some(office)=office {tx.update("crpOffices",office,&[("rentalSlot",Value::Int(slot))])?;}
    }
    Ok(())
}
