//! A refused paid jump must leave the live fight intact. Reserve identity and
//! capacitor while SQL waits, and detach only after accepted payment commits.
use super::*;
use crate::{combat::{self,Charged},items::ItemTxn,services::Ctx};

#[derive(Clone,Copy)]
pub(super) enum Mobility { Jump, Dock, Admin }

#[derive(Clone,Copy,PartialEq)]
pub(super) enum CommitState {Prepared,Uncertain,Accepted}

pub(super) struct Reservation {
    world:World,
    pub(super) system:i64,
    ship:i64,
    pilot:i64,
    client:i64,
    cost:f64,
    before:Charged,
    reserved:Charged,
    pub(super) commit_state:CommitState,
    pub(super) receipt:Option<crate::travel::Marker>,
    mobility:Mobility,
}

impl Reservation {
    pub(super) fn reserve(ctx:&Ctx<'_>,w:&mut WorldState,system:i64,mobility:Mobility,cost:f64)->std::result::Result<Self,String> {
        if !cost.is_finite() || cost<0.0 {return Err("The transition capacitor cost is invalid.".into());}
        let pilot=ctx.session.char_id.ok_or("You have no active character.")?;
        let id=ctx.session.ship_id.ok_or("You have no active ship.")?;
        let ship=w.parks.get_mut(&system).and_then(|p|p.ships.get_mut(&id))
            .filter(|s|s.is_pilot() && s.pilot.char_id==pilot && s.client_id==ctx.client_id && !s.identity_reserved)
            .ok_or("You no longer control that ship or it is completing another transition.")?;
        if (!matches!(mobility,Mobility::Admin) && ship.warping()) || (matches!(mobility,Mobility::Jump) && ship.cloaked) {
            return Err("Your ship cannot complete this transition while warping or cloaked.".into());
        }
        let fighter=ship.fight.as_mut().ok_or("Your ship cannot leave space now.")?;
        if fighter.vitals.hull_max<=0.0 || fighter.vitals.hull_damage>=fighter.vitals.hull_max-1e-6 {
            return Err("Your ship was destroyed before departure.".into());
        }
        let before=fighter.vitals.capacitor;
        if cost>0.0 && !fighter.vitals.capacitor.spend(combat::now_ms(),cost) {
            return Err("Your ship no longer has enough capacitor for the jump.".into());
        }
        let reserved=fighter.vitals.capacitor;
        ship.identity_reserved=true;
        Ok(Self {world:ctx.world.clone(),system,ship:id,pilot,client:ctx.client_id,cost,before,reserved,commit_state:CommitState::Prepared,receipt:None,mobility})
    }
    pub(super) fn current<'a>(&self,w:&'a WorldState,alive:bool)->std::result::Result<&'a Ship,String> {
        let ship=w.parks.get(&self.system).and_then(|p|p.ships.get(&self.ship))
            .filter(|s|s.is_pilot() && s.pilot.char_id==self.pilot && s.client_id==self.client && s.identity_reserved)
            .ok_or("Your ship changed before the jump completed.")?;
        if (!matches!(self.mobility,Mobility::Admin) && ship.warping()) || (matches!(self.mobility,Mobility::Jump) && ship.cloaked) {
            return Err("Your ship cannot complete this transition while warping or cloaked.".into());
        }
        if alive && ship.fight.as_ref().is_none_or(|f|f.vitals.hull_max<=0.0 || f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {
            return Err("Your ship was destroyed before departure.".into());
        }
        Ok(ship)
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        // An interrupted COMMIT is neither a refusal nor a capacitor refund.
        // Production owns this future through fresh-lock outcome resolution.
        if self.commit_state==CommitState::Uncertain {return;}
        if let Some(marker)=self.receipt {crate::travel::clear_uncertain(&self.world,&marker);}
        let mut w=self.world.lock();
        let Some(ship)=w.parks.get_mut(&self.system).and_then(|p|p.ships.get_mut(&self.ship))
            .filter(|s|s.pilot.char_id==self.pilot && s.client_id==self.client && s.identity_reserved) else {return;};
        if self.commit_state==CommitState::Prepared && self.cost>0.0 {
            if let Some(f)=ship.fight.as_mut() {
                if f.vitals.capacitor==self.reserved {f.vitals.capacitor=self.before;}
                else {f.vitals.capacitor.add(combat::now_ms(),self.cost);}
            }
        }
        ship.identity_reserved=false;
        let now=combat::now_ms();
        let Some(amount)=ship.fight.as_ref().map(|f|f.vitals.capacitor.at(now)) else {return;};
        w.notify(&[self.client],"OnModuleAttributeChanges",Value::Tuple(vec![Value::List(vec![Value::Tuple(vec![
            Value::bytes("OnModuleAttributeChange"),Value::Int(self.pilot),Value::Int(self.ship),
            Value::Int(combat::attr::CHARGE as i64),Value::Long(combat::filetime(now)),Value::Double(amount),Value::Double(amount),
        ])])]));
    }
}

/// Production calls this inside an owned task. Its publication must finish even
/// if the connection stops awaiting a COMMIT result. Tests may cancel it before
/// COMMIT to exercise transaction rollback and the reservation's RAII release.
pub(in crate::conn::location) async fn run(server:&crate::conn::Server,session:&crate::session::Session,client:i64,request:JumpRequest,arrival:&JumpArrival,jump:&PreparedJump,in_fx:&str)->std::result::Result<super::transition::Arrived,super::transition::Failure> {
    use super::transition::Failure;
    let lease=server.db.lease().await.map_err(|e|Failure::Refused(format!("The travel database is unavailable: {e}")))?;
    let ctx=&Ctx {bulk:&server.bulk,dogma:&server.dogma,session,db:&lease,db_url:&server.db_url,node_id:server.node_id,world:&server.world,client_id:client};
    let pilot=ctx.session.char_id.ok_or("You are not in space.")?;
    let ship=ctx.session.ship_id.ok_or("You have no active ship.")?;
    if !jump.capacitor.is_finite() || jump.capacitor<0.0 {return Err("The jump capacitor cost is invalid.".into());}
    let mut db=crate::db::connect(ctx.db_url).await.map_err(travel_failed)?;
    let mut txn=ItemTxn::begin(&mut db).await.map_err(travel_failed)?;
    if !crate::svc::login::lock_character_lifecycle(txn.tx(),pilot as i32).await.map_err(travel_failed)? {
        return Err("Your character is no longer available.".into());
    }
    let current=txn.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2
        AND "solarSystemID"=$3 AND COALESCE("stationID",0)=0 FOR UPDATE"#,
        &[&(pilot as i32),&(ship as i32),&(arrival.from as i32)]).await.map_err(travel_failed)?;
    if current.is_none() {return Err("Your ship or location changed before departure.".into());}
    if crate::losses::pending::blocked_character(txn.tx(),pilot).await.map_err(travel_failed)?
        || crate::travel::blocked_character_in_memory(ctx.world,pilot)
        || crate::svc::jumps::portal::blocked_character_in_memory(ctx.world,pilot) {return Err("Your earlier travel, portal activation or hull loss is still pending.".into());}
    let hull=txn.item(ship).await.map_err(travel_failed)?.ok_or("Your ship no longer exists.")?;
    if hull.owner_id!=pilot || hull.location_id!=arrival.from || hull.flag!=0 || hull.quantity!=1 || !hull.singleton || hull.category_id!=6 {
        return Err("Your active ship is no longer available here.".into());
    }
    let (departure,mut reservation)={
        let mut w=ctx.world.lock();
        if !w.parks.contains_key(&arrival.system) {return Err("The destination system is not ready.".into());}
        let park=w.parks.get_mut(&arrival.from).ok_or("Your ship is no longer in space.")?;
        park.advance(destiny::tick_now());
        let departure=Departure::capture(ctx,&w,request,arrival.system)?;
        let reservation=Reservation::reserve(ctx,&mut w,arrival.from,Mobility::Jump,jump.capacitor)?;
        (departure,reservation)
    };
    departure.lock_endpoints(txn.tx()).await?;
    txn.tx().execute(r#"UPDATE "character_" SET "solarSystemID"=$2,"constellationID"=$3,"regionID"=$4 WHERE "characterID"=$1"#,
        &[&(pilot as i32),&(arrival.system as i32),&(arrival.constellation as i32),&(arrival.region as i32)]).await.map_err(travel_failed)?;
    txn.tx().execute(r#"UPDATE entity SET "locationID"=$2,x=$3,y=$4,z=$5 WHERE "itemID"=$1"#,
        &[&(ship as i32),&(arrival.system as i32),&arrival.at.x,&arrival.at.y,&arrival.at.z]).await.map_err(travel_failed)?;
    txn.tx().execute(r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,
        &[&(pilot as i32),&(arrival.system as i32)]).await.map_err(travel_failed)?;
    jump.fuel.debit(&mut txn).await?;
    // Prepare the inactive modifier profile while SQL custody is held, so
    // accepted publication needs no await with the hull outside either park.
    let profile=inflight::profile(txn.tx(),ctx.dogma,pilot,ship,&Default::default(),combat::now_ms())
        .await.map_err(travel_failed)?.ok_or("Your ship's arrival profile could not be read.")?;
    let snapshot={
        let w=ctx.world.lock();
        reservation.current(&w,true)?.clone()
    };
    inflight::write_vitals(txn.tx(),ship,snapshot.fight.as_ref().unwrap(),false).await.map_err(travel_failed)?;
    crate::cloak::durability::save_history(txn.tx(),&snapshot,combat::now_ms()).await.map_err(travel_failed)?;
    let source=super::completion::source(txn.tx(),arrival.from,None,snapshot.pos).await?;
    let destination=crate::travel::Place {system:arrival.system,station:None,constellation:arrival.constellation,region:arrival.region,at:arrival.at};
    let marker=crate::travel::begin(txn.tx(),pilot,ship,source,destination).await.map_err(travel_failed)?;
    let events=txn.prepare_events().await.map_err(travel_failed)?;
    {
        let w=ctx.world.lock();
        reservation.current(&w,true)?;
        departure.live(&w)?;
    } // Accepted departure: later combat remains on the live hull through COMMIT.
    crate::travel::track_uncertain(ctx.world,&marker)?;
    reservation.receipt=Some(marker);reservation.commit_state=CommitState::Uncertain;
    if let Err(error)=txn.commit_prepared().await {
        tracing::warn!(ship,"jump COMMIT reply failed: {}",crate::logging::chain(&error));
        let (fresh,outcome)=super::completion::resolve_space(ctx.db_url,&marker).await;db=fresh;
        match outcome {
            crate::travel::Resolution::RolledBack=>{reservation.commit_state=CommitState::Prepared;return Err("The jump transaction rolled back; no payment was committed.".into());}
            crate::travel::Resolution::Finalized=>{reservation.commit_state=CommitState::Accepted;return Err(Failure::Pending("This jump already completed; reconnect to its current result.".into()));}
            crate::travel::Resolution::Accepted=>{},
            crate::travel::Resolution::Unknown=>unreachable!("resolver retains unknown outcomes"),
        }
    }
    reservation.commit_state=CommitState::Accepted;
    let live={
        let mut w=ctx.world.lock();
        // Identity reservations defer removal, warp entry and death processing.
        // The live object, including damage during COMMIT, is the one moved.
        reservation.current(&w,false).map_err(Failure::Pending)?;
        space::remove_ship(&mut w,arrival.from,ship).ok_or_else(||Failure::Pending("Your committed jump lost its source ship.".into()))?
    };
    let arrived=super::completion::finish_space(server,&mut db,marker,live,profile,None).await?;
    reservation.system=arrived.place.system;
    {let w=ctx.world.lock();
        for event in events {w.notify(&jump.clients,"OnItemChange",event);}
        if arrived.ship_id==ship && arrived.place.station.is_none() {
            let tick=w.parks[&arrival.system].tick;let flash=space::special_fx(tick,ship,None,in_fx);
            for client in w.in_space(arrival.system).into_iter().filter(|c|*c!=ctx.client_id) {space::broadcast(&w,arrival.system,Some(client),vec![flash.clone()]);}
        }
    }
    drop(reservation);
    Ok(arrived)
}
