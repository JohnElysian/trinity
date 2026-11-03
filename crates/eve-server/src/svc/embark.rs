//! Capsule ejection and boarding abandoned ships. Containment, ownership and
//! the active ship change in one transaction; a reserved live hull cannot be
//! boarded by two pilots or destroyed halfway through the transfer.

use std::collections::BTreeSet;
use tokio_postgres::GenericClient;
use tracing::warn;

use crate::ballpark::{Ship, Vec3};
use crate::items::{ItemTxn, NewItem};
use crate::services::Ctx;
use super::{inflight, moves::{refuse, MoveError}, space};

const CAPSULE: i64 = 670;
/// Trinity const.maxBoardingDistance.
const BOARD_RANGE: f64 = 6550.0;

pub async fn can_fly(db: &impl GenericClient, pilot: i64, type_id: i64) -> Result<(), MoveError> {
    let required = super::fitting::required_skill_levels(db, type_id).await?;
    let types: Vec<i32> = required.keys().copied().collect();
    let trained = super::skills::levels(db, pilot, &types).await?;
    for (skill, level) in required {
        if trained.get(&skill).copied().unwrap_or(0) < level {
            return refuse(format!("You need skill {skill} trained to level {level} to pilot this ship."));
        }
    }
    Ok(())
}

/// An empty hull keeps its property owner but receives no former pilot's
/// skills or implants. Its pilot field is only ownership metadata.
pub(crate) async fn empty_profile(db: &impl GenericClient, ctx: &Ctx<'_>, hull: &Ship) -> Result<Ship, MoveError> {
    let Some(profile) = inflight::profile(db, ctx.dogma, 0, hull.id, &BTreeSet::new(), crate::combat::now_ms()).await? else {
        return refuse("The abandoned ship could not be loaded.");
    };
    let mut empty = hull.clone();
    let owner = hull.pilot.clone();
    inflight::apply_profile(&mut empty, profile, crate::combat::now_ms());
    empty.pilot.char_id = owner.char_id;
    empty.pilot.corp_id = owner.corp_id;
    empty.pilot.alliance_id = owner.alliance_id;
    empty.pilot.security = owner.security;
    empty.pilot.bounty = owner.bounty;
    empty.client_id = 0;
    empty.unpiloted = true;
    empty.cloaked = false;
    empty.arrive(hull.pos);
    if let Some(f) = &mut empty.fight { f.modules.clear(); f.locks.clear(); f.applied.clear(); }
    Ok(empty)
}

/// Reconstruct only ships that are nobody's active hull. Offline characters'
/// active ships remain owned by their login path, never become boardable.
pub async fn load_empty(ctx: &Ctx<'_>, system: i64) -> Result<Vec<Ship>, tokio_postgres::Error> {
    let rows = ctx.db.query(r#"SELECT e."itemID",e."ownerID",e.x,e.y,e.z,
        COALESCE(c."corporationID",0),co."allianceID"
        FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID"
        JOIN "invGroups" g ON g."groupID"=t."groupID"
        LEFT JOIN "character_" c ON c."characterID"=e."ownerID"
        LEFT JOIN corporation co ON co."corporationID"=c."corporationID"
        WHERE e."locationID"=$1 AND e.flag=0 AND e.singleton=1 AND g."categoryID"=6
          AND NOT EXISTS (SELECT 1 FROM "character_" p WHERE p."shipID"=e."itemID")"#, &[&(system as i32)]).await?;
    let mut ships = Vec::new();
    for row in rows {
        let id = i64::from(row.get::<_, i32>(0));
        let pos = Vec3::new(row.get(2), row.get(3), row.get(4));
        if !pos.is_finite() { continue; }
        let Some(mut p) = inflight::profile(ctx.db, ctx.dogma, 0, id, &BTreeSet::new(), crate::combat::now_ms()).await? else { continue; };
        p.pilot.char_id = i64::from(row.get::<_, i32>(1));
        p.pilot.corp_id = i64::from(row.get::<_, i32>(5));
        p.pilot.alliance_id = row.get::<_, Option<i32>>(6).map(i64::from);
        let mut ship = Ship::new(id, 0, p.stats, p.pilot, pos, Vec3::ZERO);
        ship.fight = Some(p.fighter);
        ship.unpiloted = true;
        ships.push(ship);
    }
    Ok(ships)
}

fn ready_in(ctx: &Ctx<'_>, w: &crate::world::WorldState, system: i64, current: i64, target: Option<i64>, reserved: bool) -> Result<(), MoveError> {
    let Some(park) = w.parks.get(&system) else { return refuse("Your ship is not in space."); };
    let Some(ship) = park.ships.get(&current).filter(|s| s.is_player() && s.client_id == ctx.client_id && Some(s.pilot.char_id) == ctx.session.char_id) else {
        return refuse("You are not piloting that ship.");
    };
    if ship.warping() || ship.cloaked { return refuse("You cannot change ships while warping or cloaked."); }
    if ship.identity_reserved != reserved { return refuse("That ship is already changing pilots."); }
    if crate::svc::jumps::portal::active_until_locked(w,current).is_some() {
        return refuse("Your jump portal must finish its active cycle before you can change ships.");
    }
    if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {
        return refuse("Your ship has been destroyed.");
    }
    if ship.fight.as_ref().is_some_and(|f| f.modules.iter().any(|m| matches!(m.action, crate::combat::Action::Cyno))) {
        return refuse("Deactivate your cynosural field before ejecting.");
    }
    if let Some(target) = target {
        let Some(hull) = park.ships.get(&target).filter(|s| s.unpiloted && !s.warping() && !s.cloaked) else {
            return refuse("You can only board an abandoned ship.");
        };
        if hull.identity_reserved != reserved { return refuse("That ship is already changing pilots."); }
        if park.surface_distance(current, target).map_or(true,|d| !d.is_finite() || d > BOARD_RANGE) {
            return refuse(format!("Get within {BOARD_RANGE} metres to board that ship."));
        }
        if hull.fight.as_ref().is_some_and(|f| f.vitals.hull_damage >= f.vitals.hull_max-1e-6) {
            return refuse("That ship is too damaged to board.");
        }
    } else if ship.pilot.type_id == CAPSULE {
        return refuse("You cannot eject from your capsule.");
    }
    Ok(())
}

fn ready(ctx: &Ctx<'_>, system: i64, current: i64, target: Option<i64>) -> Result<(), MoveError> {
    ready_in(ctx,&ctx.world.lock(),system,current,target,false)
}

/// Change the cockpit profile without rewinding damage, charge or position
/// accumulated during the database transaction.
pub(crate) fn with_profile(live: &Ship, prepared: &Ship) -> Ship {
    let mut ship=live.clone();
    if let Some(fighter)=&prepared.fight {
        inflight::apply_profile(&mut ship,inflight::Profile {stats:prepared.stats,pilot:prepared.pilot.clone(),fighter:fighter.clone()},crate::combat::now_ms());
    }
    ship.client_id=prepared.client_id;ship.unpiloted=prepared.unpiloted;ship.cloaked=false;
    ship.arrive(live.pos);
    if let Some(f)=&mut ship.fight {f.modules.clear();f.locks.clear();f.applied.clear();}
    ship
}

/// None ejects into a newly created capsule. Some boards an empty hull, leaving
/// the current hull behind (an emptied capsule is removed).
pub async fn switch(ctx: &Ctx<'_>, target: Option<i64>) -> Result<i64, MoveError> {
    let (Some(pilot), Some(current), Some(system), None) =
        (ctx.session.char_id, ctx.session.ship_id, ctx.session.solar_system_id, ctx.session.station_id) else {
            return refuse("You must be in space to change ships this way.");
        };
    if target == Some(current) { return Ok(current); }
    ready(ctx, system, current, target)?;
    let mut db = crate::db::connect(ctx.db_url).await?;
    let mut txn = ItemTxn::begin(&mut db).await?;
    if !super::login::lock_character_lifecycle(txn.tx(), pilot as i32).await? {
        return refuse("That character is no longer available.");
    }
    let state = txn.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2 AND "stationID"=0 AND "solarSystemID"=$3 FOR UPDATE"#,
        &[&(pilot as i32), &(current as i32), &(system as i32)]).await?;
    if state.is_none() { return refuse("Your ship or location changed. Try again."); }
    let mut ids = vec![current];
    ids.extend(target);
    ids.sort_unstable();
    for id in ids { if txn.item(id).await?.is_none() { return refuse("That ship no longer exists."); } }
    let old = txn.item(current).await?.unwrap();
    if old.owner_id != pilot || old.location_id != system { return refuse("You do not control that ship."); }
    let mut recipients = vec![ctx.client_id];
    if let Some(target) = target {
        let hull = txn.item(target).await?.unwrap();
        if hull.category_id != 6 || !hull.singleton || hull.flag != 0 || hull.location_id != system {
            return refuse("You can only board an assembled ship here in space.");
        }
        if txn.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "shipID"=$1"#, &[&(target as i32)]).await?.is_some() {
            return refuse("That ship already has a pilot.");
        }
        can_fly(txn.tx(), pilot, hull.type_id).await?;
        recipients.extend(ctx.world.lock().clients_of(hull.owner_id));
    }
    // Revalidate after waiting for SQL locks, then reserve both live hulls in
    // one short world-lock section. No database await holds the world lock.
    ready(ctx, system, current, target)?;
    let (old_live, target_live) = {
        let mut w = ctx.world.lock();
        ready_in(ctx,&w,system,current,target,false)?;
        let Some(park) = w.parks.get_mut(&system) else { return refuse("The ballpark changed."); };
        let old=park.ships.get_mut(&current).unwrap();old.identity_reserved=true;
        let old=old.clone();
        let target=target.map(|id| {let hull=park.ships.get_mut(&id).unwrap();hull.identity_reserved=true;hull.clone()});
        (old,target)
    };
    let done = async {
        if let Some(f) = &old_live.fight { inflight::write_vitals(txn.tx(), current, f, false).await?; }
        txn.set_position(current, old_live.pos.tuple()).await?;
        let next = match target_live.as_ref() {
            Some(hull) => {
                if let Some(f) = &hull.fight { inflight::write_vitals(txn.tx(), hull.id, f, false).await?; }
                txn.set_position(hull.id, hull.pos.tuple()).await?;
                // All carried property follows the hull. Lock and reread each
                // child so an item that moved out while waiting is not seized.
                let mut parents = vec![hull.id];
                let mut seen = BTreeSet::new();
                while let Some(parent) = parents.pop() {
                    if !seen.insert(parent) { continue; }
                    txn.set_owner(parent, pilot).await?;
                    let children = txn.tx().query(r#"SELECT "itemID" FROM entity WHERE "locationID"=$1 ORDER BY "itemID""#, &[&(parent as i32)]).await?;
                    for row in children {
                        let id = i64::from(row.get::<_, i32>(0));
                        if id == pilot { continue; }
                        if txn.item(id).await?.is_some_and(|i| i.location_id==parent) { parents.push(id); }
                    }
                }
                hull.id
            }
            None => {
                let id = txn.create(NewItem { type_id: CAPSULE, owner_id: pilot, location_id: system, flag: 0, quantity: 1, singleton: true }).await?;
                txn.set_position(id, old_live.pos.tuple()).await?;
                id
            }
        };
        let mut empty = if old.type_id == CAPSULE {
            txn.destroy(current).await?;
            None
        } else {
            Some(empty_profile(txn.tx(), ctx, &old_live).await?)
        };
        if let Some(empty) = &mut empty {
            if let Some(f) = &empty.fight { inflight::write_vitals(txn.tx(), current, f, false).await?; }
        }
        txn.tx().execute(r#"UPDATE "character_" SET "shipID"=$2 WHERE "characterID"=$1"#, &[&(pilot as i32), &(next as i32)]).await?;
        // Character inventory location follows the station/system convention
        // used by docking, jumps and clone transfers; shipID is the cockpit.
        txn.tx().execute(r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#, &[&(pilot as i32), &(system as i32)]).await?;
        let Some(profile) = inflight::profile(txn.tx(), ctx.dogma, pilot, next, &BTreeSet::new(), crate::combat::now_ms()).await? else {
            return refuse("The new ship profile could not be loaded.");
        };
        let pos = target_live.as_ref().map_or(old_live.pos, |s|s.pos);
        let mut ship = Ship::new(next, ctx.client_id, profile.stats, profile.pilot, pos, Vec3::ZERO);
        ship.fight = Some(profile.fighter);
        if ship.fight.as_ref().is_some_and(|f|f.vitals.hull_max>0.0 && f.vitals.hull_damage>=f.vitals.hull_max) {
            return refuse("That ship is too damaged for you to board.");
        }
        // Freeze the final durable snapshot as late as possible. The hulls
        // stayed visible and combat continued while the preceding SQL ran.
        let (latest_old,latest_target)={
            let mut w=ctx.world.lock();
            if let Some(park)=w.parks.get_mut(&system) {park.advance(crate::destiny::tick_now());}
            ready_in(ctx,&w,system,current,target,true)?;
            let park=&w.parks[&system];
            (park.ships[&current].clone(),target.map(|id|park.ships[&id].clone()))
        };
        if let Some(prepared)=&empty {empty=Some(with_profile(&latest_old,prepared));}
        if let Some(latest)=&latest_target {ship=with_profile(latest,&ship);} else {ship.arrive(latest_old.pos);}
        if let Some(hull)=&empty {
            txn.set_position(current,hull.pos.tuple()).await?;
            if let Some(f)=&hull.fight {inflight::write_vitals(txn.tx(),current,f,false).await?;}
        }
        txn.set_position(next,ship.pos.tuple()).await?;
        if let Some(f)=&ship.fight {inflight::write_vitals(txn.tx(),next,f,false).await?;}
        // A lethal hit or range change while the final writes waited aborts
        // the transfer; the next heartbeat processes the actual destruction.
        ready_in(ctx,&ctx.world.lock(),system,current,target,true)?;
        let events = txn.commit().await?;
        Ok::<_, MoveError>((ship, empty, events))
    }.await;
    match done {
        Ok((mut ship, mut empty, events)) => {
            let next = ship.id;
            let mut w = ctx.world.lock();
            // Keep damage, charge and position changes that happened during
            // the transaction; a failed request never removes live balls.
            if let Some(park)=w.parks.get_mut(&system) {
                park.advance(crate::destiny::tick_now());
                if let Some(live)=park.ships.get_mut(&current) {live.identity_reserved=false;}
                if let Some(id)=target {if let Some(live)=park.ships.get_mut(&id) {live.identity_reserved=false;}}
            }
            let live_old=w.parks.get(&system).and_then(|p|p.ships.get(&current)).cloned().unwrap_or(old_live);
            if let Some(prepared)=empty.take() {
                let mut live=with_profile(&live_old,&prepared);live.identity_reserved=false;
                empty=Some(live);
            }
            if let Some(id)=target {
                if let Some(live)=w.parks.get(&system).and_then(|p|p.ships.get(&id)) {
                    let mut live=with_profile(live,&ship);live.identity_reserved=false;
                    ship=live;
                }
            } else {ship.arrive(live_old.pos);}
            // Complete normal departure bookkeeping after the durable transfer.
            space::remove_ship(&mut w, system, current);
            if let Some(id)=target {space::remove_ship(&mut w,system,id);}
            if let Some(empty) = empty { space::add_ship(&mut w, system, empty); }
            space::add_ship(&mut w, system, ship);
            recipients.sort_unstable(); recipients.dedup();
            for event in events { w.notify(&recipients, "OnItemChange", event); }
            Ok(next)
        }
        Err(error) => {
            let mut w = ctx.world.lock();
            if let Some(park)=w.parks.get_mut(&system) {
                if let Some(ship)=park.ships.get_mut(&current) {ship.identity_reserved=false;}
                if let Some(id)=target {if let Some(ship)=park.ships.get_mut(&id) {ship.identity_reserved=false;}}
            }
            Err(error)
        }
    }
}

/// Station Board and Evacuate use the same ownership and skill checks, without
/// publishing an abandoned hull into space. An emptied capsule is consumed.
pub async fn station_switch(ctx: &Ctx<'_>, target: Option<i64>) -> Result<i64, MoveError> {
    let (Some(pilot), Some(current), Some(station)) = (ctx.session.char_id, ctx.session.ship_id, ctx.session.station_id) else {
        return refuse("You must be docked to use the station hangar.");
    };
    if target == Some(current) { return Ok(current); }
    let mut db=crate::db::connect(ctx.db_url).await?;
    let mut txn=ItemTxn::begin(&mut db).await?;
    if !super::login::lock_character_lifecycle(txn.tx(),pilot as i32).await? { return refuse("That character is no longer available."); }
    if txn.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2 AND "stationID"=$3 FOR UPDATE"#,
        &[&(pilot as i32),&(current as i32),&(station as i32)]).await?.is_none() {
        return refuse("Your ship or station changed. Try again.");
    }
    let mut ids=vec![current];ids.extend(target);ids.sort_unstable();
    for id in ids { if txn.item(id).await?.is_none() { return refuse("That ship no longer exists."); } }
    let old=txn.item(current).await?.unwrap();
    if old.owner_id!=pilot || old.location_id!=station || old.flag!=4 || !old.singleton || old.category_id!=6 {
        return refuse("You do not control that ship in this station.");
    }
    if target.is_none() && old.type_id==CAPSULE { return Ok(current); }
    let next=if let Some(target)=target {
        let hull=txn.item(target).await?.unwrap();
        if hull.owner_id!=pilot || hull.location_id!=station || hull.flag!=4 || !hull.singleton || hull.category_id!=6 {
            return refuse("You can only board your assembled ships in this station.");
        }
        if txn.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "shipID"=$1 AND "characterID"<>$2"#,&[&(target as i32),&(pilot as i32)]).await?.is_some() {
            return refuse("That ship already has a pilot.");
        }
        can_fly(txn.tx(),pilot,hull.type_id).await?;
        target
    } else {
        txn.create(NewItem {type_id:CAPSULE,owner_id:pilot,location_id:station,flag:4,quantity:1,singleton:true}).await?
    };
    if old.type_id==CAPSULE { txn.destroy(current).await?; }
    txn.tx().execute(r#"UPDATE "character_" SET "shipID"=$2 WHERE "characterID"=$1"#,&[&(pilot as i32),&(next as i32)]).await?;
    let events=txn.commit().await?;
    crate::items::notify(ctx.world,ctx.client_id,events);
    Ok(next)
}

pub fn error_text(error: MoveError) -> String {
    match error {
        MoveError::Refused(reason) => reason,
        MoveError::Reactivation{delay,..}=>format!("The module is still recovering for {:.1} seconds.",delay as f64/1000.0),
        MoveError::Db(error) => { warn!("changing ship: {}", crate::logging::chain(&error)); "Your ship could not be changed.".into() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{session::Session, test_support::Database};

    async fn pilot(f: &Database, ship_type: i32, client: i64) -> Session {
        let who = f.id().await;
        let hull = f.id().await;
        f.db.execute(r#"INSERT INTO "character_" ("characterID","accountID","characterName","shipID","stationID","solarSystemID","corporationID") VALUES ($1,$1,$2,$3,0,30000141,1000002)"#,
            &[&who,&format!("Embark fixture {who}"),&hull]).await.unwrap();
        f.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,singleton,quantity,x,y,z) VALUES ($1,$2,$3,30000141,0,1,1,9000,100,200)"#,
            &[&hull,&ship_type,&who]).await.unwrap();
        for (skill, level) in super::super::fitting::required_skill_levels(&f.db, 601).await.unwrap() {
            let id=f.id().await;
            f.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,singleton,quantity) VALUES ($1,$2,$3,$3,7,1,1)"#,&[&id,&skill,&who]).await.unwrap();
            f.db.execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt") VALUES ($1,280,$2)"#,&[&id,&level]).await.unwrap();
        }
        let session=Session {char_id:Some(who as i64),ship_id:Some(hull as i64),solar_system_id:Some(30000141),solar_system_id2:Some(30000141),corp_id:Some(1000002),..Default::default()};
        let mut ctx=f.ctx(&session); ctx.client_id=client;
        let mut ship=inflight::ship_for_space(&ctx,who as i64,hull as i64).await.unwrap();
        ship.client_id=client;
        let mut w=f.world.lock();
        w.parks.entry(30000141).or_insert_with(||crate::ballpark::Park::new(30000141,vec![],crate::destiny::tick_now())).ships.insert(hull as i64,ship);
        session
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn eject_reconstruct_and_reboard_preserve_hull_and_remove_empty_capsule() {
        let f=Database::load().await;
        let mut session=pilot(&f,601,1).await;
        let original=session.ship_id.unwrap();
        let capsule=switch(&f.ctx(&session),None).await.unwrap_or_else(|e|panic!("{}",error_text(e)));
        assert_ne!(capsule,original);
        {
            let w=f.world.lock(); let park=&w.parks[&30000141];
            assert!(park.ships[&original].unpiloted);
            assert!(!park.ships[&original].is_pilot());
            assert!(park.ships[&capsule].is_player());
            assert_eq!(park.ships[&capsule].pilot.type_id,CAPSULE);
        }
        let loaded=load_empty(&f.ctx(&session),30000141).await.unwrap();
        assert!(loaded.iter().any(|s|s.id==original && s.unpiloted && s.pilot.char_id==session.char_id.unwrap()));
        assert!(!loaded.iter().any(|s|s.id==capsule),"offline active capsules must not become boardable");
        session.ship_id=Some(capsule);
        assert!(switch(&f.ctx(&session),None).await.is_err(),"cannot eject from a capsule");
        f.world.lock().parks.get_mut(&30000141).unwrap().ships.get_mut(&original).unwrap().pos=Vec3::new(1e8,0.0,0.0);
        assert!(switch(&f.ctx(&session),Some(original)).await.is_err(),"boarding is range limited");
        f.world.lock().parks.get_mut(&30000141).unwrap().ships.get_mut(&original).unwrap().pos=Vec3::new(9000.0,100.0,200.0);
        assert_eq!(switch(&f.ctx(&session),Some(original)).await.unwrap_or_else(|e|panic!("{}",error_text(e))),original);
        assert!(f.db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(capsule as i32)]).await.unwrap().is_none());
        let saved:i32=f.db.query_one(r#"SELECT "shipID" FROM "character_" WHERE "characterID"=$1"#,&[&(session.char_id.unwrap() as i32)]).await.unwrap().get(0);
        assert_eq!(i64::from(saved),original);
        let w=f.world.lock(); let park=&w.parks[&30000141];
        assert!(park.ships[&original].is_player());
        assert!(!park.ships.contains_key(&capsule));
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn only_one_pilot_can_board_a_hull_and_its_nested_cargo() {
        let f=Database::load().await;
        let owner=pilot(&f,601,1).await;
        let hull=owner.ship_id.unwrap();
        let container=f.id().await;
        let cargo=f.id().await;
        f.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,singleton,quantity) VALUES ($1,3293,$3,$4,5,1,1),($2,34,$3,$1,5,0,42)"#,&[&container,&cargo,&(owner.char_id.unwrap() as i32),&(hull as i32)]).await.unwrap();
        let capsule=switch(&f.ctx(&owner),None).await.unwrap_or_else(|e|panic!("{}",error_text(e)));
        let a=pilot(&f,CAPSULE as i32,2).await;
        let b=pilot(&f,CAPSULE as i32,3).await;
        let mut ca=f.ctx(&a);ca.client_id=2;
        let mut cb=f.ctx(&b);cb.client_id=3;
        let (ra,rb)=tokio::join!(switch(&ca,Some(hull)),switch(&cb,Some(hull)));
        assert_eq!(usize::from(ra.is_ok())+usize::from(rb.is_ok()),1,"exactly one claimant wins");
        let winner=if ra.is_ok(){a.char_id.unwrap()}else{b.char_id.unwrap()};
        let rows=f.db.query(r#"SELECT "ownerID" FROM entity WHERE "itemID"=ANY($1)"#,&[&vec![hull as i32,container,cargo]]).await.unwrap();
        assert_eq!(rows.len(),3);
        assert!(rows.iter().all(|r|i64::from(r.get::<_,i32>(0))==winner));
        let original_owner_ship:i32=f.db.query_one(r#"SELECT "shipID" FROM "character_" WHERE "characterID"=$1"#,&[&(owner.char_id.unwrap() as i32)]).await.unwrap().get(0);
        assert_eq!(i64::from(original_owner_ship),capsule);
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn rejected_commit_restores_live_ship_and_rolls_back_capsule_creation() {
        let f=Database::load().await;
        let session=pilot(&f,601,1).await;
        let who=session.char_id.unwrap();let hull=session.ship_id.unwrap();
        let before:i64=f.db.query_one(r#"SELECT count(*) FROM entity WHERE "ownerID"=$1 AND "typeID"=670"#,&[&(who as i32)]).await.unwrap().get(0);
        let name=format!("embark_fixture_{who}");
        f.db.batch_execute(&format!("ALTER TABLE character_ ADD CONSTRAINT {name} CHECK (\"characterID\" <> {who} OR \"shipID\" = {hull})")).await.unwrap();
        let result=switch(&f.ctx(&session),None).await;
        f.db.batch_execute(&format!("ALTER TABLE character_ DROP CONSTRAINT {name}")).await.unwrap();
        assert!(matches!(result,Err(MoveError::Db(_))));
        let after:i64=f.db.query_one(r#"SELECT count(*) FROM entity WHERE "ownerID"=$1 AND "typeID"=670"#,&[&(who as i32)]).await.unwrap().get(0);
        assert_eq!(before,after);
        assert!(f.world.lock().parks[&30000141].ships[&hull].is_player());
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn station_evacuation_and_boarding_enforce_ownership_and_skills() {
        let f=Database::load().await;
        let mut session=pilot(&f,601,1).await;
        let who=session.char_id.unwrap();let hull=session.ship_id.unwrap();
        session.station_id=Some(60014659);session.solar_system_id=None;
        f.db.execute(r#"UPDATE "character_" SET "stationID"=60014659 WHERE "characterID"=$1"#,&[&(who as i32)]).await.unwrap();
        f.db.execute(r#"UPDATE entity SET "locationID"=60014659,flag=4 WHERE "itemID"=$1"#,&[&(hull as i32)]).await.unwrap();
        f.world.lock().parks.get_mut(&30000141).unwrap().ships.remove(&hull);
        let pod=station_switch(&f.ctx(&session),None).await.unwrap_or_else(|e|panic!("{}",error_text(e)));
        session.ship_id=Some(pod);
        assert_eq!(station_switch(&f.ctx(&session),None).await.unwrap_or_else(|e|panic!("{}",error_text(e))),pod,"already in capsule does not duplicate it");
        let battleship=f.id().await;
        f.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,singleton,quantity) VALUES ($1,638,$2,60014659,4,1,1)"#,&[&battleship,&(who as i32)]).await.unwrap();
        assert!(station_switch(&f.ctx(&session),Some(battleship as i64)).await.is_err(),"untrained pilots cannot board a battleship");
        f.db.execute(r#"UPDATE entity SET "ownerID"=1 WHERE "itemID"=$1"#,&[&(hull as i32)]).await.unwrap();
        assert!(station_switch(&f.ctx(&session),Some(hull)).await.is_err(),"station hangars do not allow another owner's hull");
        f.db.execute(r#"UPDATE entity SET "ownerID"=$2 WHERE "itemID"=$1"#,&[&(hull as i32),&(who as i32)]).await.unwrap();
        assert_eq!(station_switch(&f.ctx(&session),Some(hull)).await.unwrap_or_else(|e|panic!("{}",error_text(e))),hull);
        assert!(f.db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&(pod as i32)]).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn failed_transfer_keeps_damage_and_target_locks_during_database_wait() {
        let f=Database::load().await;
        let session=pilot(&f,601,1).await;
        let enemy=pilot(&f,601,2).await;
        let who=session.char_id.unwrap();let hull=session.ship_id.unwrap();let enemy=enemy.ship_id.unwrap();
        let name=format!("embark_combat_fixture_{who}");
        f.db.batch_execute(&format!("ALTER TABLE character_ ADD CONSTRAINT {name} CHECK (\"characterID\" <> {who} OR \"shipID\" = {hull})")).await.unwrap();
        f.db.execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueFloat") VALUES ($1,3,0) ON CONFLICT ("itemID","attributeID") DO UPDATE SET "valueFloat"=0"#,&[&(hull as i32)]).await.unwrap();
        let mut blocker=crate::db::connect(&f.url).await.unwrap();
        let lock=blocker.transaction().await.unwrap();
        lock.query_one(r#"SELECT "itemID" FROM entity_attributes WHERE "itemID"=$1 AND "attributeID"=3 FOR UPDATE"#,&[&(hull as i32)]).await.unwrap();
        let ctx=f.ctx(&session);
        let during=async {
            tokio::time::timeout(std::time::Duration::from_secs(3),async {
                loop {
                    if f.world.lock().parks[&30000141].ships[&hull].identity_reserved {break;}
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }).await.expect("transfer must reserve while waiting for attribute row");
            {
                let mut w=f.world.lock();let park=w.parks.get_mut(&30000141).unwrap();
                park.ships.get_mut(&hull).unwrap().fight.as_mut().unwrap().vitals.hull_damage=7.0;
                park.ships.get_mut(&enemy).unwrap().fight.as_mut().unwrap().locks.insert(hull,crate::combat::Lock::Locked);
                crate::fight::step(park,crate::combat::now_ms(),&mut crate::heartbeat::Out::default());
                assert!(park.ships[&enemy].fight.as_ref().unwrap().is_locked(hull),"reservation must leave target visible");
            }
            lock.rollback().await.unwrap();
        };
        let (result,())=tokio::join!(switch(&ctx,None),during);
        f.db.batch_execute(&format!("ALTER TABLE character_ DROP CONSTRAINT {name}")).await.unwrap();
        assert!(matches!(result,Err(MoveError::Db(_))));
        let w=f.world.lock();let park=&w.parks[&30000141];
        assert_eq!(park.ships[&hull].fight.as_ref().unwrap().vitals.hull_damage,7.0);
        assert!(!park.ships[&hull].identity_reserved);
        assert!(park.ships[&enemy].fight.as_ref().unwrap().is_locked(hull));
    }
}
