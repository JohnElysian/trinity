//! What losing a ship costs, done once the heartbeat has decided it happened.
//!
//! A ship goes with half of what was in and on it; the other half floats in a
//! can where it died. Its insurance pays out. The pilot ejects in a capsule.
//! A capsule lost costs the implants and, beyond what the clone keeps, skill
//! points from the pilot's biggest skill; the pilot wakes in a fresh Alpha
//! clone at its home station (see `svc::medical`). Every loss is written to
//! the kill log and mailed, in the old killmail form, to the victim and
//! whoever landed the final blow.

use std::collections::BTreeSet;

use eve_marshal::Value;
use tokio_postgres::{Client, GenericClient};
use tracing::{error, info, warn};

use crate::ballpark::{Ship, Vec3};
use crate::combat::{self, attr};
use crate::conn::Server;
use crate::items::{self, ItemTxn, NewItem};
use crate::loot::{self, Stash};
use crate::svc::medical::{GROUP_CLONE, TYPE_CLONE_ALPHA};
use crate::svc::{inflight, space};
use crate::world::Move;

mod docked;
pub mod pending;
pub mod journal;
pub use docked::ArrivalDisposition;

const TYPE_CAPSULE: i64 = 670;
const FLAG_HANGAR: i64 = 4;
const FLAG_IMPLANT: i32 = 89;
const FLAG_SKILL: i32 = 7;
const FLAG_SKILL_IN_TRAINING: i32 = 61;
/// A ship in space sits in its solar system with no flag.
const FLAG_IN_SPACE: i64 = 0;
const ATTR_SKILL_TIME_CONSTANT: i32 = 275;
const ATTR_SKILL_POINTS: i32 = 276;
const ATTR_SKILL_LEVEL: i32 = 280;
/// market_refTypes: "Insurance".
const REF_INSURANCE: i32 = 19;
const OWNER_SYSTEM: i64 = 1;
/// Of what was in a destroyed ship, this share survives into the can.
const SURVIVING_SHARE: f64 = 0.5;

/// Who hit a ship that died, and with what.
#[derive(Debug, Clone,serde::Serialize,serde::Deserialize)]
pub struct Attacker {
    /// A character, or for an NPC minus its type.
    pub who: i64,
    pub ship_type: i64,
    pub weapon_type: i64,
    pub damage: f64,
}

/// Accepted travel moved custody after the source encounter. Keep that
/// placement separate from the encounter recorded in a kill report.
#[derive(Debug, Clone, Copy, PartialEq,serde::Serialize,serde::Deserialize)]
pub struct SpaceArrivalCustody {
    pub system: i64,
    pub at: Vec3,
}

#[derive(Debug, Clone,serde::Serialize,serde::Deserialize)]
pub struct Loss {
    #[serde(skip)]
    pub client_id: i64,
    pub char_id: i64,
    pub ship_id: i64,
    pub ship_type: i64,
    pub system: i64,
    pub pos: Vec3,
    /// A confirmed loss after an accepted space arrival belongs to the
    /// destination inventory. Attribution still describes the source fight.
    #[serde(default)]
    pub arrival_custody: Option<SpaceArrivalCustody>,
    pub capsule: bool,
    /// An abandoned hull retains its owner but has no occupant to eject or pod.
    pub unpiloted: bool,
    /// Who landed the final blow, as in [`Attacker::who`].
    pub final_blow: Option<i64>,
    pub attackers: Vec<Attacker>,
    /// For a bot: the fittings and cargo destroyed with it, (type, quantity).
    pub items: Vec<(i64, i64)>,
}

impl Loss {
    pub fn custody_system(&self) -> i64 {
        self.arrival_custody.as_ref().map_or(self.system, |arrival| arrival.system)
    }

    pub fn custody_position(&self) -> Vec3 {
        self.arrival_custody.as_ref().map_or(self.pos, |arrival| arrival.at)
    }

    pub fn of(ship: &Ship, system: i64, capsule: bool) -> Loss {
        let fight = ship.fight.as_ref();
        let attackers = fight
            .map(|f| {
                f.damage_log
                    .iter()
                    .map(|(who, (ship_type, weapon_type, damage))| Attacker { who: *who, ship_type: *ship_type, weapon_type: *weapon_type, damage: *damage })
                    .collect()
            })
            .unwrap_or_default();
        let final_blow = fight.and_then(|f| f.last_attacker).map(|(who, _)| who);
        Loss {
            client_id: ship.client_id,
            char_id: ship.pilot.char_id,
            ship_id: ship.id,
            ship_type: ship.pilot.type_id,
            system,
            pos: ship.pos,
            arrival_custody: None,
            capsule,
            unpiloted: ship.unpiloted,
            final_blow,
            attackers,
            items: Vec::new(),
        }
    }
}

pub async fn handle(server: &Server, db: &mut Client, loss: Loss) {
    journal::handle(server,db,loss).await;
}

/// A bot's ship destroyed: the kill log and the killmail, as for a player.
/// The rest (a new ship, waking at home) is the bot simulation's.
pub async fn bot_lost(server: &Server, db: &mut Client, loss: Loss) {
    let report = report_text(db, &loss).await;
    let mut destroyed = Vec::new();
    for (t, n) in &loss.items {
        let name = type_name(&*db, *t).await;
        destroyed.push(if *n > 1 { format!("{name}, Qty: {n}") } else { name });
    }
    let items = format!("<items>{}</items>", loss.items.iter().map(|(ty, quantity)| format!(r#"<i t="{ty}" f="0" d="0" x="{quantity}"/>"#)).collect::<String>());
    record(server, db, &loss, report, destroyed, items).await;
}

pub async fn ensure(db: &Client) -> Result<(), tokio_postgres::Error> {
    db.batch_execute(r#"ALTER TABLE "killLog" ADD COLUMN IF NOT EXISTS "killBlob" text NOT NULL DEFAULT '';
        ALTER TABLE "killLogParticipants" ADD COLUMN IF NOT EXISTS "securityStatus" double precision"#).await
}

#[derive(Debug)]
struct LostItem { id: i32, type_id: i32, flag: i32, quantity: i32, parent: i32 }

async fn item_snapshot(db: &impl GenericClient, loss: &Loss) -> Result<Vec<LostItem>, tokio_postgres::Error> {
    let pod = loss.capsule && !loss.unpiloted;
    let root = if pod { loss.char_id } else { loss.ship_id } as i32;
    let sql = r#"WITH RECURSIVE contents AS (
        SELECT "itemID","typeID",flag,quantity,"locationID" FROM entity
        WHERE "locationID"=$1 AND "itemID"<>$2 AND (NOT $3 OR flag=89)
        UNION SELECT e."itemID",e."typeID",e.flag,e.quantity,e."locationID" FROM entity e
            JOIN contents c ON e."locationID"=c."itemID" WHERE e."itemID"<>$2)
        SELECT * FROM contents"#;
    let rows = db.query(sql, &[&root, &(loss.char_id as i32), &pod]).await?;
    Ok(rows.iter().map(|r| LostItem { id:r.get(0), type_id:r.get(1), flag:r.get(2), quantity:r.get(3), parent:r.get(4) }).collect())
}

async fn item_blob(db: &impl GenericClient, before: &[LostItem]) -> Result<String, tokio_postgres::Error> {
    let ids = before.iter().map(|i| i.id).collect::<Vec<_>>();
    let rows = db.query(r#"SELECT "itemID",quantity FROM entity WHERE "itemID"=ANY($1)"#, &[&ids]).await?;
    let survivors = rows.iter().map(|r| (r.get::<_,i32>(0),r.get::<_,i32>(1))).collect::<std::collections::BTreeMap<_,_>>();
    fn xml(item: &LostItem, all: &[LostItem], survivors: &std::collections::BTreeMap<i32,i32>, depth: usize) -> String {
        if depth > 32 { return String::new(); }
        let dropped = survivors.get(&item.id).copied().unwrap_or(0).clamp(0,item.quantity.max(0));
        let children = all.iter().filter(|i| i.parent == item.id).map(|i| xml(i,all,survivors,depth+1)).collect::<String>();
        format!(r#"<i t="{}" f="{}" d="{}" x="{}">{children}</i>"#,item.type_id,item.flag,dropped,item.quantity.max(0)-dropped)
    }
    Ok(format!("<items>{}</items>",before.iter().filter(|i| !ids.contains(&i.parent)).map(|i|xml(i,before,&survivors,0)).collect::<String>()))
}

/// Freeze the contents before reading the loss snapshot. Read it again after
/// acquiring the locks so items that moved while we waited are not included.
/// The XML is calculated before commit and before publishing the loot can.
async fn lock_loss_contents(tx: &tokio_postgres::Transaction<'_>, loss: &Loss) -> Result<(), tokio_postgres::Error> {
    let pod = loss.capsule && !loss.unpiloted;
    let root = if pod { loss.char_id } else { loss.ship_id } as i32;
    tx.query(r#"WITH RECURSIVE contents AS (
        SELECT "itemID" FROM entity WHERE "locationID"=$1 AND "itemID"<>$2 AND (NOT $3 OR flag=89)
        UNION SELECT e."itemID" FROM entity e JOIN contents c ON e."locationID"=c."itemID" WHERE e."itemID"<>$2)
        SELECT e."itemID" FROM entity e JOIN contents c ON c."itemID"=e."itemID" ORDER BY e."itemID" FOR UPDATE OF e"#,
        &[&root,&(loss.char_id as i32),&pod]).await?;
    Ok(())
}

// ---- the ship -----------------------------------------------------------------------------

/// A destroyed fitting or container takes its loaded charges and cargo with
/// it. ItemTxn::destroy intentionally operates on one item, so loss handling
/// locks and revalidates each descendant before recording its notification.
async fn destroy_tree(txn: &mut ItemTxn<'_>, id: i64, parent:i64, pilot: i64) -> Result<(), tokio_postgres::Error> {
    txn.destroy_tree(id,Some(parent),&[pilot]).await
}

/// Returns what was destroyed, by type name, for the killmail.
async fn ship_lost(server: &Server, db: &mut Client, loss: &Loss) -> Option<(Vec<String>, String)> {
    let (char_id, ship_id) = (loss.char_id, loss.ship_id);
    let mut rng = combat::Rng::seeded(combat::now_ms() as u64 ^ ship_id as u64);
    let result = async {
        let mut txn = ItemTxn::begin(db).await?;
        // Boarding/ejection lock character lifecycle, character row, then hull.
        // Keep that order, and do not redirect an owner who already left this
        // ship before the delayed destruction callback reached the database.
        let occupied = if loss.unpiloted {
            false
        } else if crate::svc::login::lock_character_lifecycle(txn.tx(),char_id as i32).await? {
            txn.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2
                AND "solarSystemID"=$3 AND COALESCE("stationID",0)=0 FOR UPDATE"#,
                &[&(char_id as i32),&(ship_id as i32),&(loss.custody_system() as i32)]).await?.is_some()
        } else { false };
        // A repeated heartbeat callback must not create another capsule,
        // charge the same loss twice, or pay its insurance again.
        let Some(hull)=txn.item(ship_id).await? else {return Ok(None);};
        if hull.owner_id!=char_id || hull.type_id!=loss.ship_type || hull.location_id!=loss.custody_system()
            || hull.flag!=FLAG_IN_SPACE || hull.quantity!=1 || !hull.singleton || hull.category_id!=6 {return Ok(None);}
        lock_loss_contents(txn.tx(), loss).await?;
        let before = item_snapshot(txn.tx(), loss).await?;
        let contents: Vec<(i64, String)> = txn
            .tx()
            .query(
                r#"SELECT e."itemID", COALESCE(t."typeName", '') FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                    WHERE e."locationID" = $1 AND e."itemID" <> $2"#,
                &[&(ship_id as i32), &(char_id as i32)],
            )
            .await?
            .iter()
            .map(|r| (r.get::<_, i32>(0) as i64, r.get(1)))
            .collect();
        let mut survivors = Vec::new();
        let mut destroyed = Vec::new();
        for (id, name) in contents {
            if rng.unit() < SURVIVING_SHARE {
                survivors.push(id);
            } else {
                destroy_tree(&mut txn, id, ship_id, char_id).await?;
                destroyed.push(name);
            }
        }
        // Save the can and its complete contents in the same transaction as
        // destruction, so a failed can creation preserves the original ship.
        let stash: Vec<_> = survivors.into_iter().map(Stash::Existing).collect();
        let can = loot::stage_can(&mut txn, loss.custody_system(), loss.custody_position(), char_id, &stash).await?;
        txn.destroy(ship_id).await?;
        crate::svc::clones::ship_destroyed(txn.tx(), ship_id).await?;
        let capsule = if occupied {
            let capsule = txn
                .create(NewItem { type_id: TYPE_CAPSULE, owner_id: char_id, location_id: loss.custody_system(), flag: FLAG_IN_SPACE, quantity: 1, singleton: true })
                .await?;
            txn.set_position(capsule,loss.custody_position().tuple()).await?;
            txn.tx().execute(r#"UPDATE "character_" SET "shipID"=$2 WHERE "characterID"=$1"#,
                &[&(char_id as i32),&(capsule as i32)]).await?;
            // Pilot inventory location follows the same system/station
            // convention as normal travel; character_.shipID is the cockpit.
            txn.tx().execute(r#"UPDATE entity SET "locationID"=$2,x=$3,y=$4,z=$5 WHERE "itemID"=$1"#,
                &[&(char_id as i32),&(loss.custody_system() as i32),&loss.custody_position().x,&loss.custody_position().y,&loss.custody_position().z]).await?;
            Some(capsule)
        } else { None };
        let insurance = claim_insurance(txn.tx(), ship_id, crate::bulkdata::filetime_now()).await?;
        let blob = item_blob(txn.tx(), &before).await?;
        let report=report_text(txn.tx(),loss).await;
        let record=stage_record(txn.tx(),loss,report,destroyed.clone(),blob.clone()).await?;
        let events = txn.commit().await?;
        Ok::<_, tokio_postgres::Error>(Some((capsule, events, can, destroyed, insurance, blob,record)))
    }
    .await;
    let (capsule_id, events, can, destroyed, insurance, blob,record) = match result {
        Ok(Some(v)) => v,
        Ok(None) => return None,
        Err(e) => {
            error!("losing ship {ship_id} of {char_id}: {}", crate::logging::chain(&e));
            return None;
        }
    };
    if loss.unpiloted {
        let clients=server.world.lock().clients_of(char_id);
        for client in clients { items::notify(&server.world,client,events.clone()); }
    } else { items::notify(&server.world, loss.client_id, events); }
    if let Some(payout) = insurance { notify_insurance(server, loss, payout); }
    if let Some(can) = can { loot::show_can(&server.world, loss.custody_system(), can); }
    deliver_record(server,db,record).await;

    let Some(capsule_id)=capsule_id else {
        info!("unoccupied ship {ship_id} of {char_id} destroyed");
        return Some((destroyed,blob));
    };

    if let Err(error)=publish_capsule(server,db,loss,capsule_id).await {
        error!("publishing replacement capsule {capsule_id}: {}",crate::logging::chain(&error));
    }
    info!("{char_id} lost ship {ship_id}; now in capsule {capsule_id}");
    Some((destroyed, blob))
}

/// Settlement is already committed. Lock the character again before loading
/// and publishing, since reconnect/boarding may have won during notification
/// delivery. An existing live capsule owns its motion and connection identity.
async fn publish_capsule(server:&Server,db:&mut Client,loss:&Loss,capsule_id:i64)->Result<(),tokio_postgres::Error> {
    let tx=db.transaction().await?;
    if !crate::svc::login::lock_character_lifecycle(&tx,loss.char_id as i32).await? {return Ok(());}
    if tx.query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2 AND "solarSystemID"=$3
        AND COALESCE("stationID",0)=0 FOR UPDATE"#,&[&(loss.char_id as i32),&(capsule_id as i32),&(loss.custody_system() as i32)]).await?.is_none(){return Ok(());}
    let Some(row)=tx.query_opt(r#"SELECT x,y,z FROM entity WHERE "itemID"=$1 AND "ownerID"=$2 AND "locationID"=$3
        AND "typeID"=670 AND flag=0 AND quantity=1 AND singleton<>0 FOR UPDATE"#,
        &[&(capsule_id as i32),&(loss.char_id as i32),&(loss.custody_system() as i32)]).await? else{return Ok(());};
    let Some(profile)=inflight::profile(&tx,&server.dogma,loss.char_id,capsule_id,&BTreeSet::new(),combat::now_ms()).await? else{return Ok(());};
    let pos=Vec3::new(row.get(0),row.get(1),row.get(2));
    {
        let mut w=server.world.lock();
        if w.parks.get(&loss.custody_system()).is_some_and(|p|p.ships.contains_key(&capsule_id)){return Ok(());}
        // Never send an old completion to a new connection. A reconnect loads
        // its authoritative capsule itself while holding the same lifecycle.
        let client=if w.clients_of(loss.char_id).contains(&loss.client_id){loss.client_id}else{0};
        let mut capsule=Ship::new(capsule_id,client,profile.stats,profile.pilot,pos,Vec3::ZERO);capsule.fight=Some(profile.fighter);
        if let Some(park)=w.parks.get_mut(&loss.custody_system()) {
            park.advance(crate::destiny::tick_now());park.ships.insert(capsule_id,capsule);
            let entries=space::add_ball_entries(park,capsule_id);space::broadcast(&w,loss.custody_system(),None,entries);
        }
        if client!=0 {w.push_move(client,Move::ShipLost {ship_id:capsule_id});}
    }
    tx.commit().await
}

struct InsurancePayout { owner: i64, payout: f64, balance: f64 }

/// Consuming the policy and crediting its wallet are part of the ship's loss
/// transaction. The caller holds the ship lock before acquiring this one.
async fn claim_insurance(tx: &tokio_postgres::Transaction<'_>, ship: i64, now: i64) -> Result<Option<InsurancePayout>, tokio_postgres::Error> {
    use crate::svc::wallet::{post, Entry, Transfer};
    let Some(r) = tx.query_opt(r#"SELECT "contractID", "payout", "ownerID", "startDate", "endDate"
        FROM "insuranceContracts" WHERE "shipID"=$1 FOR UPDATE"#, &[&(ship as i32)]).await? else { return Ok(None); };
    let (contract, payout, owner) = (r.get::<_,i32>(0), r.get::<_,f64>(1), i64::from(r.get::<_,i32>(2)));
    if r.get::<_,i64>(4) <= now {
        tx.execute(r#"DELETE FROM "insuranceContracts" WHERE "contractID"=$1"#, &[&contract]).await?;
        return Ok(None);
    }
    if r.get::<_,i64>(3) > now || !payout.is_finite() || payout <= 0.0 { return Ok(None); }
    let arg = ship.to_string();
    let entry = Entry { ref_type: REF_INSURANCE, payer: OWNER_SYSTEM, payee: owner, arg_id1: &arg, reason: "" };
    let balance = match post(tx, owner, payout, &entry).await {
        Ok(balance) => balance,
        Err(Transfer::Db(e)) => return Err(e),
        Err(_) => { warn!("insurance for {ship}: no payable wallet for {owner}"); return Ok(None); }
    };
    tx.execute(r#"DELETE FROM "insuranceContracts" WHERE "contractID"=$1"#, &[&contract]).await?;
    Ok(Some(InsurancePayout { owner, payout, balance }))
}

fn notify_insurance(server: &Server, loss: &Loss, paid: InsurancePayout) {
    let InsurancePayout { owner, payout, balance } = paid;
    info!("insurance paid {payout} ISK to {owner} for ship {}", loss.ship_id);
    let w = server.world.lock();
    let mut clients = w.clients_of(owner);
    clients.extend(w.corp_members(owner));
    w.notify(&clients, "OnAccountChange", Value::Tuple(vec![Value::bytes("cash"), Value::Int(owner), Value::Double(balance)]));
    let text = format!("Your insurance has paid out {} ISK for the loss of your ship.", crate::heartbeat::fmt_isk(payout));
    let (key, args) = crate::fight::notify(&text);
    w.notify(&clients, "OnRemoteMessage", Value::Tuple(vec![Value::bytes(key), args]));
}

// ---- the capsule --------------------------------------------------------------------------

async fn podded(server: &Server, db: &mut Client, loss: &Loss) -> Option<(Vec<String>, String)> {
    let (char_id, capsule_id) = (loss.char_id, loss.ship_id);
    let result = async {
        let mut txn = ItemTxn::begin(db).await?;
        if !crate::svc::login::lock_character_lifecycle(txn.tx(),char_id as i32).await? {return Ok(None);}
        if txn.tx().query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "shipID"=$2
            AND "solarSystemID"=$3 AND COALESCE("stationID",0)=0 FOR UPDATE"#,
            &[&(char_id as i32),&(capsule_id as i32),&(loss.custody_system() as i32)]).await?.is_none(){return Ok(None);}
        let Some(hull)=txn.item(capsule_id).await? else {return Ok(None);};
        if hull.owner_id!=char_id || hull.type_id!=TYPE_CAPSULE || hull.location_id!=loss.custody_system()
            || hull.flag!=FLAG_IN_SPACE || hull.quantity!=1 || !hull.singleton {return Ok(None);}
        lock_loss_contents(txn.tx(), loss).await?;
        let before = item_snapshot(txn.tx(), loss).await?;
        let mut bounty_balance = None;
        if let Some(killer) = loss.final_blow.filter(|id| *id >= 100_000_000 && *id != char_id) {
            if pay_player_bounty(txn.tx(), char_id, killer).await? > 0.0 {
                let balance = txn.tx().query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#, &[&(killer as i32)]).await?.get::<_,f64>(0);
                bounty_balance = Some((killer, balance));
            }
        }
        txn.destroy_tree(capsule_id,Some(loss.custody_system()),&[char_id]).await?;
        let implants = lose_implants(&mut txn, char_id).await?;
        let skill_loss = lose_skill_points(&txn, char_id).await?;
        // The clone's station, else where they last docked, else their corporation's.
        let chosen = crate::svc::medical::home_station(txn.tx(), char_id).await?;
        crate::svc::medical::replace_clone(txn.tx(), char_id, TYPE_CLONE_ALPHA, chosen).await?;
        let home = txn
            .tx()
            .query_opt(
                r#"SELECT s."stationID", s."solarSystemID", s."constellationID", s."regionID"
                     FROM "character_" c
                     JOIN "staStations" s ON s."stationID" = COALESCE($2, NULLIF(c."stationID", 0),
                          (SELECT co."stationID" FROM corporation co WHERE co."corporationID" = c."corporationID"))
                    WHERE c."characterID" = $1"#,
                &[&(char_id as i32), &chosen.map(|s| s as i32)],
            )
            .await?;
        let Some(home) = home else { return Ok(None) };
        let station = home.get::<_, i32>(0) as i64;
        let pod = txn
            .create(NewItem { type_id: TYPE_CAPSULE, owner_id: char_id, location_id: station, flag: FLAG_HANGAR, quantity: 1, singleton: true })
            .await?;
        txn.tx()
            .execute(
                r#"UPDATE "character_" SET "shipID" = $2, "stationID" = $3, "solarSystemID" = $4,
                          "constellationID" = $5, "regionID" = $6
                    WHERE "characterID" = $1"#,
                &[&(char_id as i32), &(pod as i32), &(station as i32), &home.get::<_, i32>(1), &home.get::<_, i32>(2), &home.get::<_, i32>(3)],
            )
            .await?;
        txn.tx()
            .execute(r#"UPDATE entity SET "locationID" = $2 WHERE "itemID" = $1"#, &[&(char_id as i32), &(station as i32)])
            .await?;
        let blob = item_blob(txn.tx(), &before).await?;
        let report=report_text(txn.tx(),loss).await;
        let record=stage_record(txn.tx(),loss,report,implants.clone(),blob.clone()).await?;
        let events = txn.commit().await?;
        let m = Move::Podded {
            station_id: station,
            solar_system_id: home.get::<_, i32>(1) as i64,
            constellation_id: home.get::<_, i32>(2) as i64,
            region_id: home.get::<_, i32>(3) as i64,
            ship_id: Some(pod),
        };
        Ok::<_, tokio_postgres::Error>(Some((m, events, implants, skill_loss, bounty_balance, blob,record)))
    }
    .await;
    match result {
        Ok(Some((m, events, implants, skill_loss, bounty_balance, blob,record))) => {
            info!("{char_id} podded; waking at {m:?}");
            items::notify(&server.world, loss.client_id, events);
            deliver_record(server,db,record).await;
            let w = server.world.lock();
            if let Some((killer, balance)) = bounty_balance {
                w.notify(&w.clients_of(killer), "OnAccountChange", Value::Tuple(vec![Value::bytes("cash"), Value::Int(killer), Value::Double(balance)]));
            }
            if let Some((skill, lost)) = skill_loss {
                let text = format!(
                    "Your clone could not hold everything you knew: you have lost {} skill points of {skill}. \
                     Upgrade your clone at a station's medical bay to keep more.",
                    crate::heartbeat::fmt_isk(lost)
                );
                let (key, args) = crate::fight::notify(&text);
                w.notify(&[loss.client_id], "OnRemoteMessage", Value::Tuple(vec![Value::bytes(key), args]));
            }
            w.push_move(loss.client_id, m);
            Some((implants, blob))
        }
        Ok(None) => {
            error!("{char_id} podded with no station to wake in");
            None
        }
        Err(e) => {
            error!("podding {char_id}: {}", crate::logging::chain(&e));
            None
        }
    }
}

/// A player's bounty is claimed on capsule destruction, in the same transaction
/// as the loss. Clearing both the aggregate and contributions prevents repeats.
pub(crate) async fn pay_player_bounty(tx: &tokio_postgres::Transaction<'_>, victim: i64, killer: i64) -> Result<f64, tokio_postgres::Error> {
    use crate::svc::wallet::{post, Entry, Transfer};
    if victim == killer || killer < 100_000_000 { return Ok(0.0); }
    let people = tx.query(r#"SELECT "characterID", bounty FROM "character_" WHERE "characterID"=ANY($1)
        ORDER BY "characterID" FOR UPDATE"#, &[&vec![victim as i32,killer as i32]]).await?;
    if !people.iter().any(|r| r.get::<_,i32>(0) as i64 == killer) { return Ok(0.0); }
    let bounty = people.iter().find(|r| r.get::<_,i32>(0) as i64 == victim).map_or(0.0,|r| r.get::<_,f64>(1));
    if !bounty.is_finite() || bounty <= 0.0 { return Ok(0.0); }
    let arg = victim.to_string();
    let entry = Entry {ref_type:17,payer:1,payee:killer,arg_id1:&arg,reason:"Player bounty prize"};
    match post(tx,killer,bounty,&entry).await {
        Ok(_) => {},
        Err(Transfer::Db(e)) => return Err(e),
        Err(_) => return Ok(0.0),
    }
    tx.execute(r#"UPDATE "character_" SET bounty=0 WHERE "characterID"=$1"#, &[&(victim as i32)]).await?;
    tx.execute(r#"DELETE FROM bounties WHERE "characterID"=$1"#, &[&(victim as i32)]).await?;
    Ok(bounty)
}

/// Destroys the implants in the pilot's head; returns their names.
async fn lose_implants(txn: &mut ItemTxn<'_>, char_id: i64) -> Result<Vec<String>, tokio_postgres::Error> {
    let rows = txn
        .tx()
        .query(
            r#"SELECT e."itemID", COALESCE(t."typeName", '') FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                WHERE e."locationID" = $1 AND e."flag" = $2"#,
            &[&(char_id as i32), &FLAG_IMPLANT],
        )
        .await?;
    let mut names = Vec::new();
    for r in &rows {
        txn.destroy_tree(r.get::<_, i32>(0) as i64,Some(char_id),&[char_id]).await?;
        names.push(r.get(1));
    }
    Ok(names)
}

/// Skill points a skill of `rank * 250` time constant has at `level`.
pub(crate) fn points_at(time_constant: f64, level: i64) -> f64 {
    if level <= 0 { 0.0 } else { time_constant * 2f64.powf(2.5 * (level - 1) as f64) }
}

/// The level `points` reach.
fn level_of(time_constant: f64, points: f64) -> i64 {
    (1..=5).rev().find(|l| points + 1e-6 >= points_at(time_constant, *l)).unwrap_or(0)
}

/// What the clone keeps; anything over comes off the biggest skill. Returns
/// (skill, points lost) if anything was.
async fn lose_skill_points(txn: &ItemTxn<'_>, char_id: i64) -> Result<Option<(String, f64)>, tokio_postgres::Error> {
    let saved: Option<f64> = txn
        .tx()
        .query_opt(
            r#"SELECT coalesce(a."valueFloat", a."valueInt"::float8)
                 FROM entity e
                 JOIN "invTypes" t ON t."typeID" = e."typeID"
                 JOIN "dgmTypeAttributes" a ON a."typeID" = e."typeID" AND a."attributeID" = $3
                WHERE e."ownerID" = $1 AND t."groupID" = $2
                ORDER BY 1 DESC LIMIT 1"#,
            &[&(char_id as i32), &GROUP_CLONE, &attr::SKILL_POINTS_SAVED],
        )
        .await?
        .and_then(|r| r.get::<_, Option<f64>>(0));
    let saved = match saved {
        Some(s) => s,
        None => txn
            .tx()
            .query_opt(
                r#"SELECT coalesce("valueFloat", "valueInt"::float8) FROM "dgmTypeAttributes" WHERE "typeID" = $1 AND "attributeID" = $2"#,
                &[&(TYPE_CLONE_ALPHA as i32), &attr::SKILL_POINTS_SAVED],
            )
            .await?
            .and_then(|r| r.get::<_, Option<f64>>(0))
            .unwrap_or(f64::MAX),
    };
    let skills = txn
        .tx()
        .query(
            r#"SELECT e."itemID", COALESCE(t."typeName", ''),
                      COALESCE((SELECT coalesce(a."valueFloat", a."valueInt"::float8) FROM entity_attributes a
                                 WHERE a."itemID" = e."itemID" AND a."attributeID" = $4), 0),
                      COALESCE((SELECT coalesce(d."valueFloat", d."valueInt"::float8) FROM "dgmTypeAttributes" d
                                 WHERE d."typeID" = e."typeID" AND d."attributeID" = $5), 250)
                 FROM entity e JOIN "invTypes" t ON t."typeID" = e."typeID"
                WHERE e."locationID" = $1 AND e."flag" IN ($2, $3)"#,
            &[&(char_id as i32), &FLAG_SKILL, &FLAG_SKILL_IN_TRAINING, &ATTR_SKILL_POINTS, &ATTR_SKILL_TIME_CONSTANT],
        )
        .await?;
    let total: f64 = skills.iter().map(|r| r.get::<_, f64>(2)).sum();
    if total <= saved {
        return Ok(None);
    }
    let Some(biggest) = skills.iter().max_by(|a, b| a.get::<_, f64>(2).total_cmp(&b.get::<_, f64>(2))) else { return Ok(None) };
    let (id, name, points, stc): (i32, String, f64, f64) = (biggest.get(0), biggest.get(1), biggest.get(2), biggest.get(3));
    let lost = (total - saved).min(points);
    let left = points - lost;
    for (a, value) in [(ATTR_SKILL_POINTS, left), (ATTR_SKILL_LEVEL, level_of(stc, left) as f64)] {
        txn.tx()
            .execute(
                r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt","valueFloat") VALUES ($1, $2, NULL, $3)
                   ON CONFLICT ("itemID","attributeID") DO UPDATE SET "valueInt" = NULL, "valueFloat" = $3"#,
                &[&id, &a, &value],
            )
            .await?;
    }
    info!("{char_id} lost {lost} skill points of {name} (clone keeps {saved}, had {total})");
    Ok(Some((name, lost)))
}

// ---- the kill report ------------------------------------------------------------------------

struct Named {
    name: String,
    corp_id: i64,
    corp: String,
    alliance_id: i64,
    alliance: String,
    security: f64,
}

async fn who(db: &impl GenericClient, char_id: i64) -> Named {
    let row = db
        .query_opt(
            r#"SELECT COALESCE(c."characterName", ''), c."corporationID", COALESCE(co."corporationName", ''),
                      COALESCE(co."allianceID", 0), COALESCE(al."allianceName", ''), c."securityRating"
                 FROM "character_" c
                 LEFT JOIN corporation co ON co."corporationID" = c."corporationID"
                 LEFT JOIN alliances al ON al."allianceID" = co."allianceID"
                WHERE c."characterID" = $1"#,
            &[&(char_id as i32)],
        )
        .await;
    match row {
        Ok(Some(r)) => Named {
            name: r.get(0),
            corp_id: r.get::<_, i32>(1) as i64,
            corp: r.get(2),
            alliance_id: r.get::<_, i32>(3) as i64,
            alliance: r.get(4),
            security: r.get(5),
        },
        _ => Named { name: format!("character {char_id}"), corp_id: 0, corp: String::new(), alliance_id: 0, alliance: String::new(), security: 0.0 },
    }
}

async fn type_name(db: &impl GenericClient, type_id: i64) -> String {
    db.query_opt(r#"SELECT COALESCE("typeName", '') FROM "invTypes" WHERE "typeID" = $1"#, &[&(type_id as i32)])
        .await
        .ok()
        .flatten()
        .map_or_else(|| format!("type {type_id}"), |r| r.get(0))
}

fn or_none(s: &str) -> &str {
    if s.is_empty() { "NONE" } else { s }
}

/// The killmail, up to its "Destroyed items" list (which only the loss knows).
async fn report_text(db: &impl GenericClient, loss: &Loss) -> String {
    let victim = who(db, loss.char_id).await;
    let (system, security) = db
        .query_opt(r#"SELECT "solarSystemName", "security" FROM "mapSolarSystems" WHERE "solarSystemID" = $1"#, &[&(loss.system as i32)])
        .await
        .ok()
        .flatten()
        .map_or((String::new(), 0.0), |r| (r.get::<_, Option<String>>(0).unwrap_or_default(), r.get::<_, Option<f64>>(1).unwrap_or(0.0)));
    let mut text = format!(
        "{}\n\nVictim: {}\nAlliance: {}\nCorp: {}\nDestroyed: {}\nSystem: {}\nSecurity: {:.1}\n\nInvolved parties:\n",
        killmail_time(),
        victim.name,
        or_none(&victim.alliance),
        victim.corp,
        type_name(db, loss.ship_type).await,
        system,
        security
    );
    let mut attackers = loss.attackers.clone();
    attackers.sort_by(|a, b| b.damage.total_cmp(&a.damage));
    for a in &attackers {
        let final_blow = if Some(a.who) == loss.final_blow { " (laid the final blow)" } else { "" };
        if a.who > 0 {
            let p = who(db, a.who).await;
            text.push_str(&format!(
                "\nName: {}{final_blow}\nSecurity: {:.1}\nAlliance: {}\nCorp: {}\nShip: {}\nWeapon: {}\n",
                p.name,
                p.security,
                or_none(&p.alliance),
                p.corp,
                type_name(db, a.ship_type).await,
                type_name(db, a.weapon_type).await
            ));
        } else {
            text.push_str(&format!("\nName: {}{final_blow}\n", type_name(db, -a.who).await));
        }
    }
    text
}

fn killmail_time() -> String {
    // FILETIME to a calendar date, without a date library: days since 1601.
    let secs = crate::bulkdata::filetime_now() / 10_000_000 - 11_644_473_600;
    let (days, rest) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}.{m:02}.{d:02} {:02}:{:02}", rest / 3_600, rest % 3_600 / 60)
}

/// Howard Hinnant's days-to-civil, for 1970-based day numbers.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

struct RecordedLoss { subject: String, text: String, recipients: Vec<i64> }

/// Callers that settle custody can include the kill record in that transaction.
async fn stage_record(tx: &tokio_postgres::Transaction<'_>, loss: &Loss, mut text: String, destroyed: Vec<String>, items: String)
    -> Result<RecordedLoss,tokio_postgres::Error> {
    text.push_str("\nDestroyed items:\n\n");
    for item in &destroyed {
        text.push_str(item);
        text.push('\n');
    }
    let victim = who(tx, loss.char_id).await;
    let total: f64 = loss.attackers.iter().map(|a| a.damage).sum();
    let ship_name = type_name(tx, loss.ship_type).await;
    let subject = format!("Kill: {} ({ship_name})", victim.name);
        let final_char = loss.final_blow.filter(|w| *w > 0).unwrap_or(0);
        let kill_id: i64 = tx.query_one(
            r#"INSERT INTO "killLog" ("killTime","victimCharID","victimCorpID","victimAllianceID","victimShipTypeID",
                                      "victimItemID","solarSystemID","locationID","finalBlowID","totalDamage","isPodKill","killBlob")
               VALUES ($1,$2,$3,$4,$5,$6,$7,$7,$8,$9,$10,$11) RETURNING "killID""#,
            &[
                &crate::bulkdata::filetime_now(),
                &(loss.char_id as i32),
                &(victim.corp_id as i32),
                &(victim.alliance_id as i32),
                &(loss.ship_type as i32),
                &(loss.ship_id as i32),
                &(loss.system as i32),
                &(loss.final_blow.unwrap_or(0) as i32),
                &total,
                &i16::from(loss.capsule && !loss.unpiloted),
                &items,
            ],
        )
        .await?
        .get(0);
        for a in loss.attackers.iter().filter(|a| a.who != 0) {
            let p = who(tx, a.who).await;
            tx.execute(
                r#"INSERT INTO "killLogParticipants" ("killID","ownerID","corporationID","allianceID","shipTypeID",
                                                      "weaponTypeID","damageDone","finalBlow","securityStatus")
                   VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)"#,
                &[
                    &kill_id,
                    &(a.who as i32),
                    &(p.corp_id as i32),
                    &(p.alliance_id as i32),
                    &(a.ship_type as i32),
                    &(a.weapon_type as i32),
                    &a.damage,
                    &i16::from(Some(a.who) == loss.final_blow),
                    &if a.who > 0 { Some(p.security) } else { None },
                ],
            )
            .await?;
        }
    tracing::debug!("staged kill {kill_id}: {} lost a {ship_name}", victim.name);
    let mut recipients = vec![loss.char_id];
    if final_char != 0 && final_char != loss.char_id {
        recipients.push(final_char);
    }
    Ok(RecordedLoss {subject,text,recipients})
}

async fn deliver_record(server:&Server,db:&mut Client,record:RecordedLoss) {
    for to in record.recipients {
        mail(server,db,to,&record.subject,&record.text).await;
    }
}

/// The kill log rows, and the killmail to the victim and the final blow.
async fn record(server: &Server, db: &mut Client, loss: &Loss, text: String, destroyed: Vec<String>, items: String) {
    let result=async {
        let tx=db.transaction().await?;
        let record=stage_record(&tx,loss,text,destroyed,items).await?;
        tx.commit().await?;
        Ok::<_,tokio_postgres::Error>(record)
    }.await;
    match result {
        Ok(record)=>deliver_record(server,db,record).await,
        Err(e)=>error!("logging the loss of {}: {}",loss.ship_id,crate::logging::chain(&e)),
    }
}

/// An EVE mail from the system to `to`.
async fn mail(server: &Server, db: &mut Client, to: i64, subject: &str, body: &str) {
    let body = body.replace('\n', "<br>");
    if let Err(e) = crate::svc::chat::deliver(&*db, &server.world, OWNER_SYSTEM, &[to], subject, &body).await {
        error!("mailing {to}: {}", crate::logging::chain(&e));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn derelict_losses_preserve_the_owner_and_occupied_losses_persist_capsule_position() {
        let fixture=crate::test_support::Database::load().await;
        let mut db=crate::db::connect(&fixture.url).await.unwrap();
        let server=Server { table:Default::default(),db:crate::db::Shared::new(fixture.db,fixture.url.clone()),db_url:fixture.url,node_id:1,
            bulk:fixture.bulk,dogma:fixture.dogma,services:crate::svc::registry(),world:fixture.world,
            dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0 };
        let system=30004971i32;
        server.world.lock().parks.insert(system as i64,crate::ballpark::Park::new(system as i64,vec![],crate::destiny::tick_now()));
        // Explicitly empty hull, stale occupied snapshot after owner left,
        // and a genuinely occupied hull all share the same loss transaction.
        for (case,unpiloted,elsewhere) in [(0,true,true),(1,false,true),(2,false,false)] {
            let ids=server.db.snapshot().query("SELECT nextval('dyn_id_seq')::integer FROM generate_series(1,4)",&[]).await.unwrap();
            let (owner,hull,old_pod,cargo)=(ids[0].get::<_,i32>(0),ids[1].get::<_,i32>(0),ids[2].get::<_,i32>(0),ids[3].get::<_,i32>(0));
            let client=case+1;
            let current=if elsewhere {old_pod} else {hull};
            let current_system=if elsewhere {30000142i32} else {system};
            server.db.snapshot().execute(r#"INSERT INTO "character_"("characterID","characterName","shipID","solarSystemID","stationID","corporationID",balance)
                VALUES($1,$2,$3,$4,0,1000002,1000)"#,&[&owner,&format!("Loss fixture {owner}"),&current,&current_system]).await.unwrap();
            for (id,kind,location,flag,quantity) in [(owner,1373,current_system,57,1),(hull,587,system,0,1),(cargo,34,hull,5,2)] {
                server.db.snapshot().execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton,x,y,z)
                    VALUES($1,$2,$3,$4,$5,$6,1,11,22,33)"#,&[&id,&kind,&owner,&location,&flag,&quantity]).await.unwrap();
            }
            if elsewhere {
                server.db.snapshot().execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton,x,y,z)
                    VALUES($1,670,$2,$3,0,1,1,11,22,33)"#,&[&old_pod,&owner,&current_system]).await.unwrap();
            }
            if unpiloted {
                let now=crate::bulkdata::filetime_now();
                server.db.snapshot().execute(r#"INSERT INTO "insuranceContracts"("contractID","shipID","ownerID","startDate","endDate",fraction,amount,payout,"isCorp")
                    VALUES($1,$1,$2,$3,$4,0.5,100,500,0)"#,&[&hull,&owner,&(now-1),&(now+100_000_000)]).await.unwrap();
            }
            let pos=Vec3::new(987654321.25,-7654321.5,54321.75);
            let mut ship=Ship::new(hull as i64,client,crate::ballpark::ShipStats::new(37.0,1.1e6,3.1,320.0,2.0),
                crate::ballpark::Pilot{type_id:587,group_id:25,name:"Lost hull".into(),char_id:owner as i64,
                    corp_id:1000002,alliance_id:None,security:0.0,bounty:0.0},pos,Vec3::ZERO);
            ship.unpiloted=unpiloted;
            let loss=Loss::of(&ship,system as i64,false);
            assert_eq!(loss.unpiloted,unpiloted);
            let mut receiver={
                let mut world=server.world.lock();
                let rx=world.connect(client);
                world.set_presence(client,Some(crate::world::Presence{char_id:owner as i64,corp_id:1000002,
                    solar_system_id:current_system as i64,docked:false,station_id:None,role:0}));
                world.sees(client,current_system as i64);
                rx
            };
            handle(&server,&mut db,loss.clone()).await;
            let row=server.db.snapshot().query_one(r#"SELECT "shipID","solarSystemID",balance FROM "character_" WHERE "characterID"=$1"#,&[&owner]).await.unwrap();
            let after=row.get::<_,i32>(0);
            assert_eq!(row.get::<_,i32>(1),current_system);
            assert_eq!(row.get::<_,f64>(2),if unpiloted {1500.0} else {1000.0});
            let position=server.db.snapshot().query_one(r#"SELECT "locationID",x,y,z FROM entity WHERE "itemID"=$1"#,&[&owner]).await.unwrap();
            let pilot_pos=(position.get::<_,f64>(1),position.get::<_,f64>(2),position.get::<_,f64>(3));
            let mut moves=Vec::new();
            while let Ok(push)=receiver.try_recv() { if let crate::world::Push::Move(m)=push { moves.push(m); } }
            if elsewhere {
                assert_eq!(after,old_pod,"owner's current cockpit is untouched");
                assert_eq!(position.get::<_,i32>(0),current_system);
                assert_eq!(pilot_pos,(11.0,22.0,33.0));
                assert!(moves.is_empty(),"an abandoned hull cannot change its owner's session");
            } else {
                assert_ne!(after,hull);
                assert_eq!(position.get::<_,i32>(0),system);
                assert_eq!(pilot_pos,pos.tuple());
                assert_eq!(moves,vec![Move::ShipLost{ship_id:after as i64}]);
                let capsule=server.db.snapshot().query_one(r#"SELECT "typeID","locationID",x,y,z FROM entity WHERE "itemID"=$1"#,&[&after]).await.unwrap();
                assert_eq!(capsule.get::<_,i32>(0),670);
                assert_eq!(capsule.get::<_,i32>(1),system);
                assert_eq!((capsule.get::<_,f64>(2),capsule.get::<_,f64>(3),capsule.get::<_,f64>(4)),pos.tuple());
                assert_eq!(server.world.lock().parks[&(system as i64)].ships[&(after as i64)].pos,pos);
            }
            assert_eq!(server.db.snapshot().query_one(r#"SELECT count(*) FROM entity WHERE "ownerID"=$1 AND "typeID"=670"#,&[&owner]).await.unwrap().get::<_,i64>(0),1);
            assert!(server.db.snapshot().query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&hull]).await.unwrap().is_none());
            let blob=server.db.snapshot().query_one(r#"SELECT "killBlob" FROM "killLog" WHERE "victimItemID"=$1"#,&[&hull]).await.unwrap().get::<_,String>(0);
            assert!(blob.contains(r#"<i t="34" f="5""#),"cargo remains in the destruction report: {blob}");
            handle(&server,&mut db,loss).await;
            assert_eq!(server.db.snapshot().query_one(r#"SELECT count(*) FROM "killLog" WHERE "victimItemID"=$1"#,&[&hull]).await.unwrap().get::<_,i64>(0),1);
            assert!(receiver.try_recv().is_err(),"duplicate destruction has no repeated effects");
        }
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn insurance_payout_rolls_back_expires_and_settles_once() {
        let fixture = crate::test_support::Database::load().await;
        let owner = fixture.id().await;
        let ship = fixture.id().await;
        let now = crate::bulkdata::filetime_now();
        fixture.db.execute(r#"INSERT INTO "character_" ("characterID","characterName",balance) VALUES ($1,$2,1000)"#, &[&owner,&format!("Insured {owner}")]).await.unwrap();
        let fixture_ref = &fixture;
        let add = |id: i32, target: i32, recipient: i32, end: i64| async move {
            let db = crate::db::connect(&fixture_ref.url).await.unwrap();
            db.execute(r#"INSERT INTO "insuranceContracts" ("contractID","shipID","ownerID","startDate","endDate",fraction,amount,payout,"isCorp")
                VALUES ($1,$2,$3,$4,$5,0.5,100,500,0)"#, &[&id,&target,&recipient,&(now-100),&end]).await.unwrap();
        };
        add(fixture.id().await,ship,owner,now+100_000_000).await;
        let mut first = crate::db::connect(&fixture.url).await.unwrap();
        let tx = first.transaction().await.unwrap();
        assert_eq!(claim_insurance(&tx,ship as i64,now).await.unwrap().unwrap().payout,500.0);
        tx.rollback().await.unwrap();
        assert_eq!(fixture.db.query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,&[&owner]).await.unwrap().get::<_,f64>(0),1000.0);
        let claim = || async {
            let mut db = crate::db::connect(&fixture.url).await.unwrap();
            let tx = db.transaction().await.unwrap();
            let paid = claim_insurance(&tx,ship as i64,now).await.unwrap().is_some();
            tx.commit().await.unwrap();
            paid
        };
        let (a,b) = tokio::join!(claim(),claim());
        assert_ne!(a,b);
        assert_eq!(fixture.db.query_one(r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,&[&owner]).await.unwrap().get::<_,f64>(0),1500.0);
        let expired = fixture.id().await;
        add(fixture.id().await,expired,owner,now).await;
        let tx = first.transaction().await.unwrap();
        assert!(claim_insurance(&tx,expired as i64,now).await.unwrap().is_none());
        tx.commit().await.unwrap();
        assert!(fixture.db.query_opt(r#"SELECT 1 FROM "insuranceContracts" WHERE "shipID"=$1"#,&[&expired]).await.unwrap().is_none());
        let missing_owner = fixture.id().await;
        add(fixture.id().await,missing_owner,missing_owner,now+100_000_000).await;
        let tx = first.transaction().await.unwrap();
        assert!(claim_insurance(&tx,missing_owner as i64,now).await.unwrap().is_none());
        tx.commit().await.unwrap();
        assert!(fixture.db.query_opt(r#"SELECT 1 FROM "insuranceContracts" WHERE "shipID"=$1"#,&[&missing_owner]).await.unwrap().is_some());
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE pointing at a disposable imported database"]
    async fn nested_loss_destroys_contents_and_records_actual_survivors() {
        let mut fixture = crate::test_support::Database::load().await;
        let pilot = i64::from(fixture.id().await);
        let mut txn = ItemTxn::begin(&mut fixture.db).await.unwrap();
        let ship = txn.create(NewItem {type_id:606,owner_id:pilot,location_id:30004971,flag:0,quantity:1,singleton:true}).await.unwrap();
        let doomed = txn.create(NewItem {type_id:3293,owner_id:pilot,location_id:ship,flag:5,quantity:1,singleton:true}).await.unwrap();
        let nested = txn.create(NewItem {type_id:3293,owner_id:pilot,location_id:doomed,flag:5,quantity:1,singleton:true}).await.unwrap();
        let cargo = txn.create(NewItem {type_id:34,owner_id:pilot,location_id:nested,flag:5,quantity:12,singleton:false}).await.unwrap();
        let module = txn.create(NewItem {type_id:484,owner_id:pilot,location_id:ship,flag:27,quantity:1,singleton:true}).await.unwrap();
        let charge = txn.create(NewItem {type_id:230,owner_id:pilot,location_id:module,flag:0,quantity:20,singleton:false}).await.unwrap();
        txn.tx().execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt") VALUES ($1,3,1)"#,&[&(cargo as i32)]).await.unwrap();
        txn.commit().await.unwrap();
        let loss = Loss {client_id:1,char_id:pilot,ship_id:ship,ship_type:606,system:30004971,pos:Vec3::ZERO,arrival_custody:None,capsule:false,unpiloted:false,final_blow:None,attackers:vec![],items:vec![]};
        let mut txn = ItemTxn::begin(&mut fixture.db).await.unwrap();
        lock_loss_contents(txn.tx(),&loss).await.unwrap();
        let before = item_snapshot(txn.tx(),&loss).await.unwrap();
        assert_eq!(before.len(),5);
        destroy_tree(&mut txn,doomed,ship,pilot).await.unwrap();
        txn.move_to(module,30004971,0).await.unwrap();
        let xml = item_blob(txn.tx(),&before).await.unwrap();
        txn.commit().await.unwrap();
        let remaining = fixture.db.query(r#"SELECT "itemID" FROM entity WHERE "itemID"=ANY($1)"#,&[&vec![doomed as i32,nested as i32,cargo as i32,module as i32,charge as i32]]).await.unwrap();
        assert_eq!(remaining.len(),2);
        assert!(remaining.iter().all(|r| [module,charge].contains(&i64::from(r.get::<_,i32>(0)))));
        assert_eq!(fixture.db.query_one(r#"SELECT count(*) FROM entity_attributes WHERE "itemID"=$1"#,&[&(cargo as i32)]).await.unwrap().get::<_,i64>(0),0);
        // A looter may immediately merge this dropped stack into another.
        // The already captured report must retain the actual drop outcome.
        let mut txn = ItemTxn::begin(&mut fixture.db).await.unwrap();
        txn.destroy(charge).await.unwrap();
        txn.commit().await.unwrap();
        assert!(xml.contains(r#"<i t="3293" f="5" d="0" x="1"><i t="3293" f="5" d="0" x="1"><i t="34" f="5" d="0" x="12">"#),"{xml}");
        assert!(xml.contains(r#"<i t="484" f="27" d="1" x="0"><i t="230" f="0" d="20" x="0">"#),"{xml}");
    }

    #[test]
    fn levels_follow_the_skill_curve() {
        let stc = 250.0;
        assert_eq!(points_at(stc, 1), 250.0);
        assert!((points_at(stc, 2) - 1_414.21).abs() < 0.01);
        assert_eq!(level_of(stc, 0.0), 0);
        assert_eq!(level_of(stc, 250.0), 1);
        assert_eq!(level_of(stc, 1_414.0), 1);
        assert_eq!(level_of(stc, 256_000.0), 5);
    }

    #[test]
    fn days_become_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_390), (2025, 10, 29));
    }
}
