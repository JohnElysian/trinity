use super::*;

/// Receipts are defined in the store's schema; nothing is left to set up.
pub fn ensure(_tx:&mut Tx<'_>)->Result<(),StoreError> {Ok(())}

pub(super) fn begin(tx:&mut Tx<'_>,pilot:i64,ship:i64,source:Place,destination:Place)->Result<Marker,StoreError> {
    let valid=id(pilot) && id(ship) && source.valid() && destination.valid() && destination.station.is_none();
    let source_json=serde_json::to_string(&source).unwrap_or_default();
    let destination_json=serde_json::to_string(&destination).unwrap_or_default();
    // The hull must be in space at the destination, piloted by its owner.
    let ship_type=valid.then(||{
        let piloted=tx.table("character_").get_id(pilot).is_some_and(|c|c.int("shipID")==Some(ship)
            && c.int("solarSystemID")==Some(destination.system) && c.int("stationID").unwrap_or(0)==0);
        tx.table("entity").get_id(ship).filter(|e|piloted && e.int("ownerID")==Some(pilot) && e.int("locationID")==Some(destination.system)
            && e.int("flag")==Some(0) && e.int("quantity")==Some(1) && e.int("singleton").is_some_and(|s|s!=0)
            && e.int("typeID").is_some_and(|t|crate::sde::get().types_in_category(6).contains(&(t as i32))))
            .and_then(|e|e.int("typeID"))
    }).flatten();
    let Some(ship_type)=ship_type else {
        return Err(StoreError {code:crate::store::DATA_EXCEPTION,message:format!("travel receipt for ship {ship}: the hull is not in its pilot's custody at the destination")});
    };
    let row=tx.insert("travelReceipts",&[("characterID",Value::Int(pilot)),("shipID",Value::Int(ship)),("shipTypeID",Value::Int(ship_type)),
        ("source",Value::Text(source_json.into())),("destination",Value::Text(destination_json.into())),("createdAt",Value::Int(crate::combat::now_ms()))])?;
    let generation=tx.table("travelReceipts").row(row).and_then(|r|r.int("generation")).unwrap_or(0);
    Ok(Marker {generation,pilot,ship,ship_type,source,destination})
}

pub(super) fn marker(row:&crate::store::Row)->Option<Marker> {
    let source=row.text("source")?;let destination=row.text("destination")?;
    if source.len()>4096 || destination.len()>4096 {return None;}
    let m=Marker {generation:row.int("generation")?,pilot:row.int("characterID")?,ship:row.int("shipID")?,
        ship_type:row.int("shipTypeID")?,source:serde_json::from_str(source).ok()?,
        destination:serde_json::from_str(destination).ok()?};
    m.valid().then_some(m)
}

/// Whether a pending (state 0) receipt names `ship` or `pilot` in `column`.
pub(super) fn pending(db:&Db,column:&str,id:i64)->bool {
    // The column's index holds only pending receipts.
    db.table("travelReceipts").find(&[column],&[Value::Int(id)]).any(|r|r.int("state")==Some(0))
}

/// Pending receipts holding a loss, those after `cursor` first, then by
/// generation: each one's generation, marker and loss.
pub(super) fn with_losses(db:&Db,cursor:i64,limit:usize)->Vec<(i64,Option<Marker>,String)> {
    let mut rows:Vec<_>=db.table("travelReceipts").rows().filter(|r|r.int("state")==Some(0)).filter_map(|r|{
        let loss=r.text("loss")?;
        Some((r.int("generation").unwrap_or(0),marker(&r),loss.to_string()))
    }).collect();
    rows.sort_by_key(|(generation,_,_)|(*generation<=cursor,*generation));
    rows.truncate(limit);
    rows
}

/// Keep `raw` as the pending receipt's loss unless it already holds one.
pub(super) fn retain_loss(tx:&mut Tx<'_>,generation:i64,raw:&str)->Result<(),StoreError> {
    let Some(row)=tx.table("travelReceipts").get_id(generation).filter(|r|r.int("state")==Some(0) && r.text("loss").is_none()).map(|r|r.id) else {return Ok(())};
    tx.update("travelReceipts",row,&[("loss",Value::Text(raw.into()))])
}

pub(super) fn complete(tx:&mut Tx<'_>,generation:i64,now:i64)->Result<(),StoreError> {
    let Some(row)=tx.table("travelReceipts").get_id(generation).filter(|r|r.int("state")==Some(0)).map(|r|r.id) else {return Ok(())};
    tx.update("travelReceipts",row,&[("state",Value::Int(1)),("completedAt",Value::Int(now))])
}

pub(super) struct Locked {pub resolution:Resolution,pub loss:Option<String>}
pub(super) fn locked(db:&Db,m:Marker)->Locked {
    let pilot=db.table("character_").get_id(m.pilot);
    let hull=db.table("entity").get_id(m.ship);
    let custody=|place:Place|pilot.as_ref().is_some_and(|row|row.int("shipID")==Some(m.ship)
        && row.int("solarSystemID")==Some(place.system) && row.int("stationID").unwrap_or(0)==place.station.unwrap_or(0))
        && hull.as_ref().is_some_and(|row|row.int("ownerID").unwrap_or(0)==m.pilot && row.int("typeID").unwrap_or(0)==m.ship_type
            && row.int("locationID").unwrap_or(0)==place.station.unwrap_or(place.system) && row.int("flag").unwrap_or(0)==if place.station.is_some(){4}else{0}
            && row.int("quantity").unwrap_or(0)==1 && row.int("singleton").unwrap_or(0)!=0);
    let receipts=db.table("travelReceipts");
    let Some(row)=receipts.get_id(m.generation) else {
        // A scan: the ship and pilot indexes hold only pending receipts.
        let newer=receipts.rows().any(|r|r.int("generation").is_some_and(|g|g>m.generation)
            && (r.int("shipID")==Some(m.ship) || r.int("characterID")==Some(m.pilot)));
        return Locked {resolution:if !newer && custody(m.source){Resolution::RolledBack}else{Resolution::Unknown},loss:None};
    };
    if marker(&row)!=Some(m){return Locked {resolution:Resolution::Unknown,loss:None};}
    let state=row.int("state").unwrap_or(0);
    let resolution=if state==1 {Resolution::Finalized}else if state==0 && custody(m.destination){Resolution::Accepted}else{Resolution::Unknown};
    Locked {resolution,loss:row.text("loss").map(String::from)}
}
