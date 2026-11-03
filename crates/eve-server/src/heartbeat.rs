//! The server's pulse: four times a second, every solar system with a player
//! in space moves on (see `fight`). The pulse only decides, under the world
//! lock; what it decided that needs the database is done after the lock is
//! released, on the heartbeat's own connection: charges used, ore mined,
//! bounties paid, ships and capsules lost.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use eve_marshal::Value;
use tokio_postgres::Client;
use tracing::{error, info, warn};

use crate::ballpark::Vec3;
use crate::combat;
use crate::conn::Server;
use crate::items::{self, ItemTxn, NewItem};
use crate::svc::{inflight, space};
use crate::world::{World, WorldState};

const PULSE_MS: u64 = 250;
/// Pulses between looks at slow corporate business (alliance applications
/// coming due): once a minute.
const AFFAIRS_EVERY: u64 = 60_000 / PULSE_MS;
const FLAG_CARGO: i64 = 5;
/// market_refTypes: "Bounty Prize".
const REF_BOUNTY_PRIZE: i32 = 17;
const OWNER_CONCORD: i64 = 1000125;

/// What a pulse decided, to be told or done once the world lock is released.
#[derive(Default)]
pub struct Out {
    /// Destiny entries for everyone in a system, in order.
    destiny: Vec<(i64, Value)>,
    notes: Vec<(i64, &'static str, Value)>,
    pub jobs: Vec<Job>,
    /// Ships whose hit points changed this pulse.
    hurt: BTreeSet<i64>,
    /// Ships whose capacitor changed this pulse.
    drained: BTreeSet<i64>,
}

impl Out {
    pub fn destiny(&mut self, system: i64, entry: Value) {
        self.destiny.push((system, entry));
    }

    pub fn note(&mut self, client_id: i64, event: &'static str, args: Value) {
        self.notes.push((client_id, event, args));
    }

    pub fn hurt(&mut self, ship_id: i64) {
        self.hurt.insert(ship_id);
    }

    pub fn drained(&mut self, ship_id: i64) {
        self.drained.insert(ship_id);
    }

    /// The hurt and drained marks for ships `here` says belong to the caller.
    pub fn take_marks(&mut self, here: impl Fn(i64) -> bool) -> (BTreeSet<i64>, BTreeSet<i64>) {
        let (hurt, rest): (BTreeSet<i64>, BTreeSet<i64>) = std::mem::take(&mut self.hurt).into_iter().partition(|id| here(*id));
        self.hurt = rest;
        let (drained, rest): (BTreeSet<i64>, BTreeSet<i64>) =
            std::mem::take(&mut self.drained).into_iter().partition(|id| here(*id));
        self.drained = rest;
        (hurt, drained)
    }

    #[cfg(test)]
    pub fn notes(&self) -> &[(i64, &'static str, Value)] {
        &self.notes
    }

    #[cfg(test)]
    pub fn destiny_entries(&self) -> &[(i64, Value)] {
        &self.destiny
    }

    /// Sends what was decided; returns the jobs left to do.
    pub fn deliver(self, world: &WorldState) -> Vec<Job> {
        let mut by_system: BTreeMap<i64, Vec<Value>> = BTreeMap::new();
        for (system, e) in self.destiny {
            by_system.entry(system).or_default().push(e);
        }
        for (system, entries) in by_system {
            space::broadcast(world, system, None, entries);
        }
        for (client, event, args) in self.notes {
            world.notify(&[client], event, args);
        }
        self.jobs
    }
}

/// Work a pulse leaves for after the lock.
#[derive(Debug, Clone)]
pub enum Job {
    NpcKill { system:i64 },
    /// A charge stack burned down to `quantity`.
    Ammo { client_id: i64, charge_id: i64, quantity: i64 },
    /// Ore into `ship_id`'s hold, mined by `module_id` on `miner_id` (the ship, or a drone).
    Ore { client_id: i64, char_id: i64, ship_id: i64, miner_id: i64, module_id: i64, system: i64, type_id: i64, quantity: i64 },
    /// A rat's bounty, and what killing it does for security status: (share
    /// of the way to 10, highest it raises it to).
    Bounty { client_id: Option<i64>, char_id: i64, amount: f64, npc_type: i64, security: (f64, f64) },
    /// A module that changes attributes stopped.
    Refit { client_id: i64, char_id: i64, ship_id: i64, system: i64 },
    /// A player's ship (or capsule) was destroyed.
    Lost(Box<crate::losses::Loss>),
    /// A bot's ship was destroyed.
    BotLost(Box<crate::losses::Loss>),
    /// Something was killed with loot on it.
    Loot { system: i64, pos: Vec3, owner_id: i64, stash: Vec<crate::loot::Stash> },
    /// A can's time ran out.
    Scrap { can_id: i64 },
    /// A drone flew home: back into its ship's drone bay.
    DroneHome { client_id: i64, drone_id: i64, ship_id: i64 },
    /// A drone was destroyed.
    DroneLost { client_id: Option<i64>, drone_id: i64, system:i64, owner:i64, generation:Option<i64> },
    /// The last rat of a mission site died.
    SiteCleared { mission_id: i64, char_id: i64 },
    /// A crime, or a kill, changed a pilot's security status by this much.
    Security { char_id: i64, delta: f64 },
    /// A starbase structure's health after a fight.
    StructureDamage { id: i64, shield: f64, armor: f64, hull: f64 },
    /// A tower fell below a quarter shield and may go reinforced.
    Reinforce { system: i64, tower: i64 },
    /// A battery fired a round from its bay.
    StructureAmmo { item_id: i64, quantity: i64 },
    /// A starbase structure was destroyed.
    StructureLost { id: i64, tower: bool },
    /// A starbase structure finished a timed step.
    Structure { id: i64, state: i16, stamp: Option<i64>, tower: Option<i64> },
    /// A cloaking device stopped: the ship is seen again.
    Uncloak { system: i64, ship_id: i64 },
    /// Available power or a new scramble changed the predicted warp at entry.
    WarpResync { system: i64 },
    /// The last room of a deadspace complex was cleared.
    ComplexCleared { complex_id: i64 },
}

pub async fn run(server: Arc<Server>) {
    let (queued,waiting)=tokio::sync::mpsc::unbounded_channel();
    // A stalled established query is just as possible as a failed reconnect.
    // Join two scoped futures: dropping run cancels both; database I/O never
    // holds up the simulation and there is only one persistence consumer.
    tokio::join!(simulation(&server,queued),persistence(&server,waiting));
}

async fn simulation(server:&Server,queued:tokio::sync::mpsc::UnboundedSender<Job>) {
    info!("heartbeat every {PULSE_MS} ms");
    let mut interval = tokio::time::interval(Duration::from_millis(PULSE_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        for job in pulse(&server.world, combat::now_ms()) {
            match job {
                // Decloaking changes the live ballpark, not the database.
                Job::Uncloak{system,ship_id}=>crate::cloak::uncloaked(&mut server.world.lock(),system,ship_id),
                job=>if queued.send(job).is_err() {return;},
            }
        }
    }
}

async fn persistence(server:&Server,mut waiting:tokio::sync::mpsc::UnboundedReceiver<Job>) {
    let mut connection:Option<tokio_postgres::Client>=None;
    let mut reconnect=crate::db::Reconnect::default();
    // The receiver owns undispatched jobs during outages. Once dispatched, an
    // error might mean COMMIT succeeded: only the subsystem's journal can retry.
    let mut interval=tokio::time::interval(Duration::from_millis(PULSE_MS));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pulses=0u64;
    loop {
        interval.tick().await;
        pulses=pulses.wrapping_add(1);
        if connection.as_ref().is_some_and(|db|db.is_closed()) {connection=None;}
        if connection.is_none() {connection=reconnect.poll(&server.db_url).await;}
        let Some(db)=connection.as_mut() else {continue;};
        // Bound each batch so a continuously producing simulation cannot
        // starve checkpoints, journal repair, or other periodic work.
        for _ in 0..128 {
            if db.is_closed() {break;}
            let job=match waiting.try_recv() {
                Ok(job)=>job,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty)=>break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)=>return,
            };
            perform(server, db, job).await;
        }
        if db.is_closed() {continue;}
        crate::svc::fleet::update_bonuses(&server.world, db, &server.dogma).await;
        if pulses%4==0 {crate::svc::boosters::pulse(&server,db).await;}
        if pulses % 20 == 1 {
            crate::drones::checkpoint(&server.world, db).await;
            crate::space_state::checkpoint(&server.world, db).await;
            crate::svc::maintenance::retry_repairs(&server.world, &server.db_url).await;
            crate::losses::pending::retry(&server, db).await;
            crate::losses::journal::retry(&server, db).await;
            crate::travel::retry(&server, db).await;
            crate::svc::jumps::portal::retry(&server, db).await;
            crate::conn::logout::retry(&server,db).await;
            if let Err(e) = crate::svc::fleet::refresh_skills(&server.world, db).await {
                warn!("refreshing fleet skills: {e}");
            }
        }
        if pulses % AFFAIRS_EVERY == 1 {
            crate::svc::alliance::take_effect(&server.world, db, server.alliance_join_delay_secs).await;
            crate::svc::alliance::bill_alliances(db).await;
            crate::svc::war::bill_wars(&server.world, db).await;
            crate::svc::starbase::burn_fuel(&server.world, db).await;
            crate::svc::starbase::sovereignty(db, server.sov_claim_delay_secs).await;
            crate::svc::votes::close_votes(&server.world, db).await;
            crate::complexes::seed(db).await;
            crate::svc::outposts::finish(&server.world, db).await;
            crate::guard::reload(&server.world, db).await;
            crate::guard::refresh_standings(db).await;
        }
    }
}

/// One pulse of every occupied system.
fn pulse(world: &World, now: i64) -> Vec<Job> {
    let mut w = world.lock();
    crate::drones::reconcile(&mut w);
    let mut out = Out::default();
    let tick = crate::destiny::tick_now();
    for park in w.parks.values_mut() {
        crate::pos::step(park, &mut out);
        if park.players().next().is_none() {
            continue;
        }
        park.advance(tick);
        warp_changes(park, &mut out);
        crate::cloak::proximity(park, now, &mut out);
        crate::complexes::step(park, now, &mut out);
        crate::fight::step(park, now, &mut out);
        for e in crate::beacons::expire(park, now) {
            out.destiny(park.solar_system_id, e);
        }
    }
    crate::svc::jumps::orphaned_cynos(&mut w);
    crate::svc::jumps::close_bridges(&mut w, now);
    crate::svc::fleet::pulse(&mut w, now);
    let mut jobs = std::mem::take(&mut w.pending_jobs);
    jobs.extend(out.deliver(&w));
    // A shared RPC connection may recover before this worker does. Quarantine
    // a confirmed death while still holding the simulation lock, even if its
    // SQL job has not started, so reconnect cannot reload the destroyed hull.
    for job in &jobs {
        if let Job::Lost(loss)=job {
            w.pending_ship_losses.entry(loss.ship_id).or_insert_with(||(**loss).clone());
        }
    }
    crate::svc::space::refresh_views(&w);
    jobs
}

fn warp_changes(park: &mut crate::ballpark::Park, out: &mut Out) {
    let mut resync = false;
    for (ship_id, change) in std::mem::take(&mut park.warp_events) {
        let Some(ship) = park.ships.get(&ship_id) else { continue; };
        match change {
            crate::ballpark::WarpChange::Entered { cost, limited } => {
                if cost > 0.0 { out.drained(ship_id); }
                if limited {
                    resync = true;
                    if ship.is_player() {
                        out.note(ship.client_id,"OnRemoteMessage",Value::Tuple(vec![Value::bytes("WarpingWithAvailablePower"),Value::Dict(vec![])]));
                    }
                }
            }
            crate::ballpark::WarpChange::Stopped { scrambled } => {
                resync = true;
                if ship.is_player() {
                    let (key,args) = if scrambled { ("WarpScrambled",Value::Dict(vec![])) } else {
                        ("CustomNotify",Value::Dict(vec![(Value::bytes("notify"),Value::text("Your ship cannot enter warp with its current power or destination."))]))
                    };
                    out.note(ship.client_id,"OnRemoteMessage",Value::Tuple(vec![Value::bytes(key),args]));
                }
            }
        }
    }
    if resync { out.jobs.push(Job::WarpResync { system:park.solar_system_id }); }
}

async fn perform(server: &Server, db: &mut Client, job: Job) {
    match job {
        Job::NpcKill { system } => {
            if let Err(e) = crate::svc::map::record_activity(db,system,2).await { warn!("recording NPC kill: {e}"); }
        }
        Job::Ammo { client_id, charge_id, quantity } => ammo(server, db, client_id, charge_id, quantity).await,
        Job::Ore { client_id, char_id, ship_id, miner_id, module_id, system, type_id, quantity } => {
            ore(server, db, client_id, char_id, (ship_id, miner_id, module_id), system, type_id, quantity).await
        }
        Job::Bounty { client_id, char_id, amount, npc_type, security } => {
            bounty(server, db, client_id, char_id, amount, npc_type).await;
            raise_security(db, char_id, security).await;
        }
        Job::Refit { client_id, char_id, ship_id, system } => {
            inflight::refit(db, &server.dogma, &server.world, client_id, char_id, ship_id, system).await
        }
        Job::Lost(loss) => crate::losses::handle(server, db, *loss).await,
        Job::BotLost(loss) => crate::losses::bot_lost(server, db, *loss).await,
        Job::Loot { system, pos, owner_id, stash } => crate::loot::drop_can(db, &server.world, system, pos, owner_id, stash).await,
        Job::Scrap { can_id } => crate::loot::scrap(db, can_id).await,
        Job::DroneHome { client_id, drone_id, ship_id } => crate::drones::home(server, db, client_id, drone_id, ship_id).await,
        Job::DroneLost { client_id, drone_id, system, owner, generation } => crate::drones::lost(server, db, client_id, drone_id, system, owner, generation).await,
        Job::Security { char_id, delta } => change_security(db, char_id, delta).await,
        Job::SiteCleared { mission_id, char_id } => crate::svc::agent_missions::site_cleared(&server.world, db, mission_id, char_id).await,
        Job::Structure { id, state, stamp, tower } => save_structure(db, id, state, stamp, tower).await,
        Job::StructureDamage { id, shield, armor, hull } => crate::svc::starbase::save_damage(db, id, shield, armor, hull).await,
        Job::Reinforce { system, tower } => crate::svc::starbase::reinforce(&server.world, db, system, tower).await,
        Job::StructureLost { id, tower } => crate::svc::starbase::lost(db, id, tower).await,
        Job::StructureAmmo { item_id, quantity } => crate::guard::save_ammo(db, item_id, quantity).await,
        Job::Uncloak { system, ship_id } => crate::cloak::uncloaked(&mut server.world.lock(), system, ship_id),
        Job::WarpResync { system } => space::resync_after_warp_limit(db, &server.world, system).await,
        Job::ComplexCleared { complex_id } => crate::complexes::cleared(db, complex_id).await,
    }
}

async fn ammo(server: &Server, db: &mut Client, client_id: i64, charge_id: i64, quantity: i64) {
    let result = async {
        let mut txn = ItemTxn::begin(db).await?;
        txn.set_quantity(charge_id, quantity).await?;
        txn.commit().await
    }
    .await;
    match result {
        Ok(events) => items::notify(&server.world, client_id, events),
        Err(e) => error!("using charge {charge_id}: {}", crate::logging::chain(&e)),
    }
}

/// Mined ore goes into the cargo hold, as much as fits; a full hold stops
/// the laser.
#[allow(clippy::too_many_arguments)]
async fn ore(server: &Server, db: &mut Client, client_id: i64, char_id: i64, (ship_id, miner_id, module_id): (i64, i64, i64), system: i64, type_id: i64, quantity: i64) {
    let result = async {
        let room = db
            .query_one(
                r#"SELECT COALESCE(st."capacity", 0)
                          - COALESCE((SELECT SUM(e."quantity" * COALESCE(t."volume", 0))
                                        FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                                       WHERE e."locationID" = $1 AND e."flag" = $2), 0),
                          (SELECT COALESCE("volume", 1) FROM "invTypes" WHERE "typeID" = $3)
                     FROM entity s JOIN "invTypes" st ON st."typeID" = s."typeID"
                    WHERE s."itemID" = $1"#,
                &[&(ship_id as i32), &(FLAG_CARGO as i32), &(type_id as i32)],
            )
            .await?;
        let (room, volume): (f64, f64) = (room.get(0), room.get(1));
        let fits = ((room / volume.max(1e-6)).floor() as i64).clamp(0, quantity);
        if fits == 0 {
            return Ok((0, Vec::new()));
        }
        let mut txn = ItemTxn::begin(db).await?;
        txn.stack_onto(NewItem { type_id, owner_id: char_id, location_id: ship_id, flag: FLAG_CARGO, quantity: fits, singleton: false })
            .await?;
        Ok::<_, tokio_postgres::Error>((fits, txn.commit().await?))
    }
    .await;
    match result {
        Ok((fits, events)) => {
            items::notify(&server.world, client_id, events);
            if fits < quantity {
                let mut w = server.world.lock();
                let mut out = Out::default();
                if let Some(park) = w.parks.get_mut(&system) {
                    let message = crate::fight::notify("Your cargo hold is full; the mining laser shuts down.");
                    crate::fight::stop(park, miner_id, module_id, combat::now_ms(), &mut out, Some(message));
                }
                out.deliver(&w);
            }
        }
        Err(e) => error!("mining {quantity} of {type_id} into {ship_id}: {}", crate::logging::chain(&e)),
    }
}

async fn bounty(server: &Server, db: &mut Client, client_id: Option<i64>, char_id: i64, amount: f64, npc_type: i64) {
    use crate::svc::wallet::{post, Entry, Transfer};
    let arg = npc_type.to_string();
    let result = async {
        let tx = db.transaction().await.map_err(Transfer::Db)?;
        let entry = Entry { ref_type: REF_BOUNTY_PRIZE, payer: OWNER_CONCORD, payee: char_id, arg_id1: &arg, reason: "" };
        let balance = post(&tx, char_id, amount, &entry).await?;
        tx.commit().await.map_err(Transfer::Db)?;
        Ok::<_, Transfer>(balance)
    }
    .await;
    match result {
        Ok(balance) => {
            info!("bounty of {amount} ISK to {char_id} for a {npc_type}");
            if let Some(client) = client_id {
                let w = server.world.lock();
                let cash = Value::Tuple(vec![Value::bytes("cash"), Value::Int(char_id), Value::Double(balance)]);
                w.notify(&[client], "OnAccountChange", cash);
                let text = format!("A bounty of {} ISK has been paid into your wallet.", fmt_isk(amount));
                let (key, args) = crate::fight::notify(&text);
                w.notify(&[client], "OnRemoteMessage", Value::Tuple(vec![Value::bytes(key), args]));
            }
        }
        Err(Transfer::Db(e)) => error!("paying a bounty to {char_id}: {}", crate::logging::chain(&e)),
        Err(_) => warn!("bounty for {char_id}: no wallet"),
    }
}

/// A pirate killed: security status rises by a share of the way to 10, but
/// no killing of that rat raises it past its maximum.
async fn raise_security(db: &mut Client, char_id: i64, (bonus, max_gain): (f64, f64)) {
    if bonus <= 0.0 {
        return;
    }
    let sql = r#"UPDATE "character_"
                    SET "securityRating" = LEAST($3, "securityRating" + (10 - "securityRating") * $2)
                  WHERE "characterID" = $1 AND "securityRating" < $3"#;
    if let Err(e) = db.execute(sql, &[&(char_id as i32), &bonus, &max_gain]).await {
        error!("raising the security status of {char_id}: {}", crate::logging::chain(&e));
    }
}

async fn save_structure(db: &mut Client, id: i64, state: i16, stamp: Option<i64>, tower: Option<i64>) {
    let sql = r#"UPDATE "posStructures" SET "state" = $2, "stateTimestamp" = $3, "controlTowerID" = $4 WHERE "itemID" = $1"#;
    if let Err(e) = db.execute(sql, &[&(id as i32), &state, &stamp, &tower.map(|t| t as i32)]).await {
        error!("saving structure {id}: {}", crate::logging::chain(&e));
    }
}

async fn change_security(db: &mut Client, char_id: i64, delta: f64) {
    let sql = r#"UPDATE "character_" SET "securityRating" = GREATEST(-10, LEAST(10, "securityRating" + $2))
                  WHERE "characterID" = $1 RETURNING "securityRating""#;
    match db.query_opt(sql, &[&(char_id as i32), &delta]).await {
        Ok(Some(r)) => info!("security status of {char_id} {delta:+.3}, now {:.2}", r.get::<_, f64>(0)),
        Ok(None) => {}
        Err(e) => error!("changing the security status of {char_id}: {}", crate::logging::chain(&e)),
    }
}

pub fn fmt_isk(amount: f64) -> String {
    let whole = amount.round() as i64;
    let digits = whole.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isk_is_grouped_in_thousands() {
        assert_eq!(fmt_isk(10_000.0), "10,000");
        assert_eq!(fmt_isk(999.4), "999");
        assert_eq!(fmt_isk(1_234_567.0), "1,234,567");
    }

    #[test]
    fn marks_split_by_park() {
        let mut out = Out::default();
        out.hurt(1);
        out.hurt(2);
        out.drained(2);
        let (hurt, drained) = out.take_marks(|id| id == 2);
        assert_eq!(hurt.into_iter().collect::<Vec<_>>(), vec![2]);
        assert_eq!(drained.into_iter().collect::<Vec<_>>(), vec![2]);
        assert_eq!(out.hurt.into_iter().collect::<Vec<_>>(), vec![1]);
    }

    #[test]
    fn limited_warp_reports_charge_and_resynchronizes_the_native_phase_once() {
        use crate::ballpark::{Park,Pilot,Ship,ShipStats,WarpChange};
        let mut park=Park::new(30_000_001,vec![],0);
        park.ships.insert(1,Ship::new(1,10,ShipStats::new(10.0,1.0e6,1.0,300.0,1.0),Pilot{
            type_id:587,group_id:25,name:"Warp test".into(),char_id:100,corp_id:1000002,
            alliance_id:None,security:0.0,bounty:0.0},Vec3::ZERO,Vec3::ZERO));
        park.warp_events.push((1,WarpChange::Entered{cost:4.0,limited:true}));
        let mut out=Out::default();warp_changes(&mut park,&mut out);
        assert_eq!(out.drained.into_iter().collect::<Vec<_>>(),vec![1]);
        assert!(matches!(out.jobs.as_slice(),[Job::WarpResync{system:30_000_001}]));
        assert_eq!(out.notes[0].1,"OnRemoteMessage");
        assert_eq!(out.notes[0].2.seq().unwrap()[0],Value::bytes("WarpingWithAvailablePower"));
        assert!(park.warp_events.is_empty());
        let mut next=Out::default();warp_changes(&mut park,&mut next);
        assert!(next.jobs.is_empty() && next.notes.is_empty());
    }
}

#[cfg(test)] mod recovery_tests;
