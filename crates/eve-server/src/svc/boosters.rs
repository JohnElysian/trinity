//! Original 45017 combat boosters. Only formulas established by the original
//! type/effect catalogue are admitted; unsupported boosters are never consumed.

use std::collections::BTreeMap;
use eve_marshal::Value;
use tokio_postgres::{Client, GenericClient};
use crate::{items::{ItemTxn, NewItem}, services::{Ctx, Outcome}};
use super::{fitting, moves::{self, MoveError}};

mod refresh;
#[cfg(test)] mod tests;

pub const FLAG: i64 = 88;
pub const EXPIRY: i32 = 1088;
const DURATION: i32 = 330;
const SLOT: i32 = 1087;
const BIOLOGY: i32 = 3405;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target { Ship, Gunnery, Missile }

/// Original effect names encode the percentage operation and target scope.
/// Effect2432 is also on Energy Management and the original capacitor rigs.
/// No later type-ID modifier rows are used for any booster.
pub fn modifier(type_id:i64) -> Option<(i32, Target, i32, i32)> {
    Some(match type_id {
        28672 => (1472, Target::Missile, 654, 848),
        28678 => (290, Target::Gunnery, 54, 294),
        28680 => (2432, Target::Ship, 482, 1079),
        28682 => (223, Target::Ship, 37, 315),
        28684 => (298, Target::Gunnery, 158, 349),
        _ => return None,
    })
}

pub async fn ensure(db:&Client)->Result<(),tokio_postgres::Error> {
    db.batch_execute(r#"
        CREATE TABLE IF NOT EXISTS character_boosters (
            item integer PRIMARY KEY REFERENCES entity("itemID") ON DELETE CASCADE,
            character integer NOT NULL REFERENCES "character_"("characterID") ON DELETE CASCADE,
            type_id integer NOT NULL, slot integer NOT NULL CHECK(slot>0),
            started bigint NOT NULL, expires bigint NOT NULL CHECK(expires>started),
            UNIQUE(character,slot));
        CREATE TABLE IF NOT EXISTS character_booster_refresh (
            character integer PRIMARY KEY REFERENCES "character_"("characterID") ON DELETE CASCADE);
        CREATE INDEX IF NOT EXISTS character_boosters_expiry ON character_boosters(expires);
    "#).await
}

fn expiry(start:i64, duration:f64, biology_bonus:f64, level:i64)->Option<i64> {
    let duration=duration*(1.0+biology_bonus*level.clamp(0,5) as f64/100.0);
    if !duration.is_finite() || duration<=0.0 {return None;}
    let ticks=duration*10_000.0;
    if !ticks.is_finite() || ticks>=i64::MAX as f64 {return None;}
    start.checked_add(ticks.round() as i64).filter(|end|*end>start)
}

async fn dirty(db:&impl GenericClient,character:i64)->Result<(),tokio_postgres::Error> {
    db.execute("INSERT INTO character_booster_refresh(character) VALUES($1) ON CONFLICT DO NOTHING",&[&(character as i32)]).await?;
    Ok(())
}

/// The caller already owns the character lifecycle lock. Entity rows are
/// rechecked after their lock is acquired, so expiry never deletes an item
/// subsequently moved, lost, or transferred by another operation.
async fn expire_locked(tx:&mut ItemTxn<'_>,character:i64,now:i64)->Result<bool,tokio_postgres::Error> {
    let rows=tx.tx().query("SELECT item,type_id FROM character_boosters WHERE character=$1 AND expires<=$2 ORDER BY item",&[&(character as i32),&now]).await?;
    for row in &rows {
        let id=i64::from(row.get::<_,i32>(0));
        let item=tx.item(id).await?;
        // A newer deadline/generation is checked under the entity lock too.
        let due=tx.tx().query_opt("SELECT type_id FROM character_boosters WHERE item=$1 AND character=$2 AND expires<=$3 FOR UPDATE",&[&(id as i32),&(character as i32),&now]).await?;
        let Some(due)=due else {continue};
        if item.as_ref().is_some_and(|item|item.owner_id==character && item.location_id==character
            && item.flag==FLAG && item.type_id==i64::from(due.get::<_,i32>(0)) && item.quantity==1 && item.singleton) {
            tx.destroy(id).await?;
        } else {
            tx.tx().execute("DELETE FROM character_boosters WHERE item=$1",&[&(id as i32)]).await?;
        }
    }
    if !rows.is_empty() {dirty(tx.tx(),character).await?;}
    Ok(!rows.is_empty())
}

pub async fn consume(ctx:&Ctx<'_>,args:&[Value])->Outcome {
    let [arg]=args else {return Outcome::custom("Choose one booster to consume.");};
    let Some(id)=arg.as_i64().filter(|id|*id>0 && *id<=i32::MAX as i64) else {return Outcome::custom("Invalid booster item.");};
    let Some(character)=ctx.session.char_id else {return Outcome::custom("Select a character first.");};
    let _inventory=super::pvptrade::inventory_guard(ctx.world,character).await;
    let result:Result<_,Error>=async {
        let mut db=crate::db::connect(ctx.db_url).await?;
        let mut tx=ItemTxn::begin(&mut db).await?;
        if !super::login::lock_character_lifecycle(tx.tx(),character as i32).await? {return Err(Error::Refused("That character no longer exists."));}
        let row=tx.tx().query_one(r#"SELECT "shipID","stationID","solarSystemID" FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#,&[&(character as i32)]).await?;
        if crate::losses::pending::blocked_character(tx.tx(),character).await?
            || crate::losses::journal::blocked_character_in_memory(ctx.world,character)
            || crate::travel::blocked_character_in_memory(ctx.world,character) {
            return Err(Error::Refused("Your ship loss or station arrival must finish before consuming a booster."));
        }
        if super::rows::int(&row,0).filter(|id|*id>0)!=ctx.session.ship_id
            || super::rows::int(&row,1).filter(|id|*id>0)!=ctx.session.station_id.filter(|id|*id>0)
            || super::rows::int(&row,2).filter(|id|*id>0)!=ctx.session.solar_system_id2 {
            return Err(Error::Refused("Your location changed. Try again after the session change."));
        }
        let Some(item)=tx.item(id).await? else {return Err(Error::Refused("That booster no longer exists."));};
        if item.owner_id!=character || item.category_id!=20 || item.quantity<1 {return Err(Error::Refused("That is not your booster."));}
        let typ=tx.tx().query_one(r#"SELECT "groupID","typeName" FROM "invTypes" WHERE "typeID"=$1"#,&[&(item.type_id as i32)]).await?;
        if typ.get::<_,i32>(0)!=303 {return Err(Error::Refused("That item is not a booster."));}
        let Some((effect,..))=modifier(item.type_id) else {return Err(Error::Refused("The original effects of this booster are not implemented yet. It has not been consumed."));};
        let effects=tx.tx().query(r#"SELECT "effectID" FROM "dgmTypeEffects" WHERE "typeID"=$1"#,&[&(item.type_id as i32)]).await?;
        if effects.len()!=1 || effects[0].get::<_,i32>(0)!=effect {return Err(Error::Refused("This booster's effect data does not match the original supported formula."));}
        super::maintenance::pending_mutation(ctx,&mut tx,&item).await?;
        if !matches!(item.flag,0|4|5) || !moves::reachable(ctx,&mut tx,&item).await? {return Err(Error::Refused("The booster must be in accessible cargo or your hangar."));}
        fitting::check_skills(tx.tx(),character,item.type_id).await?;
        let attrs=tx.tx().query(r#"SELECT "attributeID",COALESCE("valueFloat","valueInt"::float8) FROM "dgmTypeAttributes" WHERE "typeID"=$1 AND "attributeID"=ANY($2)"#,&[&(item.type_id as i32),&vec![DURATION,SLOT]]).await?
            .into_iter().filter_map(|r|r.get::<_,Option<f64>>(1).map(|value|(r.get::<_,i32>(0),value))).collect::<BTreeMap<_,_>>();
        let slot=attrs.get(&SLOT).copied().filter(|v|v.is_finite() && v.fract()==0.0 && *v>0.0 && *v<=i32::MAX as f64).ok_or(Error::Refused("The booster has no valid slot."))? as i32;
        let biology=super::skills::levels(tx.tx(),character,&[BIOLOGY]).await?.get(&BIOLOGY).copied().unwrap_or(0);
        let bonus=tx.tx().query_opt(r#"SELECT COALESCE(a."valueFloat",a."valueInt"::float8) FROM "dgmTypeAttributes" a WHERE a."typeID"=$1 AND a."attributeID"=66 AND EXISTS(SELECT 1 FROM "dgmTypeEffects" e WHERE e."typeID"=$1 AND e."effectID"=2866)"#,&[&BIOLOGY]).await?.and_then(|r|r.get::<_,Option<f64>>(0)).ok_or(Error::Refused("The original Biology duration data is missing."))?;
        let now=crate::bulkdata::filetime_now();
        let expires=expiry(now,*attrs.get(&DURATION).unwrap_or(&0.0),bonus,biology).ok_or(Error::Refused("The booster has no valid duration."))?;
        expire_locked(&mut tx,character,now).await?;
        if let Some(existing)=tx.tx().query_opt(r#"SELECT t."typeName" FROM character_boosters b JOIN "invTypes" t ON t."typeID"=b.type_id WHERE b.character=$1 AND b.slot=$2"#,&[&(character as i32),&slot]).await? {
            return Err(Error::Occupied(typ.get(1),existing.get(0)));
        }
        let consumed=if item.quantity>1 {
            tx.set_quantity(id,item.quantity-1).await?;
            tx.create(NewItem{type_id:item.type_id,owner_id:character,location_id:character,flag:FLAG,quantity:1,singleton:true}).await?
        } else {tx.move_to(id,character,FLAG).await?;tx.set_singleton(id,true).await?;id};
        tx.tx().execute("INSERT INTO character_boosters(item,character,type_id,slot,started,expires) VALUES($1,$2,$3,$4,$5,$6)",&[&(consumed as i32),&(character as i32),&(item.type_id as i32),&slot,&now,&expires]).await?;
        tx.tx().execute(r#"INSERT INTO entity_attributes("itemID","attributeID","valueInt","valueFloat") VALUES($1,$2,$3,NULL) ON CONFLICT("itemID","attributeID") DO UPDATE SET "valueInt"=$3,"valueFloat"=NULL"#,&[&(consumed as i32),&EXPIRY,&expires]).await?;
        dirty(tx.tx(),character).await?;
        if crate::losses::journal::blocked_character_in_memory(ctx.world,character)
            || crate::travel::blocked_character_in_memory(ctx.world,character) {
            return Err(Error::Refused("Your ship loss must finish before consuming a booster."));
        }
        let origins=tx.origins();
        Ok((tx.commit().await?,origins))
    }.await;
    match result {
        Ok((events,origins))=>{
            crate::items::notify(ctx.world,ctx.client_id,events);
            super::pvptrade::offers_changed(ctx.world,&origins);
            // The accepted inventory mutation is durable even if this refresh
            // fails. Its durable marker lets the heartbeat retry safely.
            if let Err(error)=refresh::character(ctx.db,ctx.dogma,ctx.world,character).await {tracing::warn!("refreshing consumed booster: {error}");}
            Outcome::Value(Value::None)
        }
        Err(Error::Occupied(first,second))=>Outcome::UserError{msg:"OnlyOneBoosterActive".into(),args:vec![("typeName".into(),Value::text(first)),("typeName2".into(),Value::text(second))]},
        Err(Error::Refused(why))=>Outcome::custom(why),
        Err(Error::Move(MoveError::Refused(why)))=>Outcome::custom(why),
        Err(Error::Move(MoveError::Reactivation{item,delay}))=>moves::reactivation_error(item,delay),
        Err(Error::Db(error))|Err(Error::Move(MoveError::Db(error)))=>{tracing::error!("consuming booster: {}",crate::logging::chain(&error));Outcome::custom("The booster could not be consumed.")}
    }
}

enum Error { Db(tokio_postgres::Error), Move(MoveError), Refused(&'static str), Occupied(String,String) }
impl From<tokio_postgres::Error> for Error {fn from(e:tokio_postgres::Error)->Self{Self::Db(e)}}
impl From<MoveError> for Error {fn from(e:MoveError)->Self{Self::Move(e)}}

pub async fn pulse(server:&crate::conn::Server,db:&mut Client) {
    if let Err(error)=expire(db,&server.world).await {tracing::warn!("expiring boosters: {error}");}
    let pending=match db.query("SELECT character FROM character_booster_refresh ORDER BY character LIMIT 256",&[]).await {
        Ok(rows)=>rows,Err(error)=>{tracing::warn!("reading booster refreshes: {error}");return;}
    };
    for row in pending {
        let character=i64::from(row.get::<_,i32>(0));
        let _inventory=super::pvptrade::inventory_guard(&server.world,character).await;
        if let Err(error)=refresh::character(db,&server.dogma,&server.world,character).await {tracing::warn!("refreshing expired booster for {character}: {error}");}
    }
}

pub async fn expire(db:&mut Client,world:&crate::world::World)->Result<(),tokio_postgres::Error> {
    let now=crate::bulkdata::filetime_now();
    let rows=db.query("SELECT DISTINCT character FROM character_boosters WHERE expires<=$1 ORDER BY character LIMIT 256",&[&now]).await?;
    for row in rows {
        let character=i64::from(row.get::<_,i32>(0));
        let _inventory=super::pvptrade::inventory_guard(world,character).await;
        let mut tx=ItemTxn::begin(db).await?;
        if !super::login::lock_character_lifecycle(tx.tx(),character as i32).await? {continue;}
        expire_locked(&mut tx,character,now).await?;
        let events=tx.commit().await?;
        let w=world.lock();let clients=w.clients_of(character);
        for event in events {w.notify(&clients,"OnItemChange",event);}
    }
    Ok(())
}
