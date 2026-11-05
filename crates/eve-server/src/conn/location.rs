//! Calls that move the session: entering the game, undocking, docking and
//! jumping. Each also moves the ship into or out of its system's ballpark, so
//! the other players there see it arrive and leave.

use std::time::Duration;

use eve_marshal::Value;
use eve_proto::MachoPacket;
use tracing::{error, info, warn};

use super::{Conn, Result};
use crate::ballpark::{Command, Ship, Vec3};
use crate::destiny;
use crate::exceptions::{custom_error, user_error};
use crate::store::{Store, StoreError, Tx, Value as StoreValue};
use crate::svc::jumps::PreparedJump;
#[cfg(test)] use crate::svc::jumps::FuelCost;
use crate::svc::{bound, inflight, space};
use crate::world::Move;

mod departure;
use departure::transition;
#[cfg(test)] use departure::Departure;

struct JumpArrival {
    from: i64,
    system: i64,
    constellation: i64,
    region: i64,
    at: Vec3,
}

#[derive(Clone,Copy)]
enum JumpRequest {
    Beacon {member:i64,beacon:i64},
    Alliance {beacon:i64},
    Portal {pilot:i64,ship:i64,alliance:bool},
    Structure {local:i64,remote:i64},
}

impl JumpRequest {
    async fn prepare(self,ctx:&crate::services::Ctx<'_>,to:i64)->std::result::Result<PreparedJump,String> {
        let (Some(pilot),Some(ship),Some(from))=(ctx.session.char_id,ctx.session.ship_id,ctx.session.solar_system_id) else {
            return Err("You are not in space.".into());
        };
        match self {
            Self::Beacon{member,beacon}=>crate::svc::jumps::prepare_beacon_jump(ctx,pilot,ship,from,member,beacon,to).await,
            Self::Alliance{beacon}=>crate::svc::jumps::prepare_alliance_jump(ctx,pilot,ship,from,beacon,to).await,
            Self::Portal{pilot:host,ship:portal,alliance}=>{
                let (destination,jump)=crate::svc::jumps::prepare_jump_through(ctx,pilot,ship,from,host,portal,alliance).await?;
                if destination!=to {return Err("The portal's destination changed before your jump.".into());}
                Ok(jump)
            }
            Self::Structure{local,remote}=>crate::svc::bridges::prepare(ctx,local,remote,to).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::StoreError;

    /// The first station (by ID): its ID, system, constellation and region.
    fn first_station(store: &Store) -> (i64, i64, i64, i64) {
        store.read(|db| {
            let s = db.table("staStations").rows().min_by_key(|s| s.int("stationID")).unwrap();
            let id = |column: &str| s.int(column).unwrap();
            (id("stationID"), id("solarSystemID"), id("constellationID"), id("regionID"))
        })
    }

    /// `UPDATE table SET .. WHERE <primary key>=id` for fixture changes.
    fn set(store: &Store, table: &str, id: i64, sets: &[(&str, StoreValue)]) {
        store.write(|tx| update_by_id(tx, table, id, sets)).unwrap();
    }

    fn int(store: &Store, table: &str, id: i64, column: &str) -> Option<i64> {
        store.read(|db| db.table(table).get_id(id).and_then(|r| r.int(column)))
    }

    fn entity(id: i64, type_id: i64, owner: i64, location: i64, flag: i64, quantity: i64, singleton: i64) -> Vec<(&'static str, StoreValue)> {
        vec![("itemID", StoreValue::Int(id)), ("typeID", StoreValue::Int(type_id)), ("ownerID", StoreValue::Int(owner)),
            ("locationID", StoreValue::Int(location)), ("flag", StoreValue::Int(flag)), ("quantity", StoreValue::Int(quantity)),
            ("singleton", StoreValue::Int(singleton))]
    }

    #[tokio::test]
    async fn disconnect_saves_actual_capsule_with_unprocessed_clone_jump_session() {
        let fixture = crate::test_support::Database::load().await;
        let pilot = i64::from(fixture.id().await);
        let old_hull = i64::from(fixture.id().await);
        let capsule = i64::from(fixture.id().await);
        let other_hull = i64::from(fixture.id().await);
        let derelict = i64::from(fixture.id().await);
        let (station, origin, _, _) = first_station(&fixture.store);
        let destination = fixture.store.read(|db| {
            db.table("mapSolarSystems").rows().filter_map(|s| s.int("solarSystemID")).filter(|id| *id != origin).min().unwrap()
        });
        // The clone jump has committed and published its capsule, but the
        // connection still has the old docked session when its socket closes.
        fixture.store.write(|tx| {
            tx.insert("character_", &[("characterID", StoreValue::Int(pilot)), ("characterName", StoreValue::Text(format!("Stale clone session {pilot}").into())),
                ("shipID", StoreValue::Int(capsule)), ("stationID", StoreValue::Int(0)), ("solarSystemID", StoreValue::Int(destination))])?;
            tx.insert("entity", &entity(old_hull, 587, pilot, station, 4, 1, 1))?;
            tx.insert("entity", &entity(capsule, 670, pilot, destination, 0, 1, 1))?;
            Ok::<_, StoreError>(())
        }).unwrap();
        let current = crate::session::Session {
            char_id: Some(pilot), ship_id: Some(capsule),
            solar_system_id: Some(destination), solar_system_id2: Some(destination),
            ..Default::default()
        };
        let mut ship = inflight::ship_for_space(&fixture.ctx(&current), pilot, capsule).await.unwrap();
        ship.arrive(Vec3::new(12345.0, -67890.0, 34567.0));
        let fight = ship.fight.as_mut().unwrap();
        fight.vitals.armor_damage = 7.0;
        fight.vitals.capacitor.recharge_ms = 0.0;
        fight.vitals.capacitor.set(crate::combat::now_ms(), 17.0);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, accepted) = tokio::join!(
            tokio::net::TcpStream::connect(listener.local_addr().unwrap()), listener.accept()
        );
        let client = client.unwrap();
        let (socket, peer) = accepted.unwrap();
        let server = std::sync::Arc::new(super::super::Server {
            table: Default::default(), store: fixture.store, node_id: 1,
            bulk: fixture.bulk, dogma: fixture.dogma, services: crate::svc::registry(),
            world: fixture.world, dump_dir: Default::default(),
            alliance_join_delay_secs: 0, sov_claim_delay_secs: 0,
        });
        let mut conn = Conn::new(socket, peer, server, 9292);
        conn.session = crate::session::Session {
            char_id: Some(pilot), ship_id: Some(old_hull),
            station_id: Some(station), solar_system_id2: Some(origin),
            ..Default::default()
        };
        ship.client_id = conn.client_id;
        let other = Ship::new(other_hull, conn.client_id + 1, ship.stats.clone(), ship.pilot.clone(), Vec3::ZERO, Vec3::ZERO);
        let mut empty = Ship::new(derelict, conn.client_id, ship.stats.clone(), ship.pilot.clone(), Vec3::ZERO, Vec3::ZERO);
        empty.unpiloted = true;
        let _notifications = {
            let mut world = conn.server.world.lock();
            let notifications = world.connect(conn.client_id);
            world.set_presence(conn.client_id, conn.presence());
            let mut park = crate::ballpark::Park::new(destination, vec![], destiny::tick_now());
            park.ships.insert(capsule, ship);
            park.ships.insert(other_hull, other);
            park.ships.insert(derelict, empty);
            world.parks.insert(destination, park);
            world.push_move(conn.client_id, Move::Relocate {
                station_id: None, solar_system_id: destination,
                constellation_id: 0, region_id: 0, ship_id: capsule,
            });
            notifications
        };
        drop(client);
        conn.leave_world().await;
        {
            let world = conn.server.world.lock();
            let ships = &world.parks[&destination].ships;
            assert!(!ships.contains_key(&capsule));
            assert!(ships.contains_key(&other_hull), "another client's hull stays in space");
            assert!(ships.contains_key(&derelict), "unpiloted hull stays even with a stale client ID");
            assert!(world.clients_of(pilot).is_empty());
        }
        let saved = conn.server.store.read(|db| {
            let e = db.table("entity").get_id(capsule).unwrap();
            (e.float("x"), e.float("y"), e.float("z"))
        });
        assert_eq!(saved, (Some(12345.0), Some(-67890.0), Some(34567.0)));
        let loaded = inflight::ship_for_space(&conn.ctx(), pilot, capsule).await.unwrap();
        let vitals = &loaded.fight.as_ref().unwrap().vitals;
        assert_eq!(vitals.armor_damage, 7.0);
        assert!(vitals.capacitor.at(crate::combat::now_ms()) < 18.0, "stale docked session must not refill the space capsule");
        assert_eq!(conn.session.station_id, Some(station), "test never consumes the queued relocation");
    }

    #[tokio::test]
    async fn pending_docking_keeps_commands_and_disconnect_responsive_and_commits_on_arrival() {
        let fixture = crate::test_support::Database::load().await;
        let pilot = i64::from(fixture.id().await);
        let hull = i64::from(fixture.id().await);
        let (station, system, _, _) = first_station(&fixture.store);
        fixture.store.write(|tx| {
            tx.insert("character_", &[("characterID", StoreValue::Int(pilot)), ("accountID", StoreValue::Int(pilot)),
                ("characterName", StoreValue::Text(format!("Approach fixture {pilot}").into())), ("shipID", StoreValue::Int(hull)),
                ("stationID", StoreValue::Int(0)), ("solarSystemID", StoreValue::Int(system)), ("corporationID", StoreValue::Int(1000002))])?;
            tx.insert("entity", &[("itemID", StoreValue::Int(hull)), ("typeID", StoreValue::Int(587)), ("ownerID", StoreValue::Int(pilot)),
                ("locationID", StoreValue::Int(system)), ("quantity", StoreValue::Int(1)), ("singleton", StoreValue::Int(1)), ("x", StoreValue::Float(10000.0))])?;
            Ok::<_, StoreError>(())
        }).unwrap();
        let session = crate::session::Session {
            char_id: Some(pilot),
            ship_id: Some(hull),
            solar_system_id: Some(system),
            solar_system_id2: Some(system),
            corp_id: Some(1000002),
            ..Default::default()
        };
        let mut ship = inflight::ship_for_space(&fixture.ctx(&session), pilot, hull)
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let (client, accepted) = tokio::join!(
            tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
            listener.accept()
        );
        let _client = client.unwrap();
        let (socket, peer) = accepted.unwrap();
        let server = std::sync::Arc::new(super::super::Server {
            table: Default::default(),
            store: fixture.store,
            node_id: 1,
            bulk: fixture.bulk,
            dogma: fixture.dogma,
            services: crate::svc::registry(),
            world: fixture.world,
            dump_dir: Default::default(),
            alliance_join_delay_secs: 0,
            sov_claim_delay_secs: 0,
        });
        let mut conn = Conn::new(socket, peer, server, 9191);
        conn.session = session;
        ship.client_id = conn.client_id;
        let _notifications = {
            let mut world = conn.server.world.lock();
            let notifications = world.connect(conn.client_id);
            world.set_presence(conn.client_id, conn.presence());
            let station_ball = crate::ballpark::Celestial {
                id: station,
                type_id: 1529,
                group_id: 15,
                name: "Docking fixture".into(),
                pos: Vec3::ZERO,
                radius: 1000.0,
                jump: None,
            };
            let mut park =
                crate::ballpark::Park::new(system, vec![station_ball], destiny::tick_now());
            park.ships.insert(hull, ship);
            world.parks.insert(system, park);
            notifications
        };
        let request = MachoPacket::call_req(Value::None);
        tokio::time::timeout(
            Duration::from_secs(1),
            conn.dock(&request, station, false),
        )
        .await
        .expect("approach must not hold the connection until arrival")
        .unwrap();
        assert!(conn.pending_dock.is_some());
        let stop = crate::services::Call {
            service: "beyonce".into(),
            method: "Stop".into(),
            args: vec![],
            kwargs: vec![],
        };
        conn.handle_call(&request, &stop).await.unwrap();
        assert!(conn.pending_dock.is_none());
        assert!(matches!(
            conn.server.world.lock().parks[&system].ships[&hull].command,
            Command::Stop
        ));

        conn.dock(&request, station, false).await.unwrap();
        assert!(conn.pending_dock.is_some());
        tokio::time::timeout(Duration::from_secs(1), conn.leave_world())
            .await
            .expect("disconnect must not wait for docking");
        assert!(conn.pending_dock.is_none());
        assert!(!conn.server.world.lock().parks[&system]
            .ships
            .contains_key(&hull));
        assert!(conn.server.world.lock().clients_of(pilot).is_empty());

        // A fresh character selection reconstructs the ship before GetFormations.
        let _reconnected = conn.server.world.lock().connect(conn.client_id);
        conn.session = crate::session::Session {
            user_id: Some(pilot),
            ..Default::default()
        };
        conn.select_character(&request, pilot).await.unwrap();
        assert_eq!(
            conn.session.station_id, None,
            "persisted stationID=0 means space, not station zero"
        );
        assert_eq!(conn.session.solar_system_id, Some(system));
        assert!(conn.server.world.lock().parks[&system]
            .ships
            .contains_key(&hull));
        conn.dock(&request, station, false).await.unwrap();
        assert!(conn.pending_dock.is_some());
        conn.server
            .world
            .lock()
            .parks
            .get_mut(&system)
            .unwrap()
            .ships
            .get_mut(&hull)
            .unwrap()
            .pos = Vec3::new(2000.0, 0.0, 0.0);
        conn.poll_docking().await.unwrap();
        assert!(conn.pending_dock.is_none());
        assert_eq!(conn.session.station_id, Some(station));
        assert!(!conn.server.world.lock().parks[&system]
            .ships
            .contains_key(&hull));
        assert_eq!(int(&conn.server.store, "character_", pilot, "stationID"), Some(station));
    }

    #[tokio::test]
    async fn dock_undock_and_reconnect_persist_all_locations_or_none() {
        let db = crate::test_support::Database::load().await;
        let pilot = i64::from(db.id().await);
        let hull = i64::from(db.id().await);
        let (station, system, _, _) = first_station(&db.store);
        db.store.write(|tx| {
            tx.insert("character_", &[("characterID", StoreValue::Int(pilot)), ("characterName", StoreValue::Text(format!("Dock fixture {pilot}").into())),
                ("shipID", StoreValue::Int(hull)), ("stationID", StoreValue::Int(station)), ("solarSystemID", StoreValue::Int(system))])?;
            tx.insert("entity", &entity(hull, 587, pilot, station, 4, 1, 1))?;
            tx.insert("entity", &entity(pilot, 1377, pilot, station, 0, 1, 1))?;
            Ok::<_, StoreError>(())
        }).unwrap();
        let session = crate::session::Session {
            char_id: Some(pilot),
            ship_id: Some(hull),
            station_id: Some(station),
            solar_system_id2: Some(system),
            ..Default::default()
        };
        let mut ship = inflight::ship_for_space(&db.ctx(&session), pilot, hull)
            .await
            .unwrap();
        ship.pos = Vec3::new(1234.0, 5678.0, 9012.0);
        let fight = ship.fight.as_mut().unwrap();
        fight.vitals.armor_damage = 12.0;
        fight.vitals.capacitor.recharge_ms = 0.0;
        fight.vitals.capacitor.set(crate::combat::now_ms(), 37.0);
        persist_undock(&db.store, pilot, station, system, &ship)
            .await
            .unwrap();
        let saved = db.store.read(|db| {
            let s = db.table("entity").get_id(hull).unwrap();
            (int_of(db, "character_", pilot, "stationID"), s.int("locationID"), int_of(db, "entity", pilot, "locationID"),
                (s.float("x"), s.float("y"), s.float("z")))
        });
        assert_eq!(saved.0, Some(0));
        assert_eq!(saved.1, Some(system));
        assert_eq!(saved.2, Some(system));
        assert_eq!(saved.3, (Some(1234.0), Some(5678.0), Some(9012.0)));
        assert!(
            persist_undock(&db.store, pilot, station, system, &ship)
                .await
                .is_err()
        );
        let resumed = reconnect_ship(&db.ctx(&session), pilot, hull, system)
            .await
            .unwrap();
        assert_eq!(resumed.pos, ship.pos);
        assert_eq!(resumed.fight.as_ref().unwrap().vitals.armor_damage, 12.0);
        assert!(
            resumed
                .fight
                .as_ref()
                .unwrap()
                .vitals
                .capacitor
                .at(crate::combat::now_ms())
                < 38.0
        );
        set(&db.store, "entity", hull, &[("ownerID", StoreValue::Int(0))]);
        assert!(
            persist_dock(&db.store, pilot, system, station, &ship)
                .await
                .is_err()
        );
        assert!(
            reconnect_ship(&db.ctx(&session), pilot, hull, system)
                .await
                .is_err()
        );
        assert_eq!(int(&db.store, "character_", pilot, "stationID"), Some(0));
        set(&db.store, "entity", hull, &[("ownerID", StoreValue::Int(pilot))]);
        persist_dock(&db.store, pilot, system, station, &ship)
            .await
            .unwrap();
        let saved = db.store.read(|db| {
            (int_of(db, "character_", pilot, "stationID"), int_of(db, "entity", hull, "locationID"),
                int_of(db, "entity", pilot, "locationID"), int_of(db, "entity", hull, "flag"))
        });
        assert_eq!(saved, (Some(station), Some(station), Some(station), Some(4)));
        let charges = db.store.read(|db| {
            [18, 264].iter().filter(|a| db.table("entity_attributes").get(&[StoreValue::Int(hull), StoreValue::Int(**a)]).is_some()).count()
        });
        assert_eq!(charges, 0);
        let other = db.store.read(|db| {
            let s = db.table("mapSolarSystems").rows().filter(|s| s.int("solarSystemID") != Some(system))
                .min_by_key(|s| s.int("solarSystemID")).unwrap();
            let id = |column: &str| s.int(column).unwrap();
            (id("solarSystemID"), id("constellationID"), id("regionID"))
        });
        ship.pos = Vec3::new(999.0, 888.0, 777.0);
        persist_relocation(
            &db.store,
            pilot,
            system,
            Some(station),
            other.0,
            None,
            other.1,
            other.2,
            &ship,
        )
        .await
        .unwrap();
        assert_eq!(
            reconnect_ship(
                &db.ctx(&session),
                pilot,
                hull,
                other.0
            )
            .await
            .unwrap()
            .pos,
            ship.pos
        );
    }

    fn int_of(db: &crate::store::Db, table: &str, id: i64, column: &str) -> Option<i64> {
        db.table(table).get_id(id).and_then(|r| r.int(column))
    }

    #[tokio::test]
    async fn jump_persistence_moves_pilot_and_ship_or_neither() {
        let fixture = crate::test_support::Database::load().await;
        let pilot = i64::from(fixture.id().await);
        let ship = i64::from(fixture.id().await);
        fixture.store.write(|tx| {
            tx.insert("character_", &[("characterID", StoreValue::Int(pilot)), ("characterName", StoreValue::Text(format!("Jump test {pilot}").into())),
                ("shipID", StoreValue::Int(ship)), ("solarSystemID", StoreValue::Int(30000142))])?;
            tx.insert("entity", &[("itemID", StoreValue::Int(ship)), ("typeID", StoreValue::Int(587)), ("ownerID", StoreValue::Int(pilot)),
                ("locationID", StoreValue::Int(30000142)), ("quantity", StoreValue::Int(1)), ("singleton", StoreValue::Int(1))])?;
            Ok::<_, StoreError>(())
        }).unwrap();
        let at = Vec3::new(123.0, 456.0, 789.0);
        let mut arrival = JumpArrival {
            from: 30000142,
            system: 30000143,
            constellation: 20000020,
            region: 10000002,
            at,
        };
        persist_jump_state(
            &fixture.store,
            pilot,
            ship,
            &arrival,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let saved = fixture.store.read(|db| {
            let e = db.table("entity").get_id(ship).unwrap();
            (int_of(db, "character_", pilot, "solarSystemID"), e.int("locationID"), (e.float("x"), e.float("y"), e.float("z")))
        });
        assert_eq!((saved.0, saved.1), (Some(30000143), Some(30000143)));
        assert_eq!(saved.2, (Some(123.0), Some(456.0), Some(789.0)));
        set(&fixture.store, "entity", ship, &[("ownerID", StoreValue::Int(0))]);
        arrival.from = 30000143;
        arrival.system = 30000144;
        assert!(persist_jump_state(
            &fixture.store,
            pilot,
            ship,
            &arrival,
            None,
            None,
            None
        )
        .await
        .is_err());
        assert_eq!(
            int(&fixture.store, "character_", pilot, "solarSystemID"),
            Some(30000143),
            "ship write failure rolls back the pilot write"
        );
        assert!(persist_jump_state(
            &fixture.store,
            pilot,
            ship + 1,
            &arrival,
            None,
            None,
            None
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn paid_jump_failure_and_duplicate_completion_never_lose_or_repeat_payment() {
        let fixture = crate::test_support::Database::load().await;
        let pilot = i64::from(fixture.id().await);
        let ship_id = i64::from(fixture.id().await);
        let fuel = i64::from(fixture.id().await);
        fixture.store.write(|tx| {
            tx.insert("character_", &[("characterID", StoreValue::Int(pilot)), ("characterName", StoreValue::Text(format!("Paid jump {pilot}").into())),
                ("shipID", StoreValue::Int(ship_id)), ("solarSystemID", StoreValue::Int(30000142))])?;
            tx.insert("entity", &[("itemID", StoreValue::Int(ship_id)), ("typeID", StoreValue::Int(587)), ("ownerID", StoreValue::Int(pilot)),
                ("locationID", StoreValue::Int(30000142)), ("quantity", StoreValue::Int(1)), ("singleton", StoreValue::Int(1))])?;
            tx.insert("entity", &[("itemID", StoreValue::Int(fuel)), ("typeID", StoreValue::Int(16273)), ("ownerID", StoreValue::Int(pilot)),
                ("locationID", StoreValue::Int(ship_id)), ("flag", StoreValue::Int(5)), ("quantity", StoreValue::Int(8))])?;
            Ok::<_, StoreError>(())
        }).unwrap();
        let profile = fixture.store.read(|db| inflight::profile(
            db,
            &fixture.dogma,
            pilot,
            ship_id,
            &Default::default(),
            crate::combat::now_ms(),
        ))
        .unwrap()
        .unwrap();
        let mut ship = Ship::new(
            ship_id,
            1,
            profile.stats,
            profile.pilot,
            Vec3::ZERO,
            Vec3::ZERO,
        );
        ship.fight = Some(profile.fighter);
        ship.fight.as_mut().unwrap().vitals.capacitor =
            crate::combat::Charged::full(1000.0, 0.0, crate::combat::now_ms());
        let before = ship.fight.as_ref().unwrap().vitals.capacitor;
        let arrival = JumpArrival {
            from: 30000142,
            system: 30000143,
            constellation: 20000020,
            region: 10000002,
            at: Vec3::new(1.0, 2.0, 3.0),
        };
        let jump = PreparedJump {
            at: arrival.at,
            fuel: FuelCost {
                cargo: ship_id,
                type_id: 16273,
                quantity: 3,
                flag: Some(5),
            },
            capacitor: 100.0,
            clients: vec![],
        };
        // The ship was transferred or destroyed during the jump-out delay.
        set(&fixture.store, "entity", ship_id, &[("ownerID", StoreValue::Int(0))]);
        for _ in 0..2 {
            assert!(
                complete_paid_jump(&fixture.store, pilot, &mut ship, &arrival, &jump, None)
                    .await
                    .is_err()
            );
            assert_eq!(ship.fight.as_ref().unwrap().vitals.capacitor, before);
        }
        assert_eq!(
            int(&fixture.store, "entity", fuel, "quantity"), Some(8),
            "failed callbacks never debit or manufacture a refund"
        );
        set(&fixture.store, "entity", ship_id, &[("ownerID", StoreValue::Int(pilot))]);
        // Fuel available at preparation was consumed during the delay.
        set(&fixture.store, "entity", fuel, &[("quantity", StoreValue::Int(2))]);
        assert!(
            complete_paid_jump(&fixture.store, pilot, &mut ship, &arrival, &jump, None)
                .await
                .is_err()
        );
        assert_eq!(ship.fight.as_ref().unwrap().vitals.capacitor, before);
        assert_eq!(
            (int(&fixture.store, "character_", pilot, "solarSystemID"), int(&fixture.store, "entity", ship_id, "locationID")),
            (Some(30000142), Some(30000142)),
            "fuel refusal rolls back both arrival writes"
        );
        set(&fixture.store, "entity", fuel, &[("quantity", StoreValue::Int(8))]);
        complete_paid_jump(&fixture.store, pilot, &mut ship, &arrival, &jump, None)
            .await
            .unwrap();
        assert!(
            complete_paid_jump(&fixture.store, pilot, &mut ship, &arrival, &jump, None)
                .await
                .is_err()
        );
        assert_eq!(
            ship.fight
                .as_ref()
                .unwrap()
                .vitals
                .capacitor
                .at(crate::combat::now_ms()),
            900.0
        );
        let state = fixture.store.read(|db| {
            (int_of(db, "entity", fuel, "quantity"),
                db.table("entity_attributes").get(&[StoreValue::Int(ship_id), StoreValue::Int(18)]).and_then(|a| a.float("valueFloat")))
        });
        assert_eq!(
            state.0,
            Some(5),
            "successful completion debits only once"
        );
        assert_eq!(
            state.1,
            Some(900.0),
            "capacitor is saved with the arrival"
        );
        let next = JumpArrival {
            from: 30000143,
            system: 30000144,
            ..arrival
        };
        let (a, b) = tokio::join!(
            persist_jump_state(
                &fixture.store,
                pilot,
                ship_id,
                &next,
                Some(&jump.fuel),
                Some(800.0),
                None
            ),
            persist_jump_state(
                &fixture.store,
                pilot,
                ship_id,
                &next,
                Some(&jump.fuel),
                Some(800.0),
                None
            )
        );
        assert_ne!(
            a.is_ok(),
            b.is_ok(),
            "concurrent duplicate completions have one winner"
        );
        assert_eq!(int(&fixture.store, "entity", fuel, "quantity"), Some(2));
    }
}

/// Commit the character and its ship together before publishing a successful
/// jump. A failed write leaves the pilot and ship at their original location.
#[cfg(test)]
async fn persist_jump_state(
    store: &Store,
    char_id: i64,
    ship_id: i64,
    arrival: &JumpArrival,
    fuel: Option<&FuelCost>,
    capacitor: Option<f64>,
    fighter: Option<&crate::combat::Fighter>,
) -> std::result::Result<Vec<Value>, String> {
    persist_jump_state_guarded(store,char_id,ship_id,arrival,fuel,capacitor,fighter,None).await
}

#[cfg(test)]
async fn persist_jump_state_guarded(
    store:&Store,char_id:i64,ship_id:i64,arrival:&JumpArrival,fuel:Option<&FuelCost>,
    capacitor:Option<f64>,fighter:Option<&crate::combat::Fighter>,departure:Option<&Departure<'_>>,
) -> std::result::Result<Vec<Value>,String> {
    let failed = |error: StoreError| {
        error!("saving jump: {}", crate::logging::chain(&error));
        "Your jump could not be saved. Please try again.".to_string()
    };
    store.write(|tx| -> std::result::Result<Vec<Value>, transition::Failure> {
        let mut txn = crate::items::ItemTxn::new(tx);
        if txn.db().table("character_").get_id(char_id).is_none() {
            return Err("Your character is no longer available for this jump.".into());
        }
        // The source predicates are the completion guard: a repeated callback
        // from the previous system cannot debit fuel or capacitor a second time.
        let pilot=txn.db().table("character_").get_id(char_id).filter(|c|c.int("shipID")==Some(ship_id)
            && c.int("solarSystemID")==Some(arrival.from) && c.int("stationID").unwrap_or(0)==0).map(|c|c.id);
        let ship=txn.db().table("entity").get_id(ship_id)
            .filter(|e|e.int("ownerID")==Some(char_id) && e.int("locationID")==Some(arrival.from)).map(|e|e.id);
        let (Some(pilot),Some(ship))=(pilot,ship) else {
            return Err("Your ship has moved or is no longer available for this jump.".into());
        };
        txn.tx().update("character_",pilot,&[("solarSystemID",StoreValue::Int(arrival.system)),
            ("constellationID",StoreValue::Int(arrival.constellation)),("regionID",StoreValue::Int(arrival.region))]).map_err(failed)?;
        txn.tx().update("entity",ship,&[("locationID",StoreValue::Int(arrival.system)),
            ("x",StoreValue::Float(arrival.at.x)),("y",StoreValue::Float(arrival.at.y)),("z",StoreValue::Float(arrival.at.z))]).map_err(failed)?;
        if let Some(departure)=departure {departure.check_endpoints(txn.db())?;}
        update_by_id(txn.tx(),"entity",char_id,&[("locationID",StoreValue::Int(arrival.system))]).map_err(failed)?;
        if let Some(fuel) = fuel {
            fuel.debit(&mut txn)?;
        }
        if let Some(fighter) = fighter {
            inflight::write_vitals(txn.tx(), ship_id, fighter, false).map_err(failed)?;
        }
        if let Some(capacitor) = capacitor {
            txn.tx().upsert("entity_attributes",&["itemID","attributeID"],
                &[("itemID",StoreValue::Int(ship_id)),("attributeID",StoreValue::Int(18)),("valueInt",StoreValue::Null),("valueFloat",StoreValue::Float(capacitor))],
                &[("valueInt",StoreValue::Null),("valueFloat",StoreValue::Float(capacitor))]).map_err(failed)?;
        }
        if let Some(departure)=departure {departure.accept()?;}
        Ok(txn.events())
    }).map_err(|failure|failure.to_string())
}

fn travel_failed(error: StoreError) -> String {
    error!("saving travel: {}", crate::logging::chain(&error));
    "Your ship's location could not be saved. Please try again.".into()
}

/// `UPDATE table SET .. WHERE <primary key>=id`: no row, no change.
fn update_by_id(tx:&mut Tx<'_>,table:&str,id:i64,sets:&[(&str,StoreValue)])->std::result::Result<(),StoreError> {
    let Some(row)=tx.table(table).get_id(id).map(|r|r.id) else {return Ok(())};
    tx.update(table,row,sets)
}

#[cfg(test)]
fn restore_departure(world:&mut crate::world::WorldState,system:i64,ship:Ship,configuration:Option<crate::svc::maintenance::Configuration>) {
    // A refused transition never left the system. Preserve its transient bay
    // preference only for the same pilot, hull and source system.
    if let Some(configuration)=configuration.filter(|c|c.system==system && c.pilot==ship.pilot.char_id) {
        world.maintenance.insert(ship.id,configuration);
    }
    space::add_ship(world,system,ship);
}

async fn persist_undock(
    store: &Store,
    char_id: i64,
    station: i64,
    system: i64,
    ship: &Ship,
) -> std::result::Result<(), String> {
    store.write(|tx| -> std::result::Result<(), transition::Failure> {
        let in_system=tx.table("staStations").get_id(station).is_some_and(|s|s.int("solarSystemID")==Some(system));
        let pilot=tx.table("character_").get_id(char_id).filter(|c|in_system && c.int("shipID")==Some(ship.id)
            && c.int("stationID")==Some(station) && c.int("solarSystemID")==Some(system)).map(|c|c.id);
        if crate::losses::pending::blocked_character(tx.db(),char_id).map_err(travel_failed)? {
            return Err("Your station arrival is awaiting its final combat state. Try again shortly.".into());
        }
        let moved=tx.table("entity").get_id(ship.id).filter(|e|e.int("ownerID")==Some(char_id)
            && e.int("locationID")==Some(station) && e.int("flag")==Some(4)).map(|e|e.id);
        let (Some(pilot),Some(moved))=(pilot,moved) else {
            return Err("Your active ship is no longer in that station.".into());
        };
        tx.update("character_",pilot,&[("stationID",StoreValue::Int(0))]).map_err(travel_failed)?;
        tx.update("entity",moved,&[("locationID",StoreValue::Int(system)),("flag",StoreValue::Int(0)),
            ("x",StoreValue::Float(ship.pos.x)),("y",StoreValue::Float(ship.pos.y)),("z",StoreValue::Float(ship.pos.z))]).map_err(travel_failed)?;
        update_by_id(tx,"entity",char_id,&[("locationID",StoreValue::Int(system))]).map_err(travel_failed)?;
        if let Some(fight) = &ship.fight {
            inflight::write_vitals(tx, ship.id, fight, false).map_err(travel_failed)?;
        }
        Ok(())
    }).map_err(|failure|failure.to_string())
}

#[cfg(test)]
async fn persist_dock(
    store: &Store,
    char_id: i64,
    from: i64,
    station: i64,
    ship: &Ship,
) -> std::result::Result<(i64, i64), String> {
    store.write(|tx| -> std::result::Result<(i64, i64), transition::Failure> {
        let place=tx.table("staStations").get_id(station).filter(|s|s.int("solarSystemID")==Some(from))
            .map(|s|(s.int("constellationID").unwrap_or(0),s.int("regionID").unwrap_or(0)))
            .ok_or("That station is not in this solar system.")?;
        let pilot=tx.table("character_").get_id(char_id).filter(|c|c.int("shipID")==Some(ship.id)
            && c.int("solarSystemID")==Some(from) && c.int("stationID").unwrap_or(0)==0).map(|c|c.id);
        let moved=tx.table("entity").get_id(ship.id)
            .filter(|e|e.int("ownerID")==Some(char_id) && e.int("locationID")==Some(from)).map(|e|e.id);
        let (Some(pilot),Some(moved))=(pilot,moved) else {
            return Err("Your ship is no longer available to dock.".into());
        };
        tx.update("character_",pilot,&[("stationID",StoreValue::Int(station)),
            ("constellationID",StoreValue::Int(place.0)),("regionID",StoreValue::Int(place.1))]).map_err(travel_failed)?;
        tx.update("entity",moved,&[("locationID",StoreValue::Int(station)),("flag",StoreValue::Int(4))]).map_err(travel_failed)?;
        update_by_id(tx,"entity",char_id,&[("locationID",StoreValue::Int(station))]).map_err(travel_failed)?;
        if let Some(fight) = &ship.fight {
            inflight::write_vitals(tx, ship.id, fight, true).map_err(travel_failed)?;
        }
        Ok(place)
    }).map_err(|failure|failure.to_string())
}

pub(super) fn publish_reconnect(world:&mut crate::world::WorldState,system:i64,loaded:Ship)->std::result::Result<(),&'static str> {
    if world.pending_ship_losses.contains_key(&loaded.id) || world.pending_logouts.contains_key(&loaded.id) {
        return Err("Your ship's final state is still being saved. Try again shortly.");
    }
    let Some(park)=world.parks.get_mut(&system) else {return Err("Your solar system is unavailable.");};
    if let Some(live)=park.ships.get_mut(&loaded.id) {
        if !live.is_player() || live.pilot.char_id!=loaded.pilot.char_id || live.pilot.type_id!=loaded.pilot.type_id
            || live.identity_reserved || live.fight.as_ref().is_none_or(|f|!f.vitals.hull_damage.is_finite()
                || !f.vitals.hull_max.is_finite() || f.vitals.hull_max<=0.0 || f.vitals.hull_damage>=f.vitals.hull_max-1e-6) {
            return Err("Your live ship is changing state. Try again shortly.");
        }
        // A portal's active cycle keeps its source in combat after logout.
        // Reclaim that exact hull; loading cannot rewind its damage or motion.
        live.client_id=loaded.client_id;
        world.retained_logouts.remove(&loaded.id);
    } else {
        space::add_ship(world,system,loaded);
    }
    Ok(())
}

async fn reconnect_ship(
    ctx: &crate::services::Ctx<'_>,
    char_id: i64,
    ship_id: i64,
    system: i64,
) -> std::result::Result<Ship, String> {
    let saved = ctx
        .store
        .read(|db| {
            db.table("entity").get_id(ship_id)
                .filter(|e| e.int("ownerID") == Some(char_id) && e.int("locationID") == Some(system))
                .map(|e| [e.float("x"), e.float("y"), e.float("z")].map(|v| v.unwrap_or(f64::NAN)))
        })
        .ok_or_else(|| "Your active ship is no longer in this solar system.".to_string())?;
    let mut ship = inflight::ship_for_space(ctx, char_id, ship_id)
        .await
        .ok_or_else(|| "Your ship could not be loaded.".to_string())?;
    ship.pos = Vec3::new(saved[0], saved[1], saved[2]);
    if !ship.pos.x.is_finite() || !ship.pos.y.is_finite() || !ship.pos.z.is_finite() {
        return Err("Your ship's saved position is invalid.".into());
    }
    crate::space_state::restore(ctx,system,&mut ship).await.map_err(travel_failed)?;
    Ok(ship)
}

#[cfg(test)]
async fn persist_relocation(
    store: &Store,
    char_id: i64,
    from_system: i64,
    from_station: Option<i64>,
    system: i64,
    station: Option<i64>,
    constellation: i64,
    region: i64,
    ship: &Ship,
) -> std::result::Result<(), String> {
    let destination = station.unwrap_or(system);
    let source = from_station.unwrap_or(from_system);
    let flag = if station.is_some() { 4 } else { 0 };
    store.write(|tx| -> std::result::Result<(), transition::Failure> {
        let pilot=tx.table("character_").get_id(char_id).filter(|c|c.int("shipID")==Some(ship.id)
            && c.int("solarSystemID")==Some(from_system) && c.int("stationID").unwrap_or(0)==from_station.unwrap_or(0)).map(|c|c.id);
        let moved=tx.table("entity").get_id(ship.id)
            .filter(|e|e.int("ownerID")==Some(char_id) && e.int("locationID")==Some(source)).map(|e|e.id);
        let (Some(pilot),Some(moved))=(pilot,moved) else {
            return Err("Your active ship has moved and cannot be relocated.".into());
        };
        tx.update("character_",pilot,&[("stationID",StoreValue::Int(station.unwrap_or(0))),("solarSystemID",StoreValue::Int(system)),
            ("constellationID",StoreValue::Int(constellation)),("regionID",StoreValue::Int(region))]).map_err(travel_failed)?;
        tx.update("entity",moved,&[("locationID",StoreValue::Int(destination)),("flag",StoreValue::Int(flag)),
            ("x",StoreValue::Float(ship.pos.x)),("y",StoreValue::Float(ship.pos.y)),("z",StoreValue::Float(ship.pos.z))]).map_err(travel_failed)?;
        update_by_id(tx,"entity",char_id,&[("locationID",StoreValue::Int(destination))]).map_err(travel_failed)?;
        if let Some(fight) = &ship.fight {
            inflight::write_vitals(tx, ship.id, fight, station.is_some()).map_err(travel_failed)?;
        }
        Ok(())
    }).map_err(|failure|failure.to_string())
}

/// Test-only lower-level persistence harness. Production drive/portal travel
/// uses departure::reserved and keeps the live hull in combat until commit.
/// The ship is detached from the ballpark while this runs, so its capacitor
/// cannot change beneath the transaction. On any refusal restore the exact
/// original curve; fuel and all persisted arrival state roll back together.
#[cfg(test)]
async fn complete_paid_jump(
    store: &Store,
    char_id: i64,
    ship: &mut Ship,
    arrival: &JumpArrival,
    jump: &PreparedJump,
    departure:Option<&Departure<'_>>,
) -> std::result::Result<Vec<Value>, String> {
    let now = crate::combat::now_ms();
    let before = ship.fight.as_ref().map(|f| f.vitals.capacitor);
    if jump.capacitor > 0.0 {
        let paid = ship
            .fight
            .as_mut()
            .is_some_and(|f| f.vitals.capacitor.spend(now, jump.capacitor));
        if !paid {
            return Err("Your ship no longer has enough capacitor for the jump.".into());
        }
    }
    let capacitor = ship.fight.as_ref().map(|f| f.vitals.capacitor.at(now));
    match persist_jump_state_guarded(
        store,
        char_id,
        ship.id,
        arrival,
        Some(&jump.fuel),
        capacitor,
        ship.fight.as_ref(),
        departure,
    )
    .await
    {
        Ok(events) => Ok(events),
        Err(reason) => {
            if let (Some(before), Some(fight)) = (before, ship.fight.as_mut()) {
                fight.vitals.capacitor = before;
            }
            Err(reason)
        }
    }
}

/// const.maxStargateJumpingDistance.
const JUMP_RANGE: f64 = 2_500.0;
/// The client runs its own simulation; allow for the two drifting apart.
const RANGE_SLACK: f64 = 1.25;
/// How long the gate's flash and the ship's jump-out play before the ship
/// is gone. Their length is in the effects' models; this is a choice.
const JUMP_OUT_TIME: Duration = Duration::from_secs(3);
/// Close enough to a station's surface to be towed in.
const DOCK_RANGE: f64 = 2_500.0;
/// The longest a docking approach may take before the request is refused.
const DOCK_APPROACH_LIMIT: i64 = 600;

pub(super) struct DockingApproach {
    request: MachoPacket,
    system: i64,
    ship: i64,
    station: i64,
    deadline: std::time::Instant,
}

impl Conn {
    pub(super) async fn select_character(&mut self, req: &MachoPacket, char_id: i64) -> Result {
        if let Some(current) = self.session.char_id {
            return if current == char_id {
                self.reply(req, &Value::None).await
            } else {
                self.send_error(
                    req,
                    custom_error("A character is already selected on this connection."),
                )
                .await
            };
        }
        if char_id <= 0 || char_id > i32::MAX as i64 {
            return self
                .send_error(req, custom_error("That character is not on this account."))
                .await;
        }
        if !self.server.store.read(|db| db.table("character_").get_id(char_id).is_some()) {
            return self
                .send_error(req, custom_error("That character is not on this account."))
                .await;
        }
        let already_online = self
            .server
            .world
            .lock()
            .clients_of(char_id)
            .iter()
            .any(|id| *id != self.client_id);
        if already_online {
            return self.send_error(req,custom_error("That character is already connected. Wait for the previous connection to close.")).await;
        }
        let before = self.session.clone();

        let user_id = self.session.user_id.unwrap_or(0);
        let row = self.server.store.read(|db| {
            db.table("character_").get_id(char_id).filter(|c| c.int("accountID") == Some(user_id)).map(|c| {
                let opt = |column: &str| c.int(column);
                (opt("stationID"), opt("solarSystemID"), opt("constellationID"), opt("regionID"), opt("shipID"),
                    c.text("characterName").map(String::from), opt("typeID"))
            })
        });
        let Some((station_id, solar_system_id, constellation_id, region_id, ship, char_name, char_type_id)) = row else {
            warn!("character {char_id} is not on account {user_id}");
            let err = custom_error("That character is not on this account.");
            return self.send_error(req, err).await;
        };

        if crate::losses::journal::blocked_character_in_memory(&self.server.world,char_id)
            || crate::travel::blocked_character_in_memory(&self.server.world,char_id)
            || crate::svc::jumps::portal::blocked_character_in_memory(&self.server.world,char_id)
            || crate::conn::logout::blocked_character_in_memory(&self.server.world,char_id)
            || self.server.store.read(|db|crate::losses::pending::blocked_character(db,char_id))? {
            return self.send_error(req,custom_error("That character's station arrival is awaiting its final combat state. Try again shortly.")).await;
        }

        self.session.char_id = Some(char_id);
        self.session.char_name = char_name.unwrap_or_default();
        self.session.char_type_id = char_type_id.unwrap_or(0);
        self.session.station_id = station_id.filter(|id| *id > 0);
        self.session.solar_system_id = solar_system_id;
        self.session.constellation_id = constellation_id;
        self.session.region_id = region_id;
        self.session.solar_system_id2 = solar_system_id;
        if self.session.station_id.is_some() {
            self.session.solar_system_id = None;
        }
        self.session.ship_id = match self.session.station_id {
            Some(station) => self.resolve_active_ship(char_id, station).await,
            None => ship,
        };
        self.load_affiliation(char_id).await;
        self.span.record("char", char_id);

        if let Some(system) = self.session.solar_system_id {
            // Loading a cold system can take time. Recover the paid native
            // warp only after that work, immediately before publication.
            space::ensure_park(&self.ctx(), system).await;
            let loaded = match self.session.ship_id {
                Some(ship) => reconnect_ship(&self.ctx(), char_id, ship, system).await,
                None => Err("You have no active ship in space.".into()),
            };
            let ship = match loaded {
                Ok(ship) => ship,
                Err(reason) => {
                    self.session = before;
                    return self.send_error(req, custom_error(&reason)).await;
                }
            };
            let published=publish_reconnect(&mut self.server.world.lock(),system,ship);
            if let Err(reason)=published {
                self.session=before;
                return self.send_error(req,custom_error(reason)).await;
            }
        }

        self.send_session_change_from(&before).await?;
        info!(
            corp = ?self.session.corp_id,
            station = ?self.session.station_id,
            system = ?self.session.solar_system_id2,
            ship = ?self.session.ship_id,
            "entered the game as {:?} ({char_id})",
            self.session.char_name
        );
        self.reply(req, &Value::None).await
    }

    pub(super) async fn undock(
        &mut self,
        req: &MachoPacket,
        bind_params: Option<Vec<i64>>,
    ) -> Result {
        let (Some(station_id), Some(system_id)) =
            (self.session.station_id, self.session.solar_system_id2)
        else {
            warn!("undock while not docked");
            return self
                .send_error(req, custom_error("You are not docked."))
                .await;
        };
        let Some(ship_id) = self.session.ship_id else {
            warn!("undock with no active ship");
            let err = custom_error("You have no active ship to undock in.");
            return self.send_error(req, err).await;
        };

        let char_id = self.session.char_id.unwrap_or(0);
        let (loaded, exit) = {
            let ctx = self.ctx();
            space::ensure_park(&ctx, system_id).await;
            (
                inflight::ship_for_space(&ctx, char_id, ship_id).await,
                space::undock_point(&ctx, station_id).await,
            )
        };
        let (Some(mut ship), Some((pos, heading))) = (loaded, exit) else {
            error!("cannot undock ship {ship_id} from {station_id}: ship or station data missing");
            return self
                .send_error(req, custom_error("Your ship could not be undocked."))
                .await;
        };
        // Out of the bay at full speed, coasting to a stop clear of the station.
        ship.arrive(pos);
        ship.vel = heading * ship.stats.max_velocity;

        if let Some(char_id) = self.session.char_id {
            crate::svc::pvptrade::left_station(&self.server.store, &self.server.world, char_id)
                .await;
        }
        if let Err(reason) =
            persist_undock(&self.server.store, char_id, station_id, system_id, &ship).await
        {
            return self.send_error(req, custom_error(&reason)).await;
        }
        let before = self.session.clone();
        self.session.station_id = None;
        self.session.solar_system_id = Some(system_id);
        self.session.solar_system_id2 = Some(system_id);
        space::add_ship(&mut self.server.world.lock(), system_id, ship);
        self.send_session_change_from(&before).await?;
        info!("undocked from station {station_id} into system {system_id} in ship {ship_id}");

        let retval = match bind_params {
            Some(params) => bound::bind_result(self.server.node_id, "ship", &params, Value::None),
            None => Value::None,
        };
        self.reply(req, &retval).await
    }

    /// ship.Board(shipID) while docked: make another of the character's
    /// assembled ships in this station the active one.
    pub(super) async fn board(
        &mut self,
        req: &MachoPacket,
        ship_id: i64,
        bind_params: Option<Vec<i64>>,
    ) -> Result {
        if self.session.station_id.is_none() {
            return self.change_space_ship(req, Some(ship_id), bind_params).await;
        }
        if let Err(error)=crate::svc::embark::station_switch(&self.ctx(),Some(ship_id)).await {
            return self.send_error(req,custom_error(&crate::svc::embark::error_text(error))).await;
        }
        let before = self.session.clone();
        self.session.ship_id = Some(ship_id);
        self.send_session_change_from(&before).await?;
        info!("boarded ship {ship_id}");
        let retval = match bind_params {
            Some(params) => bound::bind_result(self.server.node_id, "ship", &params, Value::None),
            None => Value::None,
        };
        self.reply(req, &retval).await
    }

    pub(super) async fn change_space_ship(&mut self, req: &MachoPacket, target: Option<i64>, bind_params: Option<Vec<i64>>) -> Result {
        self.cancel_docking("Changing ships cancelled the docking approach.").await?;
        let changed = if self.session.station_id.is_some() {
            crate::svc::embark::station_switch(&self.ctx(), target).await
        } else { crate::svc::embark::switch(&self.ctx(), target).await };
        let next = match changed {
            Ok(next) => next,
            Err(error) => return self.send_error(req, custom_error(&crate::svc::embark::error_text(error))).await,
        };
        self.finish_ship_change(req,next,bind_params).await
    }

    pub(super) async fn maintenance_ship(&mut self, req: &MachoPacket, host:i64, target:Option<i64>, bind_params:Option<Vec<i64>>) -> Result {
        self.cancel_docking("Changing ships cancelled the docking approach.").await?;
        let changed=match target {
            Some(hull)=>crate::svc::maintenance::board_stored(&self.ctx(),host,hull).await,
            None=>crate::svc::maintenance::store(&self.ctx(),host).await,
        };
        match changed {
            Ok(next)=>self.finish_ship_change(req,next,bind_params).await,
            Err(error)=>self.send_error(req,custom_error(&crate::svc::embark::error_text(error))).await,
        }
    }

    async fn finish_ship_change(&mut self,req:&MachoPacket,next:i64,bind_params:Option<Vec<i64>>) -> Result {
        let before = self.session.clone();
        self.session.ship_id = Some(next);
        self.send_session_change_from(&before).await?;
        // A ship-ID-only session change does not request GetFormations in
        // Trinity. Explicitly give this client its new ego and visible balls.
        if self.session.station_id.is_none() { space::push_set_state(&self.ctx()).await; }
        let value = bind_params.map_or(Value::None, |params|bound::bind_result(self.server.node_id,"ship",&params,Value::None));
        self.reply(req,&value).await
    }

    pub(super) async fn dock(&mut self, req: &MachoPacket, station_id: i64, towed: bool) -> Result {
        self.cancel_docking("A new docking request replaced the previous approach.")
            .await?;
        let denied =
            |reason: &str| user_error("DockingRequestDenied", &[("reason", Value::bytes(reason))]);
        if towed && self.session.role & (8 | 16 | 262_144) == 0 {
            return self
                .send_error(req, denied("Only a game master may request a remote tow."))
                .await;
        }
        let (Some(system_id), Some(ship_id)) = (self.session.solar_system_id, self.session.ship_id)
        else {
            warn!("dock while not in space");
            return self.send_error(req, denied("You are not in space.")).await;
        };
        // A player-owned station may turn some pilots away.
        const SERVICE_DOCKING: i64 = 2_097_152;
        if let Some(char_id) = self.session.char_id {
            if !self.server.store.read(|db| crate::svc::outposts::may_use(db, station_id, SERVICE_DOCKING, char_id)) {
                return self
                    .send_error(
                        req,
                        denied("The station's owners do not allow you to dock here."),
                    )
                    .await;
            }
        }

        // Where is the ship relative to the station? (Decided under the lock,
        // answered after it is released.)
        let gap = {
            let mut world = self.server.world.lock();
            match world.parks.get_mut(&system_id) {
                None => Err("You are not in space."),
                Some(park) => {
                    park.advance(destiny::tick_now());
                    if park.ships.get(&ship_id).is_some_and(|ship| ship.warping() || ship.identity_reserved) {
                        Err("You cannot dock while warping.")
                    } else if !space::is_station(park, station_id) {
                        warn!("dock at {station_id}, which is not a station in {system_id}");
                        Err("That station is not in this solar system.")
                    } else {
                        park.surface_distance(ship_id, station_id)
                            .ok_or("Your ship is not in space.")
                    }
                }
            }
        };
        let gap = match gap {
            Ok(gap) => gap,
            Err(reason) => return self.send_error(req, denied(reason)).await,
        };

        // Too far: set course for the station and wait for the ship to get there.
        if gap > DOCK_RANGE && !towed {
            let eta = {
                let mut world = self.server.world.lock();
                let park = world.parks.get_mut(&system_id).expect("checked above");
                park.set_speed_fraction(ship_id, 1.0);
                park.command(
                    ship_id,
                    Command::Follow {
                        target: station_id,
                        range: 0.0,
                    },
                );
                let eta = park.eta_within(ship_id, station_id, DOCK_RANGE, DOCK_APPROACH_LIMIT);
                let course = space::entry(
                    park.tick,
                    "FollowBall",
                    vec![
                        Value::Int(ship_id),
                        Value::Int(station_id),
                        Value::Double(0.0),
                    ],
                );
                let throttle = space::entry(
                    park.tick,
                    "SetSpeedFraction",
                    vec![Value::Int(ship_id), Value::Double(1.0)],
                );
                space::broadcast(&world, system_id, None, vec![throttle, course]);
                let message = Value::Tuple(vec![
                    Value::bytes("DockingApproach"),
                    Value::Dict(Vec::new()),
                ]);
                world.notify(&[self.client_id], "OnRemoteMessage", message);
                eta
            };
            let Some(eta) = eta else {
                space::order(
                    &mut self.server.world.lock(),
                    system_id,
                    ship_id,
                    Command::Stop,
                );
                warn!("dock at {station_id}: {gap:.0} m away, too far to approach");
                return self
                    .send_error(req, denied("The station is too far away."))
                    .await;
            };
            info!("approaching station {station_id} from {gap:.0} m, docking in ~{eta}s");
            self.pending_dock = Some(DockingApproach {
                request: req.clone(),
                system: system_id,
                ship: ship_id,
                station: station_id,
                deadline: std::time::Instant::now()
                    + Duration::from_secs(DOCK_APPROACH_LIMIT as u64),
            });
            return Ok(());
        }

        let arrived=match self.run_transition(transition::Request::Dock {station:station_id,towed}).await {
            Ok(arrived)=>arrived,
            Err(reason @ transition::Failure::Pending(_))=>return self.close(reason.to_string()).await,
            Err(reason)=>return self.send_error(req,denied(&reason.to_string())).await,
        };
        let point = |v: Vec3| {
            Value::Tuple(vec![
                Value::Double(v.x),
                Value::Double(v.y),
                Value::Double(v.z),
            ])
        };
        if let Some((start,end,station))=arrived.docking {
            let accepted=Value::Tuple(vec![point(start),point(end),Value::Int(station)]);
            self.server.world.lock().notify(&[self.client_id],"OnDockingAccepted",accepted);
        }

        let before = self.session.clone();
        self.session.station_id = arrived.place.station;
        self.session.solar_system_id = None;
        self.session.solar_system_id2 = Some(arrived.place.system);
        self.session.constellation_id = Some(arrived.place.constellation);
        self.session.region_id = Some(arrived.place.region);
        self.session.ship_id = Some(arrived.ship_id);
        self.send_session_change_from(&before).await?;

        info!(ship = ?self.session.ship_id, "docked at station {station_id}");
        self.reply(req, &Value::None).await
    }

    pub(super) async fn cancel_docking(&mut self, reason: &str) -> Result {
        let Some(pending) = self.pending_dock.take() else {
            return Ok(());
        };
        {
            let mut world = self.server.world.lock();
            let following = world
                .parks
                .get(&pending.system)
                .and_then(|p| p.ships.get(&pending.ship))
                .is_some_and(
                    |s| matches!(s.command,Command::Follow {target,..} if target==pending.station),
                );
            if following {
                space::order(&mut world, pending.system, pending.ship, Command::Stop);
            }
        }
        self.send_error(
            &pending.request,
            user_error("DockingRequestDenied", &[("reason", Value::bytes(reason))]),
        )
        .await
    }

    pub(super) async fn poll_docking(&mut self) -> Result {
        let Some(pending) = self.pending_dock.as_ref() else {
            return Ok(());
        };
        if std::time::Instant::now() >= pending.deadline {
            return self.cancel_docking("The docking approach timed out.").await;
        }
        let current = self.session.solar_system_id == Some(pending.system)
            && self.session.ship_id == Some(pending.ship);
        let range = if current {
            let mut world = self.server.world.lock();
            world.parks.get_mut(&pending.system).and_then(|park| {
                park.advance(destiny::tick_now());
                let ship=park.ships.get(&pending.ship)?;
                if ship.client_id!=self.client_id || ship.warping() || ship.identity_reserved || !matches!(ship.command,Command::Follow {target,..} if target==pending.station) {return None;}
                park.surface_distance(pending.ship,pending.station)
            })
        } else {
            None
        };
        match range {
            None => {
                self.cancel_docking("The docking approach was cancelled.")
                    .await
            }
            Some(gap) if gap <= DOCK_RANGE => {
                let pending = self.pending_dock.take().expect("checked above");
                self.dock(&pending.request, pending.station, false).await
            }
            _ => Ok(()),
        }
    }

    pub(super) async fn stargate_jump(
        &mut self,
        req: &MachoPacket,
        from_gate: i64,
        to_gate: i64,
    ) -> Result {
        let (Some(system_id), Some(ship_id)) = (self.session.solar_system_id, self.session.ship_id)
        else {
            warn!("stargate jump while not in space in a ship");
            return self
                .send_error(req, custom_error("You are not in space."))
                .await;
        };

        let gap = {
            let mut world = self.server.world.lock();
            world.parks.get_mut(&system_id).and_then(|park| {
                park.advance(destiny::tick_now());
                if park.ships.get(&ship_id).is_none_or(|ship| {
                    ship.warping() || ship.cloaked || ship.identity_reserved || ship.client_id != self.client_id
                }) {
                    return None;
                }
                park.surface_distance(ship_id, from_gate)
            })
        };
        match gap {
            Some(gap) if gap <= JUMP_RANGE * RANGE_SLACK => {}
            Some(gap) => {
                info!("jump refused: {gap:.0} m from gate {from_gate}");
                let maxdist = [("maxdist", Value::Int(JUMP_RANGE as i64))];
                return self
                    .send_error(req, user_error("NotCloseEnoughToJump", &maxdist))
                    .await;
            }
            None => {
                warn!("jump: ship {ship_id} or gate {from_gate} is not in the ballpark of {system_id}");
                return self
                    .send_error(req, custom_error("You are not in space."))
                    .await;
            }
        }

        let jump = {
            let ctx = self.ctx();
            match space::resolve_jump(&ctx, system_id, from_gate, to_gate).await {
                Some(jump) => {
                    space::ensure_park(&ctx, jump.solar_system_id).await;
                    Some(jump)
                }
                None => None,
            }
        };
        let Some(jump) = jump else {
            warn!("gate {from_gate} -> {to_gate} is not a jump from system {system_id}");
            let err = custom_error("That stargate does not lead there.");
            return self.send_error(req, err).await;
        };

        // Leave: the gate lights up and the ship jumps out, for everyone here
        // and for the pilot ("Jumping", spaceMgr.py:123); it goes once that
        // has played.
        {
            let world = self.server.world.lock();
            let tick = world
                .parks
                .get(&system_id)
                .map_or(destiny::tick_now(), |p| p.tick);
            let gate = space::special_fx(tick, from_gate, None, "effects.GateActivity");
            let out = space::special_fx(tick, ship_id, Some(from_gate), "effects.JumpOut");
            space::broadcast(&world, system_id, None, vec![gate, out]);
        }
        tokio::time::sleep(JUMP_OUT_TIME).await;
        let arrived=match self.run_transition(transition::Request::Gate {
            from_gate,to_gate,to:transition::Place {station:None,system:jump.solar_system_id,
                constellation:jump.constellation_id,region:jump.region_id,at:jump.arrival},
        }).await {
            Ok(arrived)=>arrived,
            Err(reason @ transition::Failure::Pending(_))=>return self.close(reason.to_string()).await,
            Err(reason)=>return self.send_error(req,custom_error(&reason.to_string())).await,
        };
        let before = self.session.clone();
        self.session.station_id = arrived.place.station;
        self.session.solar_system_id = arrived.place.station.is_none().then_some(arrived.place.system);
        self.session.solar_system_id2 = Some(arrived.place.system);
        self.session.constellation_id = Some(arrived.place.constellation);
        self.session.region_id = Some(arrived.place.region);
        self.session.ship_id = Some(arrived.ship_id);
        self.send_session_change_from(&before).await?;

        info!(
            "ship {ship_id} jumped from gate {from_gate} in {system_id} to gate {to_gate} in {}",
            jump.solar_system_id
        );
        if let Err(error) = crate::svc::map::record_activity(&self.server.store, system_id, 1).await {
            warn!("recording gate jump: {}", crate::logging::chain(&error));
        }
        self.reply(req, &Value::None).await
    }

    /// beyonce.BeaconJump(charID, beaconID, solarsystemID): a jump drive to a
    /// gang member's cynosural field.
    pub(super) async fn beacon_jump(
        &mut self,
        req: &MachoPacket,
        member: i64,
        beacon: i64,
        to: i64,
    ) -> Result {
        let (Some(char_id), Some(from), Some(ship_id)) = (
            self.session.char_id,
            self.session.solar_system_id,
            self.session.ship_id,
        ) else {
            return self
                .send_error(req, custom_error("You are not in space."))
                .await;
        };
        let landing = crate::svc::jumps::prepare_beacon_jump(
            &self.ctx(),
            char_id,
            ship_id,
            from,
            member,
            beacon,
            to,
        )
        .await;
        match landing {
            Ok(_at) => {
                self.drive_jump(
                    req,
                    from,
                    to,
                    JumpRequest::Beacon {member,beacon},
                    "effects.JumpDriveOut",
                    "effects.JumpDriveIn",
                )
                .await
            }
            Err(why) => {
                info!("jump drive refused: {why}");
                self.send_error(req, custom_error(&why)).await
            }
        }
    }

    /// beyonce.BeaconJumpAlliance(beaconID, solarsystemID).
    pub(super) async fn beacon_jump_alliance(
        &mut self,
        req: &MachoPacket,
        beacon: i64,
        to: i64,
    ) -> Result {
        let (Some(char_id), Some(from), Some(ship_id)) = (
            self.session.char_id,
            self.session.solar_system_id,
            self.session.ship_id,
        ) else {
            return self
                .send_error(req, custom_error("You are not in space."))
                .await;
        };
        match crate::svc::jumps::prepare_alliance_jump(
            &self.ctx(),
            char_id,
            ship_id,
            from,
            beacon,
            to,
        )
        .await
        {
            Ok(_at) => {
                self.drive_jump(
                    req,
                    from,
                    to,
                    JumpRequest::Alliance {beacon},
                    "effects.JumpDriveOut",
                    "effects.JumpDriveIn",
                )
                .await
            }
            Err(reason) => self.send_error(req, custom_error(&reason)).await,
        }
    }

    /// beyonce.JumpThrough(charID, shipID, beaconID, solarsystemID).
    pub(super) async fn jump_through(
        &mut self,
        req: &MachoPacket,
        titan_pilot: i64,
        titan: i64,
    ) -> Result {
        let (Some(char_id), Some(from), Some(ship_id)) = (
            self.session.char_id,
            self.session.solar_system_id,
            self.session.ship_id,
        ) else {
            return self
                .send_error(req, custom_error("You are not in space."))
                .await;
        };
        let landing = crate::svc::jumps::prepare_jump_through(
            &self.ctx(),
            char_id,
            ship_id,
            from,
            titan_pilot,
            titan,
            false,
        )
        .await;
        match landing {
            Ok((to, _at)) => {
                self.drive_jump(req, from, to, JumpRequest::Portal {pilot:titan_pilot,ship:titan,alliance:false}, "effects.JumpOut", "effects.JumpIn")
                    .await
            }
            Err(why) => {
                info!("bridge jump refused: {why}");
                self.send_error(req, custom_error(&why)).await
            }
        }
    }

    /// beyonce.JumpThroughAlliance(shipID, beaconID, solarsystemID).
    pub(super) async fn jump_through_alliance(&mut self, req: &MachoPacket, titan: i64) -> Result {
        let (Some(char_id), Some(from), Some(ship_id)) = (
            self.session.char_id,
            self.session.solar_system_id,
            self.session.ship_id,
        ) else {
            return self
                .send_error(req, custom_error("You are not in space."))
                .await;
        };
        let pilot = self
            .server
            .world
            .lock()
            .bridges
            .get(&titan)
            .map(|b| b.titan_pilot)
            .unwrap_or(0);
        match crate::svc::jumps::prepare_jump_through(
            &self.ctx(),
            char_id,
            ship_id,
            from,
            pilot,
            titan,
            true,
        )
        .await
        {
            Ok((to, _at)) => {
                self.drive_jump(req, from, to, JumpRequest::Portal {pilot,ship:titan,alliance:true}, "effects.JumpOut", "effects.JumpIn")
                    .await
            }
            Err(reason) => self.send_error(req, custom_error(&reason)).await,
        }
    }

    /// Move the ship and session through a drive or bridge, outside stargates.
    async fn drive_jump(
        &mut self,
        req: &MachoPacket,
        from: i64,
        to: i64,
        request: JumpRequest,
        out_fx: &str,
        in_fx: &str,
    ) -> Result {
        let Some(ship_id) = self.session.ship_id else {
            return self.reply(req, &Value::None).await;
        };
        let place = self.server.store.read(|db| {
            db.table("mapSolarSystems")
                .get_id(to)
                .map(|s| (s.int("constellationID").unwrap_or(0), s.int("regionID").unwrap_or(0)))
        });
        let Some((constellation, region)) = place else {
            return self
                .send_error(req, custom_error("That system does not exist."))
                .await;
        };
        space::ensure_park(&self.ctx(), to).await;
        {
            let world = self.server.world.lock();
            let tick = world
                .parks
                .get(&from)
                .map_or(destiny::tick_now(), |p| p.tick);
            space::broadcast(
                &world,
                from,
                None,
                vec![space::special_fx(tick, ship_id, None, out_fx)],
            );
        }
        tokio::time::sleep(JUMP_OUT_TIME).await;
        // The animation and destination loading yield to module, gang and
        // structure changes. Requote all permissions and costs at departure.
        let jump=match request.prepare(&self.ctx(),to).await {
            Ok(jump)=>jump,
            Err(reason)=>return self.send_error(req,custom_error(&reason)).await,
        };
        let arrival = JumpArrival {from,system:to,constellation,region,at:jump.at};
        let server=self.server.clone();
        let session=self.session.clone();
        let client=self.client_id;
        let arrival_fx=in_fx.to_string();
        // Own the payment/publication future: cancellation after the payment
        // commits must still publish its accepted arrival exactly once.
        let result=tokio::spawn(async move {
            departure::reserved::run(&server,&session,client,request,&arrival,&jump,&arrival_fx).await
        }).await;
        let arrived=match result {
            Ok(Ok(arrived))=>arrived,
            Ok(Err(reason @ transition::Failure::Pending(_)))=>return self.close(reason.to_string()).await,
            Ok(Err(reason))=>return self.send_error(req,custom_error(&reason.to_string())).await,
            Err(error)=>{
                error!("paid departure task failed: {error}");
                return self.close("The jump outcome is pending reconciliation.").await;
            }
        };
        let before = self.session.clone();
        self.session.station_id = arrived.place.station;
        self.session.solar_system_id = arrived.place.station.is_none().then_some(arrived.place.system);
        self.session.solar_system_id2 = Some(arrived.place.system);
        self.session.constellation_id = Some(arrived.place.constellation);
        self.session.region_id = Some(arrived.place.region);
        self.session.ship_id = Some(arrived.ship_id);
        self.send_session_change_from(&before).await?;
        if let Err(error) = crate::svc::map::record_activity(&self.server.store, from, 1).await {
            warn!("recording drive jump: {}", crate::logging::chain(&error));
        }
        info!("ship {ship_id} jumped from {from} to {to} without a gate");
        self.reply(req, &Value::None).await
    }

    pub(super) async fn jump_through_structure(
        &mut self,
        req: &MachoPacket,
        local: i64,
        remote: i64,
        to: i64,
    ) -> Result {
        let Some(from) = self.session.solar_system_id else {
            return self
                .send_error(req, custom_error("You are not in space."))
                .await;
        };
        match crate::svc::bridges::prepare(&self.ctx(), local, remote, to).await {
            Ok(_at) => {
                self.drive_jump(req, from, to, JumpRequest::Structure {local,remote}, "effects.JumpOut", "effects.JumpIn")
                    .await
            }
            Err(reason) => self.send_error(req, custom_error(&reason)).await,
        }
    }

    /// A session move the world decided on: the ship blew up, or the capsule did.
    pub(super) async fn forced_move(&mut self, m: Move) -> Result {
        if matches!(
            m,
            Move::ShipLost { .. }
                | Move::Podded { .. }
                | Move::Relocate { .. }
                | Move::Teleport { .. }
        ) {
            self.cancel_docking("Your ship moved before the docking approach completed.")
                .await?;
        }
        let before = self.session.clone();
        match m {
            Move::ShipLost { ship_id } => {
                self.session.ship_id = Some(ship_id);
                self.send_session_change_from(&before).await?;
                info!("ship destroyed; ejected in capsule {ship_id}");
            }
            Move::Podded {
                station_id,
                solar_system_id,
                constellation_id,
                region_id,
                ship_id,
            } => {
                self.session.station_id = Some(station_id);
                self.session.solar_system_id = None;
                self.session.solar_system_id2 = Some(solar_system_id);
                self.session.constellation_id = Some(constellation_id);
                self.session.region_id = Some(region_id);
                self.session.ship_id = ship_id;
                self.send_session_change_from(&before).await?;
                info!(ship = ?ship_id, "capsule destroyed; woke up in a clone at station {station_id}");
            }
            Move::Fleet {
                gang_id,
                role,
                wing_id,
                squad_id,
            } => {
                self.session.gang_id = gang_id;
                self.session.gang_role = gang_id.map(|_| role);
                self.session.wing_id = wing_id;
                self.session.squad_id = squad_id;
                if self.session.change_from(&before) != Value::Dict(Vec::new()) {
                    self.send_session_change_from(&before).await?;
                }
            }
            Move::Gang { gang_id, leader } => {
                self.session.gang_id = gang_id;
                self.session.wing_id = None;
                self.session.squad_id = None;
                self.session.gang_role = gang_id.map(|_| {
                    if leader {
                        crate::svc::gang::GANG_LEADER_ROLE
                    } else {
                        0
                    }
                });
                if self.session.change_from(&before) != Value::Dict(Vec::new()) {
                    self.send_session_change_from(&before).await?;
                    info!(gang = ?gang_id, leader, "gang changed");
                }
            }
            Move::Relocate {
                station_id,
                solar_system_id,
                constellation_id,
                region_id,
                ship_id,
            } => {
                self.session.station_id = station_id;
                self.session.solar_system_id = if station_id.is_some() {
                    None
                } else {
                    Some(solar_system_id)
                };
                self.session.solar_system_id2 = Some(solar_system_id);
                self.session.constellation_id = Some(constellation_id);
                self.session.region_id = Some(region_id);
                self.session.ship_id = Some(ship_id);
                self.send_session_change_from(&before).await?;
                info!(station = ?station_id, system = solar_system_id, ship = ship_id, "relocated");
            }
            Move::Teleport {
                station_id,
                solar_system_id,
                constellation_id,
                region_id,
                pos,
            } => {
                self.teleport(
                    station_id,
                    solar_system_id,
                    constellation_id,
                    region_id,
                    Vec3::new(pos.0, pos.1, pos.2),
                )
                .await?;
            }
            Move::Affiliation => {
                let Some(char_id) = self.session.char_id else {
                    return Ok(());
                };
                self.load_affiliation(char_id).await;
                if self.session.change_from(&before) != Value::Dict(Vec::new()) {
                    self.send_session_change_from(&before).await?;
                    info!(corp = ?self.session.corp_id, alliance = ?self.session.alliance_id, "affiliation changed");
                }
            }
        }
        Ok(())
    }

    /// /tr: out of where the ship is, and into the station or the spot given.
    async fn teleport(
        &mut self,
        station_id: Option<i64>,
        system: i64,
        constellation: i64,
        region: i64,
        pos: Vec3,
    ) -> Result {
        if self.session.char_id.is_none() || self.session.ship_id.is_none() {return Ok(());}
        let from=self.session.solar_system_id;
        if station_id.is_none() {space::ensure_park(&self.ctx(),system).await;}
        let arrived=match self.run_transition(transition::Request::Admin {to:transition::Place {
            station:station_id,system,constellation,region,at:pos,
        }}).await {
            Ok(arrived)=>arrived,
            Err(reason @ transition::Failure::Pending(_))=>return self.close(reason.to_string()).await,
            Err(reason)=>{
                self.server.world.lock().notify(&[self.client_id],"OnRemoteMessage",Value::Tuple(vec![
                    Value::bytes("CustomError"),Value::Dict(vec![(Value::bytes("error"),Value::text(reason.to_string()))]),
                ]));
                return Ok(());
            }
        };
        let before=self.session.clone();
        self.session.station_id=arrived.place.station;
        self.session.solar_system_id=arrived.place.station.is_none().then_some(arrived.place.system);
        self.session.solar_system_id2=Some(arrived.place.system);
        self.session.constellation_id=Some(arrived.place.constellation);
        self.session.region_id=Some(arrived.place.region);
        self.session.ship_id=Some(arrived.ship_id);
        self.send_session_change_from(&before).await?;
        // No session-location change makes Trinity reload a same-system ego.
        if arrived.place.station.is_none() && from==Some(arrived.place.system) {space::push_set_state(&self.ctx()).await;}
        info!(station=?arrived.place.station,system=arrived.place.system,"relocation completed");
        Ok(())
    }

    async fn run_transition(&self,request:transition::Request)->std::result::Result<transition::Arrived,transition::Failure> {
        let server=self.server.clone();let session=self.session.clone();let client=self.client_id;
        tokio::spawn(async move {transition::run(&server,&session,client,request).await}).await.map_err(|error| {
            error!("location transition task failed: {error}");transition::Failure::Pending("The location transition could not be completed. Reconnect after its pending state is resolved.".into())
        })?
    }

    /// Reads the character's corporation, its alliance and HQ, and the
    /// character's roles in it into the session.
    async fn load_affiliation(&mut self, char_id: i64) {
        let row = self.server.store.read(|db| {
            let c = db.table("character_").get_id(char_id)?;
            let corp_id = c.int("corporationID");
            let corp = corp_id.and_then(|id| db.table("corporation").get_id(id));
            let roles = db.table("chrCorporationRoles").get_id(char_id);
            let role = |column: &str| roles.and_then(|r| r.int(column)).unwrap_or(0);
            Some((
                corp_id,
                corp.and_then(|co| co.int("allianceID")),
                corp.and_then(|co| co.int("stationID")),
                [role("rolesAtAll"), role("rolesAtHQ"), role("rolesAtBase"), role("rolesAtOther")],
                c.int("corpAccountKey"),
            ))
        });
        let Some((corp_id, alliance_id, hq_id, [at_all, at_hq, at_base, at_other], account_key)) = row else { return };
        let s = &mut self.session;
        s.corp_id = corp_id;
        s.alliance_id = alliance_id.filter(|v| *v != 0);
        s.hq_id = hq_id.filter(|v| *v != 0);
        s.base_id = s.hq_id;
        s.roles_at_all = at_all;
        s.roles_at_hq = at_hq;
        s.roles_at_base = at_base;
        s.roles_at_other = at_other;
        s.corp_account_key = account_key.and_then(|key|i32::try_from(key).ok()).filter(|key|crate::svc::wallet::can_take(s.corp_role(),*key)).or(Some(1000));
    }

    /// Pick the character's ship in `station_id` (preferring the recorded one) and
    /// store it as their active ship.
    async fn resolve_active_ship(&self, char_id: i64, station_id: i64) -> Option<i64> {
        let resolved = self.server.store.write(|tx| {
            let Some(pilot) = tx.table("character_").get_id(char_id) else { return Ok(None) };
            let (row, recorded) = (pilot.id, pilot.int("shipID"));
            let ships = crate::sde::get().types_in_category(6);
            let candidate = tx
                .table("entity")
                .find(&["ownerID", "locationID"], &[StoreValue::Int(char_id), StoreValue::Int(station_id)])
                .filter(|e| e.int("singleton") == Some(1) && e.int("typeID").is_some_and(|t| ships.contains(&(t as i32))))
                .filter_map(|e| e.int("itemID"))
                .min_by_key(|id| (Some(*id) != recorded, *id));
            tx.update("character_", row, &[("shipID", candidate.map_or(StoreValue::Null, StoreValue::Int))])?;
            Ok::<_, StoreError>(candidate)
        });
        match resolved {
            Ok(ship) => ship,
            Err(e) => {
                error!(
                    "resolving the active ship for {char_id}: {}",
                    crate::logging::chain(&e)
                );
                None
            }
        }
    }
}
