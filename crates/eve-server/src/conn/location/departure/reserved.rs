//! A refused paid jump must leave the live fight intact. Reserve identity and
//! capacitor while the payment is written, and detach only after it commits.
use super::*;
use crate::{combat::{self,Charged},items::ItemTxn,services::Ctx};

#[derive(Clone,Copy)]
pub(super) enum Mobility { Jump, Dock, Admin }

#[derive(Clone,Copy,PartialEq)]
pub(super) enum CommitState {Prepared,Accepted}

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
        Ok(Self {world:ctx.world.clone(),system,ship:id,pilot,client:ctx.client_id,cost,before,reserved,commit_state:CommitState::Prepared,mobility})
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

/// Production calls this inside an owned task: its publication must finish
/// even if the connection stops awaiting the result.
pub(in crate::conn::location) async fn run(server:&crate::conn::Server,session:&crate::session::Session,client:i64,request:JumpRequest,arrival:&JumpArrival,jump:&PreparedJump,in_fx:&str)->std::result::Result<super::transition::Arrived,super::transition::Failure> {
    use super::transition::Failure;
    let ctx=&Ctx {bulk:&server.bulk,dogma:&server.dogma,session,store:&server.store,node_id:server.node_id,world:&server.world,client_id:client};
    let pilot=ctx.session.char_id.ok_or("You are not in space.")?;
    let ship=ctx.session.ship_id.ok_or("You have no active ship.")?;
    if !jump.capacitor.is_finite() || jump.capacitor<0.0 {return Err("The jump capacitor cost is invalid.".into());}
    let (mut reservation,profile,marker,events)=server.store.write(|tx| -> std::result::Result<_,Failure> {
        let mut txn=ItemTxn::new(tx);
        let Some(character)=txn.db().table("character_").get_id(pilot) else {
            return Err("Your character is no longer available.".into());
        };
        if character.int("shipID")!=Some(ship) || character.int("solarSystemID")!=Some(arrival.from) || character.int("stationID").unwrap_or(0)!=0 {
            return Err("Your ship or location changed before departure.".into());
        }
        if crate::losses::pending::blocked_character(txn.db(),pilot).map_err(travel_failed)?
            || crate::travel::blocked_character_in_memory(ctx.world,pilot)
            || crate::svc::jumps::portal::blocked_character_in_memory(ctx.world,pilot) {return Err("Your earlier travel, portal activation or hull loss is still pending.".into());}
        let hull=txn.item(ship).map_err(travel_failed)?.ok_or("Your ship no longer exists.")?;
        if hull.owner_id!=pilot || hull.location_id!=arrival.from || hull.flag!=0 || hull.quantity!=1 || !hull.singleton || hull.category_id!=6 {
            return Err("Your active ship is no longer available here.".into());
        }
        let (departure,reservation)={
            let mut w=ctx.world.lock();
            if !w.parks.contains_key(&arrival.system) {return Err("The destination system is not ready.".into());}
            let park=w.parks.get_mut(&arrival.from).ok_or("Your ship is no longer in space.")?;
            park.advance(destiny::tick_now());
            let departure=Departure::capture(ctx,&w,request,arrival.system)?;
            let reservation=Reservation::reserve(ctx,&mut w,arrival.from,Mobility::Jump,jump.capacitor)?;
            (departure,reservation)
        };
        departure.check_endpoints(txn.db())?;
        let row=txn.db().table("character_").get_id(pilot).map(|c|c.id).ok_or_else(unsaved)?;
        txn.tx().update("character_",row,&[("solarSystemID",StoreValue::Int(arrival.system)),
            ("constellationID",StoreValue::Int(arrival.constellation)),("regionID",StoreValue::Int(arrival.region))]).map_err(travel_failed)?;
        update_by_id(txn.tx(),"entity",ship,&[("locationID",StoreValue::Int(arrival.system)),
            ("x",StoreValue::Float(arrival.at.x)),("y",StoreValue::Float(arrival.at.y)),("z",StoreValue::Float(arrival.at.z))]).map_err(travel_failed)?;
        update_by_id(txn.tx(),"entity",pilot,&[("locationID",StoreValue::Int(arrival.system))]).map_err(travel_failed)?;
        jump.fuel.debit(&mut txn)?;
        // Prepare the inactive modifier profile in the same write, so accepted
        // publication needs no await with the hull outside either park.
        let profile=inflight::profile(txn.db(),ctx.dogma,pilot,ship,&Default::default(),combat::now_ms())
            .map_err(travel_failed)?.ok_or("Your ship's arrival profile could not be read.")?;
        let snapshot={
            let w=ctx.world.lock();
            reservation.current(&w,true)?.clone()
        };
        inflight::write_vitals(txn.tx(),ship,snapshot.fight.as_ref().unwrap(),false).map_err(travel_failed)?;
        crate::cloak::durability::save_history(txn.tx(),&snapshot,combat::now_ms()).map_err(travel_failed)?;
        let source=super::completion::source(txn.db(),arrival.from,None,snapshot.pos)?;
        let destination=crate::travel::Place {system:arrival.system,station:None,constellation:arrival.constellation,region:arrival.region,at:arrival.at};
        let marker=crate::travel::begin(txn.tx(),pilot,ship,source,destination).map_err(travel_failed)?;
        let events=txn.events();
        {
            let w=ctx.world.lock();
            reservation.current(&w,true)?;
            departure.live(&w)?;
        } // Accepted departure: later combat remains on the live hull until it commits.
        Ok((reservation,profile,marker,events))
    })?;
    reservation.commit_state=CommitState::Accepted;
    let live={
        let mut w=ctx.world.lock();
        // Identity reservations defer removal, warp entry and death processing.
        // The live object, including damage taken meanwhile, is the one moved.
        reservation.current(&w,false).map_err(Failure::Pending)?;
        space::remove_ship(&mut w,arrival.from,ship).ok_or_else(||Failure::Pending("Your committed jump lost its source ship.".into()))?
    };
    let arrived=super::completion::finish_space(server,marker,live,profile,None).await?;
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
