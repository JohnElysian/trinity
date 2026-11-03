//! The original 45017 station-management argument and rental-row contracts.
use super::*;
use std::collections::HashSet;
use tokio_postgres::{Client,Transaction};

const ROLE_STATION_MANAGER:i64=2048;
const OFFICE_FOLDER:i32=26;

pub(super) fn failed(error:tokio_postgres::Error)->String {
    error!("station management: {}",crate::logging::chain(&error));
    "The station settings could not be read or saved.".into()
}

pub async fn ensure(db:&Client)->Result<(),tokio_postgres::Error> {
    // The original UI accepts fractional ISK/m3. Changing only the RPC cast
    // still truncates that value in the inherited integer column.
    db.batch_execute(r#"DO $$ BEGIN
        IF EXISTS(SELECT 1 FROM pg_attribute WHERE attrelid='"staStations"'::regclass
            AND attname='dockingCostPerVolume' AND atttypid<>'float8'::regtype AND NOT attisdropped) THEN
            ALTER TABLE "staStations" ALTER COLUMN "dockingCostPerVolume" TYPE double precision
                USING "dockingCostPerVolume"::double precision;
        END IF;
    END $$;"#).await
}

/// Keep permission and docked custody stable until the caller commits.
pub(super) async fn authorize(ctx:&Ctx<'_>,tx:&Transaction<'_>,station:i64)->Refused<()> {
    let pilot=ctx.session.char_id.filter(|id|*id>0 && *id<=i32::MAX as i64).ok_or("Select a character first.")?;
    if !(1..=i32::MAX as i64).contains(&station) || ctx.session.station_id!=Some(station) {
        return Err("You must be docked at the station you are managing.".into());
    }
    if !crate::svc::login::lock_character_lifecycle(tx,pilot as i32).await.map_err(failed)? {return Err("Your character is no longer available.".into());}
    let character=tx.query_one(r#"SELECT "corporationID","stationID" FROM "character_" WHERE "characterID"=$1 FOR SHARE"#,&[&(pilot as i32)]).await.map_err(failed)?;
    let corp=character.get::<_,Option<i32>>(0).ok_or("Your corporation is unavailable.")?;
    if ctx.session.corp_id!=Some(i64::from(corp)) || character.get::<_,Option<i32>>(1)!=Some(station as i32) {
        return Err("Your corporation or docked station changed. Try again after the session change.".into());
    }
    if crate::losses::pending::blocked_character(tx,pilot).await.map_err(failed)?
        || crate::losses::journal::blocked_character_in_memory(ctx.world,pilot)
        || crate::travel::blocked_character_in_memory(ctx.world,pilot)
        || crate::conn::logout::blocked_character_in_memory(ctx.world,pilot)
        || crate::svc::jumps::portal::blocked_character_in_memory(ctx.world,pilot) {
        return Err("Your pending arrival, travel or ship loss must finish before station management.".into());
    }
    let place=tx.query_opt(r#"SELECT "corporationID" FROM "staStations" WHERE "stationID"=$1 FOR UPDATE"#,&[&(station as i32)]).await.map_err(failed)?.ok_or("That station no longer exists.")?;
    if place.get::<_,Option<i32>>(0)!=Some(corp) {return Err("Your corporation does not own this station.".into());}
    let ceo=tx.query_opt(r#"SELECT "ceoID" FROM corporation WHERE "corporationID"=$1 FOR SHARE"#,&[&corp]).await.map_err(failed)?.and_then(|r|r.get::<_,Option<i32>>(0));
    let roles=tx.query_opt(r#"SELECT "rolesAtAll" FROM "chrCorporationRoles" WHERE "characterID"=$1 FOR SHARE"#,&[&(pilot as i32)]).await.map_err(failed)?.map_or(0,|r|r.get::<_,Option<i64>>(0).unwrap_or(0));
    if ceo!=Some(pilot as i32) && roles&(ROLE_STATION_MANAGER|ROLE_DIRECTOR)==0 {
        return Err("Only current station managers, directors or the CEO may manage the station.".into());
    }
    Ok(())
}

fn field<'a>(value:&'a Value,name:&str)->Option<&'a Value> {
    match value {Value::Instance{state,..}=>field(state,name),Value::Dict(values)=>values.iter().find(|(key,_)|key.as_string().as_deref()==Some(name)).map(|(_,value)|value),_=>None}
}
fn numeric(value:Option<&Value>,name:&str,min:f64,max:f64)->Refused<f64> {
    value.and_then(Value::as_f64).filter(|n|n.is_finite() && *n>=min && *n<=max)
        .ok_or_else(||format!("The station {name} is outside its allowed range."))
}
fn integer(value:Option<&Value>,name:&str,min:i64,max:i64)->Refused<i64> {
    value.and_then(Value::as_i64).filter(|n|*n>=min && *n<=max)
        .ok_or_else(||format!("The station {name} must be a valid integer."))
}
fn nullable_stamp(value:&Value)->Refused<Option<i64>> {
    if matches!(value,Value::None) {return Ok(None);}
    match value {Value::Int(stamp)|Value::Long(stamp) if *stamp>0=>Ok(Some(*stamp)),
        _=>Err("The capital timestamp must be an exact positive FILETIME or None.".into())}
}
fn stored_stamp(value:Option<String>)->Refused<Option<i64>> {
    value.filter(|v|!v.is_empty()).map(|v|v.parse::<i64>().ok().filter(|n|*n>0)
        .ok_or_else(||"The stored capital timestamp is invalid.".into())).transpose()
}

struct Rule {service:i32,standing:f64,char_min:f64,char_max:f64,corp_min:f64,corp_max:f64}
struct Modifier {service:i32,discount:f64,surcharge:f64}
struct Rental {number:i32,public:bool}
struct Settings {rules:Vec<Rule>,modifiers:Vec<Modifier>,rentals:Vec<Rental>,name:String,description:String,
    docking:f64,office:i32,take:f64,hangar:Option<i32>,capital:Option<i64>}

impl Settings {
    fn parse(args:&[Value])->Refused<Self> {
        let [rules,modifiers,rentals,name,description,docking,office,take,hangar,capital]=args else {
            return Err("Station management requires the original ten arguments.".into());
        };
        let name=name.as_string().filter(|s|!s.trim().is_empty() && s.chars().count()<=100).ok_or("The station name must contain 1 to 100 characters.")?;
        let description=description.as_string().filter(|s|s.chars().count()<=5000).ok_or("The station description may contain at most 5000 characters.")?;
        let docking=numeric(Some(docking),"docking cost",0.0,1000.0)?;
        let office=integer(Some(office),"office cost",0,i32::MAX as i64)? as i32;
        let take=numeric(Some(take),"reprocessing fraction",0.0,1.0)?;
        let hangar=if matches!(hangar,Value::None) {None} else {
            let flag=integer(Some(hangar),"reprocessing hangar",0,i32::MAX as i64)?;
            if crate::svc::corphangar::division(flag).is_none(){return Err("Choose a corporation hangar division for reprocessing.".into());}
            Some(flag as i32)
        };
        let capital=nullable_stamp(capital)?;
        let Value::Dict(rows)=rules else{return Err("Station access rules must be a dictionary.".into());};
        let mut checked_rules=Vec::new();let mut seen=HashSet::new();
        for (key,row) in rows {
            let service=integer(Some(key),"service ID",1,i32::MAX as i64)? as i32;
            if !seen.insert(service) || field(row,"serviceID").is_some_and(|v|v.as_i64()!=Some(i64::from(service))) {return Err("Station service rules have duplicate or mismatched identities.".into());}
            let get=|key|numeric(field(row,key),key,-10.0,10.0);
            let rule=Rule{service,standing:get("minimumStanding")?,char_min:get("minimumCharSecurity")?,char_max:get("maximumCharSecurity")?,corp_min:get("minimumCorpSecurity")?,corp_max:get("maximumCorpSecurity")?};
            if rule.char_min>rule.char_max || rule.corp_min>rule.corp_max {return Err("A station security minimum cannot exceed its maximum.".into());}
            checked_rules.push(rule);
        }
        let Some(rows)=modifiers.seq() else{return Err("Station cost modifiers must be a list.".into());};
        let mut checked_modifiers=Vec::new();seen.clear();
        for row in rows {
            let service=integer(field(row,"serviceID"),"service ID",1,i32::MAX as i64)? as i32;
            if !seen.insert(service){return Err("Station cost modifiers repeat a service.".into());}
            checked_modifiers.push(Modifier{service,discount:numeric(field(row,"discountPerGoodStandingPoint"),"standing discount",0.0,10.0)?,surcharge:numeric(field(row,"surchargePerBadStandingPoint"),"standing surcharge",0.0,10.0)?});
        }
        let Some(rows)=rentals.seq() else{return Err("Station rentable changes must be a list.".into());};
        let mut checked_rentals=Vec::new();seen.clear();
        for row in rows {
            if integer(field(row,"typeID"),"rentable type",1,i32::MAX as i64)?!=i64::from(OFFICE_FOLDER) {return Err("Only original office-folder rental slots can be configured.".into());}
            let number=integer(field(row,"number"),"office slot",1,i32::MAX as i64)? as i32;
            if !seen.insert(number){return Err("Station rentable changes repeat an office slot.".into());}
            let public=integer(field(row,"publiclyAvailable"),"office availability",0,1)?!=0;
            // All other echoed row fields are display data, never authority to
            // change a tenant, charge, bill, or lease date.
            checked_rentals.push(Rental{number,public});
        }
        Ok(Self{rules:checked_rules,modifiers:checked_modifiers,rentals:checked_rentals,name,description,docking,office,take,hangar,capital})
    }
}

pub(super) async fn update(ctx:&Ctx<'_>,station:i64,args:&[Value])->Refused<()> {
    let settings=Settings::parse(args)?;
    let mut db=crate::db::connect(ctx.db_url).await.map_err(failed)?;let tx=db.transaction().await.map_err(failed)?;
    authorize(ctx,&tx,station).await?;
    let old=tx.query_one(r#"SELECT "capitalStation","solarSystemID" FROM "staStations" WHERE "stationID"=$1"#,&[&(station as i32)]).await.map_err(failed)?;
    let old_capital=stored_stamp(old.get(0))?;
    if settings.capital.is_some() && settings.capital!=old_capital {
        let eligible=(61_000_000..64_000_000).contains(&station)
            && tx.query_opt(r#"SELECT 1 FROM "mapSolarSystems" WHERE "solarSystemID"=$1 AND "sovereigntyLevel">=3 FOR SHARE"#,&[&old.get::<_,Option<i32>>(1)]).await.map_err(failed)?.is_some();
        if !eligible{return Err("Only an outpost in a system with sovereignty level three can be designated a capital.".into());}
    }
    let known:HashSet<i32>=tx.query(r#"SELECT "serviceID" FROM "staServices""#,&[]).await.map_err(failed)?.into_iter().map(|r|r.get(0)).collect();
    if settings.rules.iter().map(|r|r.service).chain(settings.modifiers.iter().map(|m|m.service)).any(|id|!known.contains(&id)) {return Err("That station service does not exist.".into());}
    crate::svc::corp::ensure_office_slots_locked(&tx,station as i32).await.map_err(failed)?;
    // Reconciliation retains historical leased rows, including slots outside
    // a subsequently reduced capacity. They remain readable, but cannot be
    // enabled as currently rentable slots by an echoed management row.
    let capacity=tx.query_one(r#"SELECT GREATEST(COALESCE(t."officeSlots",0),0) FROM "staStations" s
        LEFT JOIN "staStationTypes" t ON t."stationTypeID"=s."stationTypeID" WHERE s."stationID"=$1"#,&[&(station as i32)]).await.map_err(failed)?.get::<_,i32>(0);
    for rental in &settings.rentals {
        if rental.number>capacity{return Err("That office slot lies outside the station's current capacity.".into());}
        let changed=tx.execute(r#"UPDATE "rentalInfo" SET "publiclyAvailable"=$3 WHERE "stationID"=$1 AND "slotNumber"=$2 AND "typeID"=26"#,
            &[&(station as i32),&rental.number,&i16::from(rental.public)]).await.map_err(failed)?;
        if changed!=1{return Err("That office rental slot is no longer available for configuration.".into());}
    }
    let capital=settings.capital.map(|stamp|stamp.to_string());
    tx.execute(r#"UPDATE "staStations" SET "stationName"=$2,description=$3,"dockingCostPerVolume"=$4,"officeRentalCost"=$5,
        "reprocessingStationsTake"=$6,"reprocessingHangarFlag"=$7,"capitalStation"=$8 WHERE "stationID"=$1"#,
        &[&(station as i32),&settings.name,&settings.description,&settings.docking,&settings.office,&settings.take,&settings.hangar,&capital]).await.map_err(failed)?;
    tx.execute(r#"UPDATE "mapDenormalize" SET "itemName"=$2 WHERE "itemID"=$1"#,&[&(station as i32),&settings.name]).await.map_err(failed)?;
    tx.execute(r#"UPDATE "eveNames" SET "itemName"=$2 WHERE "itemID"=$1"#,&[&(station as i32),&settings.name]).await.map_err(failed)?;
    for r in settings.rules {
        tx.execute(r#"INSERT INTO "stationServiceRules"("stationID","serviceID","minimumStanding","minimumCharSecurity","maximumCharSecurity","minimumCorpSecurity","maximumCorpSecurity")
            VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT("stationID","serviceID") DO UPDATE SET "minimumStanding"=$3,"minimumCharSecurity"=$4,"maximumCharSecurity"=$5,"minimumCorpSecurity"=$6,"maximumCorpSecurity"=$7"#,
            &[&(station as i32),&r.service,&r.standing,&r.char_min,&r.char_max,&r.corp_min,&r.corp_max]).await.map_err(failed)?;
    }
    for m in settings.modifiers {
        tx.execute(r#"INSERT INTO "stationServiceRules"("stationID","serviceID","discountPerGoodStandingPoint","surchargePerBadStandingPoint") VALUES($1,$2,$3,$4)
            ON CONFLICT("stationID","serviceID") DO UPDATE SET "discountPerGoodStandingPoint"=$3,"surchargePerBadStandingPoint"=$4"#,
            &[&(station as i32),&m.service,&m.discount,&m.surcharge]).await.map_err(failed)?;
    }
    tx.commit().await.map_err(failed)?;info!(station,"station management settings updated");Ok(())
}

pub(super) async fn rentables(ctx:&Ctx<'_>,station:i64,args:&[Value])->Refused<Value> {
    if !args.is_empty(){return Err("GetRentableItems takes no arguments.".into());}
    let mut db=crate::db::connect(ctx.db_url).await.map_err(failed)?;let tx=db.transaction().await.map_err(failed)?;
    authorize(ctx,&tx,station).await?;
    crate::svc::corp::ensure_office_slots_locked(&tx,station as i32).await.map_err(failed)?;
    let rows=tx.query(r#"SELECT "slotNumber","typeID","renterID","publiclyAvailable","startDate","rentPeriodInDays","periodCost","balanceDueDate"
        FROM "rentalInfo" WHERE "stationID"=$1 AND "typeID"=26 ORDER BY "slotNumber""#,&[&(station as i32)]).await.map_err(failed)?;
    let value=Value::List(rows.iter().map(|row|key_val(vec![
        ("number",Value::Int(i64::from(row.get::<_,i32>(0)))),("typeID",Value::Int(i64::from(row.get::<_,i32>(1)))),
        ("rentedToID",int(row,2).filter(|n|*n>0).map_or(Value::None,Value::Int)),
        ("publiclyAvailable",Value::Int(i64::from(row.get::<_,i16>(3)))),
        ("startDate",int(row,4).filter(|n|*n>0).map_or(Value::None,Value::Long)),
        ("rentPeriodInDays",Value::Int(int(row,5).unwrap_or(0))),
        ("periodCost",Value::Double(row.get(6))),
        ("balanceDueDate",int(row,7).filter(|n|*n>0).map_or(Value::None,Value::Long)),
    ])).collect());
    tx.commit().await.map_err(failed)?;Ok(value)
}

pub(super) async fn details(ctx:&Ctx<'_>,station:i64,args:&[Value])->Refused<Value> {
    if args.len()!=1 || args[0].as_i64()!=Some(station) {return Err("Request the details of your bound station.".into());}
    let mut db=crate::db::connect(ctx.db_url).await.map_err(failed)?;let tx=db.transaction().await.map_err(failed)?;
    authorize(ctx,&tx,station).await?;
    let row=tx.query_one(r#"SELECT "stationName",COALESCE(description,''),security,COALESCE("dockingCostPerVolume",0)::float8,
        COALESCE("officeRentalCost",0),COALESCE("maxShipVolumeDockable",0),COALESCE("reprocessingStationsTake",0.05),"reprocessingHangarFlag","corporationID","capitalStation"
        FROM "staStations" WHERE "stationID"=$1"#,&[&(station as i32)]).await.map_err(failed)?;
    let capital=stored_stamp(row.get(9))?;
    let value=key_val(vec![("stationID",Value::Int(station)),("stationName",Value::text(text(&row,0).unwrap_or_default())),
        ("description",Value::text(text(&row,1).unwrap_or_default())),("security",Value::Double(float(&row,2).unwrap_or(0.0)/1000.0)),
        ("dockingCostPerVolume",Value::Double(row.get(3))),("officeRentalCost",Value::Int(int(&row,4).unwrap_or(0))),
        ("maxShipVolumeDockable",Value::Double(float(&row,5).unwrap_or(0.0))),("reprocessingStationsTake",Value::Double(row.get(6))),
        ("reprocessingHangarFlag",Value::int_or_none(int(&row,7))),("ownerID",Value::Int(int(&row,8).unwrap_or(0))),
        ("capitalStation",capital.map_or(Value::None,Value::Long))]);
    drop(tx);Ok(value)
}

pub(super) async fn read(ctx:&Ctx<'_>,station:i64,method:&str,args:&[Value])->Refused<Value> {
    let mut db=crate::db::connect(ctx.db_url).await.map_err(failed)?;let tx=db.transaction().await.map_err(failed)?;
    authorize(ctx,&tx,station).await?;
    let value=match method {
        "GetStationServiceAccessRule"=>{
            if args.len()!=2 || args[0].as_i64()!=Some(station){return Err("Request a service rule for your bound station.".into());}
            let service=integer(args.get(1),"service ID",1,i32::MAX as i64)? as i32;
            let row=tx.query_opt(r#"SELECT COALESCE(r."minimumStanding",-10)::float8,COALESCE(r."minimumCharSecurity",-10)::float8,
                COALESCE(r."maximumCharSecurity",10)::float8,COALESCE(r."minimumCorpSecurity",-10)::float8,COALESCE(r."maximumCorpSecurity",10)::float8
                FROM "staServices" s LEFT JOIN "stationServiceRules" r ON r."serviceID"=s."serviceID" AND r."stationID"=$1 WHERE s."serviceID"=$2"#,
                &[&(station as i32),&service]).await.map_err(failed)?.ok_or("That station service does not exist.")?;
            key_val(vec![("serviceID",Value::Int(i64::from(service))),("minimumStanding",Value::Double(row.get(0))),
                ("minimumCharSecurity",Value::Double(row.get(1))),("maximumCharSecurity",Value::Double(row.get(2))),
                ("minimumCorpSecurity",Value::Double(row.get(3))),("maximumCorpSecurity",Value::Double(row.get(4)))])
        }
        "GetStationManagementServiceCostModifiers"=>{
            if args.len()!=1 || args[0].as_i64()!=Some(station){return Err("Request cost modifiers for your bound station.".into());}
            let rows=tx.query(r#"SELECT s."serviceID",COALESCE(r."discountPerGoodStandingPoint",0)::float8,COALESCE(r."surchargePerBadStandingPoint",0)::float8
                FROM "staServices" s LEFT JOIN "stationServiceRules" r ON r."serviceID"=s."serviceID" AND r."stationID"=$1 ORDER BY s."serviceID""#,&[&(station as i32)]).await.map_err(failed)?;
            Value::List(rows.iter().map(|row|key_val(vec![("serviceID",Value::Int(int(row,0).unwrap())),
                ("discountPerGoodStandingPoint",Value::Double(row.get(1))),("surchargePerBadStandingPoint",Value::Double(row.get(2)))])).collect())
        }
        "GetOwnerIDsOfClonesAtStation"=>{
            if args.len()!=1{return Err("Choose a corporation or all corporations to list clone owners.".into());}
            let corp=integer(args.first(),"corporation ID",-1,i32::MAX as i64)? as i32;
            let rows=tx.query(r#"SELECT DISTINCT c."corporationID",CASE WHEN $2=-1 THEN 0 ELSE j."ownerID" END
                FROM "chrJumpClones" j JOIN "character_" c ON c."characterID"=j."ownerID" WHERE j."locationID"=$1 AND ($2=-1 OR c."corporationID"=$2) ORDER BY 1,2"#,
                &[&(station as i32),&corp]).await.map_err(failed)?;
            Value::List(rows.iter().map(|row|key_val(vec![("corporationID",Value::Int(int(row,0).unwrap_or(0))),("characterID",Value::Int(int(row,1).unwrap_or(0)))])).collect())
        }
        _=>return Err("Unknown station-management read.".into()),
    };
    drop(tx);Ok(value)
}

#[cfg(test)] mod tests;
