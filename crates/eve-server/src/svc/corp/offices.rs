//! Original quote/RentOffice contract with one custody and payment transaction.
use eve_marshal::Value;
use tokio_postgres::Transaction;
use crate::{items::{ItemTxn,NewItem},services::{Ctx,Outcome}};
use crate::svc::wallet::{self,Entry,Transfer,REF_OFFICE_RENTAL_FEE};

#[cfg(test)] mod tests;
mod slots;
pub(crate) use slots::{ensure_schema,ensure_slots_locked};

struct Rental { corporation:i32, cost:f64, slots:i64 }
enum Error { Db(tokio_postgres::Error), Refused(Outcome) }
impl From<Outcome> for Error {fn from(value:Outcome)->Self{Self::Refused(value)}}
impl Error {
    fn outcome(self)->Outcome {match self {
        Self::Refused(out)=>out,
        Self::Db(error)=>{tracing::error!("office rental transaction: {}",crate::logging::chain(&error));denied("The office rental could not be completed. Check your corporation's offices before retrying.")}
    }}
}
type Result<T> = std::result::Result<T,Error>;

fn message(name:&str)->Outcome {Outcome::UserError{msg:name.into(),args:vec![]}}
fn denied(reason:&str)->Outcome {Outcome::UserError{msg:"RentingOfficeRequestDenied".into(),args:vec![("reason".into(),Value::text(reason))]}}
fn failed(error:tokio_postgres::Error)->Error {Error::Db(error)}

async fn retry<T,F,Fut>(mut operation:F)->Result<T>
where F:FnMut()->Fut,Fut:std::future::Future<Output=Result<T>> {
    // Existing wallet transfers lock wallets by numeric owner ID, which can
    // oppose custody's character→station→corporation order. PostgreSQL40P01
    // proves that the entire attempt was aborted. Reopen a transaction and
    // reread every authority/quote/capacity value. Never retry a connection or
    // ambiguous COMMIT failure, and never publish an attempted rental.
    for attempt in 0..3 {
        match operation().await {
            Err(Error::Db(error)) if attempt<2 && error.code().is_some_and(|code|code.code()=="40P01")=>{
                tracing::warn!(attempt=attempt+1,"retrying confirmed-aborted office rental deadlock");
            }
            result=>return result,
        }
    }
    unreachable!()
}

/// Lock order matches station management: character, station, corporation,
/// roles. Taking the corporation wallet lock before the station would invert
/// that order. Station locking serializes both duplicate and last-slot checks.
async fn context(ctx:&Ctx<'_>,tx:&Transaction<'_>,station:i64,quote:bool)->Result<Rental> {
    let pilot=ctx.session.char_id.filter(|id|*id>0 && *id<=i32::MAX as i64)
        .ok_or_else(||denied("Select a character first."))?;
    if !(1..=i32::MAX as i64).contains(&station) || ctx.session.station_id!=Some(station) {
        return Err(denied("You must be docked at the station whose office you are renting.").into());
    }
    if !crate::svc::login::lock_character_lifecycle(tx,pilot as i32).await.map_err(failed)? {return Err(denied("That character no longer exists.").into());}
    let character=tx.query_one(r#"SELECT "corporationID","stationID" FROM "character_" WHERE "characterID"=$1 FOR SHARE"#,&[&(pilot as i32)]).await.map_err(failed)?;
    let corporation=character.get::<_,Option<i32>>(0).filter(|corp|*corp>=100_000_000)
        .ok_or_else(||denied("Only player corporations can rent offices."))?;
    if ctx.session.corp_id!=Some(i64::from(corporation)) || character.get::<_,Option<i32>>(1)!=Some(station as i32) {
        return Err(denied("Your corporation or location has changed. Try again after the session change.").into());
    }
    if crate::losses::pending::blocked_character(tx,pilot).await.map_err(failed)?
        || crate::losses::journal::blocked_character_in_memory(ctx.world,pilot)
        || crate::travel::blocked_character_in_memory(ctx.world,pilot)
        || crate::svc::jumps::portal::blocked_character_in_memory(ctx.world,pilot) {
        return Err(denied("Your ship loss or station arrival must finish first.").into());
    }
    let place=tx.query_opt(r#"SELECT "officeRentalCost","stationTypeID" FROM "staStations" WHERE "stationID"=$1 FOR UPDATE"#,&[&(station as i32)]).await.map_err(failed)?
        .ok_or_else(||denied("That station no longer exists."))?;
    let cost=place.get::<_,Option<i32>>(0).filter(|cost|*cost>=0).ok_or_else(||denied("That station has no valid office rental fee."))? as f64;
    let station_type=place.get::<_,Option<i32>>(1).ok_or_else(||denied("That station has no office configuration."))?;
    let slots=tx.query_opt(r#"SELECT "officeSlots" FROM "staStationTypes" WHERE "stationTypeID"=$1 FOR SHARE"#,&[&station_type]).await.map_err(failed)?
        .and_then(|r|r.get::<_,Option<i32>>(0)).map_or(0,|slots|i64::from(slots.max(0)));
    let corp=tx.query_opt(r#"SELECT "ceoID",deleted,balance FROM corporation WHERE "corporationID"=$1 FOR UPDATE"#,&[&corporation]).await.map_err(failed)?
        .ok_or_else(||denied("Your corporation no longer exists."))?;
    if corp.get::<_,i16>(1)!=0 || !corp.get::<_,f64>(2).is_finite() {return Err(denied("Your corporation wallet is unavailable.").into());}
    let roles=tx.query_opt(r#"SELECT "rolesAtAll" FROM "chrCorporationRoles" WHERE "characterID"=$1 FOR SHARE"#,&[&(pilot as i32)]).await.map_err(failed)?
        .map_or(0,|r|r.get::<_,i64>(0));
    if corp.get::<_,i32>(0)!=pilot as i32 && roles & crate::session::CORP_ROLE_DIRECTOR==0 {
        return Err((if quote {message("RentingOfficeQuotesOnlyGivenToActiveCEOsOrEquivale")} else {denied("Only the current CEO or a director may rent a corporation office.")}).into());
    }
    Ok(Rental{corporation,cost,slots})
}

pub(super) async fn quote(ctx:&Ctx<'_>,station:i64,args:&[Value])->Outcome {
    if !args.is_empty() {return denied("An office quote takes no arguments.");}
    let result=retry(||async {
        let mut db=crate::db::connect(ctx.db_url).await.map_err(failed)?;
        let tx=db.transaction().await.map_err(failed)?;
        let rental=context(ctx,&tx,station,true).await?;
        tx.commit().await.map_err(failed)?;
        Ok(rental.cost)
    }).await;
    result.map_or_else(Error::outcome,|cost|Outcome::Value(Value::Double(cost)))
}

pub(super) async fn rent(ctx:&Ctx<'_>,station:i64,args:&[Value])->Outcome {
    let [value]=args else {return denied("Provide the quoted office rental fee.");};
    let Some(offered)=value.as_f64().filter(|cost|cost.is_finite() && *cost>=0.0) else {return denied("The offered office rental fee must be a finite, nonnegative number.");};
    let result=retry(||async {
        let mut db=crate::db::connect(ctx.db_url).await.map_err(failed)?;
        let mut tx=ItemTxn::begin(&mut db).await.map_err(failed)?;
        let rental=context(ctx,tx.tx(),station,false).await?;
        if offered<rental.cost {return Err(Outcome::UserError{msg:"RentingAnOfficeCostsMore".into(),args:vec![("amount".into(),Value::Double(rental.cost))]}.into());}
        if tx.tx().query_opt(r#"SELECT 1 FROM "crpOffices" WHERE "corporationID"=$1 AND "stationID"=$2 LIMIT 1"#,&[&rental.corporation,&(station as i32)]).await.map_err(failed)?.is_some() {
            return Err(message("RentingYouHaveAnOfficeHere").into());
        }
        let count:i64=tx.tx().query_one(r#"SELECT count(*) FROM "crpOffices" WHERE "stationID"=$1"#,&[&(station as i32)]).await.map_err(failed)?.get(0);
        if count>=rental.slots {return Err(message("NoOfficesAreAvailableForRenting").into());}
        ensure_slots_locked(tx.tx(),station as i32).await.map_err(failed)?;
        let slot=tx.tx().query_opt(r#"SELECT "slotNumber" FROM "rentalInfo"
            WHERE "stationID"=$1 AND "typeID"=26 AND "renterID"=0 AND "publiclyAvailable"<>0
              AND "slotNumber">=1 AND "slotNumber"<=$2
              AND NOT EXISTS(SELECT 1 FROM "crpOffices" o WHERE o."stationID"=$1 AND o."rentalSlot"="rentalInfo"."slotNumber")
            ORDER BY "slotNumber" LIMIT 1 FOR UPDATE"#,&[&(station as i32),&(rental.slots as i32)]).await.map_err(failed)?
            .map(|row|row.get::<_,i32>(0)).ok_or_else(||message("NoOfficesAreAvailableForRenting"))?;
        let office=tx.create(NewItem{type_id:27,owner_id:i64::from(rental.corporation),location_id:station,flag:0,quantity:1,singleton:true}).await.map_err(failed)?;
        let folder:i32=tx.tx().query_one(r#"INSERT INTO "crpOffices"("corporationID","stationID","itemID","typeID","officeFolderID","rentalSlot") VALUES($1,$2,$3,27,nextval('office_folder_seq'),$4) RETURNING "officeFolderID""#,
            &[&rental.corporation,&(station as i32),&(office as i32),&slot]).await.map_err(failed)?.get(0);
        let start=crate::bulkdata::filetime_now();
        let due=start+30*24*60*60*10_000_000_i64;
        tx.tx().execute(r#"UPDATE "rentalInfo" SET "renterID"=$3,"startDate"=$4,"rentPeriodInDays"=30,
            "periodCost"=$5,"balanceDueDate"=$6,"billID"=0,discontinue=0 WHERE "stationID"=$1 AND "slotNumber"=$2"#,
            &[&(station as i32),&slot,&rental.corporation,&start,&rental.cost,&due]).await.map_err(failed)?;
        let entry=Entry{ref_type:REF_OFFICE_RENTAL_FEE,payer:i64::from(rental.corporation),payee:0,arg_id1:"",reason:""};
        wallet::post(tx.tx(),i64::from(rental.corporation),-rental.cost,&entry).await.map_err(|error|match error {
            Transfer::NotEnough{need,have}=>Outcome::not_enough_money(need,have).into(),
            Transfer::NoSuchOwner=>denied("Your corporation has no wallet.").into(),Transfer::Db(error)=>failed(error),
        })?;
        tx.commit().await.map_err(failed)?;
        Ok((rental,office,folder))
    }).await;
    match result {
        Ok((rental,office,folder))=>{
            {
                // The original three-argument event has no station ID and
                // updates the recipient's current-station office cache.
                let w=ctx.world.lock();let clients=w.players().into_iter()
                    .filter(|p|p.docked && p.station_id==Some(station))
                    .flat_map(|p|w.clients_of(p.char_id)).collect::<Vec<_>>();
                w.notify(&clients,"OnOfficeRentalChanged",Value::Tuple(vec![Value::Int(i64::from(rental.corporation)),Value::Int(office),Value::Int(i64::from(folder))]));
            }
            wallet::notify_balance(ctx,i64::from(rental.corporation)).await;
            tracing::info!(corporation=rental.corporation,station,office,cost=rental.cost,"corporation rented office");
            Outcome::Value(Value::Int(office))
        }
        Err(why)=>why.outcome(),
    }
}
