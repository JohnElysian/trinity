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

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn disconnect_saves_actual_capsule_with_unprocessed_clone_jump_session() {
        let fixture = crate::test_support::Database::load().await;
        let pilot = fixture.id().await;
        let old_hull = fixture.id().await;
        let capsule = fixture.id().await;
        let other_hull = fixture.id().await;
        let derelict = fixture.id().await;
        let place = fixture.db.query_one(r#"SELECT "stationID","solarSystemID" FROM "staStations" ORDER BY "stationID" LIMIT 1"#, &[]).await.unwrap();
        let station = place.get::<_, i32>(0);
        let origin = place.get::<_, i32>(1);
        let destination: i32 = fixture.db.query_one(r#"SELECT "solarSystemID" FROM "mapSolarSystems" WHERE "solarSystemID"<>$1 ORDER BY "solarSystemID" LIMIT 1"#, &[&origin]).await.unwrap().get(0);
        // The clone jump has committed and published its capsule, but the
        // connection still has the old docked session when its socket closes.
        fixture.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","shipID","stationID","solarSystemID") VALUES ($1,$2,$3,0,$4)"#, &[&pilot,&format!("Stale clone session {pilot}"),&capsule,&destination]).await.unwrap();
        fixture.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES ($1,587,$3,$4,4,1,1),($2,670,$3,$5,0,1,1)"#, &[&old_hull,&capsule,&pilot,&station,&destination]).await.unwrap();
        let current = crate::session::Session {
            char_id: Some(pilot as i64), ship_id: Some(capsule as i64),
            solar_system_id: Some(destination as i64), solar_system_id2: Some(destination as i64),
            ..Default::default()
        };
        let mut ship = inflight::ship_for_space(&fixture.ctx(&current), pilot as i64, capsule as i64).await.unwrap();
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
            table: Default::default(), db:crate::db::Shared::new(fixture.db,fixture.url.clone()), db_url: fixture.url, node_id: 1,
            bulk: fixture.bulk, dogma: fixture.dogma, services: crate::svc::registry(),
            world: fixture.world, dump_dir: Default::default(),
            alliance_join_delay_secs: 0, sov_claim_delay_secs: 0,
        });
        let mut conn = Conn::new(socket, peer, server, 9292);
        conn.session = crate::session::Session {
            char_id: Some(pilot as i64), ship_id: Some(old_hull as i64),
            station_id: Some(station as i64), solar_system_id2: Some(origin as i64),
            ..Default::default()
        };
        ship.client_id = conn.client_id;
        let other = Ship::new(other_hull as i64, conn.client_id + 1, ship.stats.clone(), ship.pilot.clone(), Vec3::ZERO, Vec3::ZERO);
        let mut empty = Ship::new(derelict as i64, conn.client_id, ship.stats.clone(), ship.pilot.clone(), Vec3::ZERO, Vec3::ZERO);
        empty.unpiloted = true;
        let _notifications = {
            let mut world = conn.server.world.lock();
            let notifications = world.connect(conn.client_id);
            world.set_presence(conn.client_id, conn.presence());
            let mut park = crate::ballpark::Park::new(destination as i64, vec![], destiny::tick_now());
            park.ships.insert(capsule as i64, ship);
            park.ships.insert(other_hull as i64, other);
            park.ships.insert(derelict as i64, empty);
            world.parks.insert(destination as i64, park);
            world.push_move(conn.client_id, Move::Relocate {
                station_id: None, solar_system_id: destination as i64,
                constellation_id: 0, region_id: 0, ship_id: capsule as i64,
            });
            notifications
        };
        drop(client);
        conn.leave_world().await;
        {
            let world = conn.server.world.lock();
            let ships = &world.parks[&(destination as i64)].ships;
            assert!(!ships.contains_key(&(capsule as i64)));
            assert!(ships.contains_key(&(other_hull as i64)), "another client's hull stays in space");
            assert!(ships.contains_key(&(derelict as i64)), "unpiloted hull stays even with a stale client ID");
            assert!(world.clients_of(pilot as i64).is_empty());
        }
        let saved = conn.db.query_one(r#"SELECT x,y,z FROM entity WHERE "itemID"=$1"#, &[&capsule]).await.unwrap();
        assert_eq!((saved.get::<_, f64>(0),saved.get::<_, f64>(1),saved.get::<_, f64>(2)), (12345.0,-67890.0,34567.0));
        let loaded = inflight::ship_for_space(&conn.ctx(), pilot as i64, capsule as i64).await.unwrap();
        let vitals = &loaded.fight.as_ref().unwrap().vitals;
        assert_eq!(vitals.armor_damage, 7.0);
        assert!(vitals.capacitor.at(crate::combat::now_ms()) < 18.0, "stale docked session must not refill the space capsule");
        assert_eq!(conn.session.station_id, Some(station as i64), "test never consumes the queued relocation");
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn pending_docking_keeps_commands_and_disconnect_responsive_and_commits_on_arrival() {
        let fixture = crate::test_support::Database::load().await;
        let pilot = fixture.id().await;
        let hull = fixture.id().await;
        let place=fixture.db.query_one(r#"SELECT "stationID","solarSystemID" FROM "staStations" ORDER BY "stationID" LIMIT 1"#,&[]).await.unwrap();
        let station = place.get::<_, i32>(0);
        let system = place.get::<_, i32>(1);
        fixture.db.execute(r#"INSERT INTO "character_" ("characterID","accountID","characterName","shipID","stationID","solarSystemID","corporationID") VALUES ($1,$1,$2,$3,0,$4,1000002)"#,&[&pilot,&format!("Approach fixture {pilot}"),&hull,&system]).await.unwrap();
        fixture.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",quantity,singleton,x) VALUES ($1,587,$2,$3,1,1,10000)"#,&[&hull,&pilot,&system]).await.unwrap();
        let session = crate::session::Session {
            char_id: Some(pilot as i64),
            ship_id: Some(hull as i64),
            solar_system_id: Some(system as i64),
            solar_system_id2: Some(system as i64),
            corp_id: Some(1000002),
            ..Default::default()
        };
        let mut ship = inflight::ship_for_space(&fixture.ctx(&session), pilot as i64, hull as i64)
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
            db:crate::db::Shared::new(fixture.db,fixture.url.clone()),
            db_url: fixture.url,
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
                id: station as i64,
                type_id: 1529,
                group_id: 15,
                name: "Docking fixture".into(),
                pos: Vec3::ZERO,
                radius: 1000.0,
                jump: None,
            };
            let mut park =
                crate::ballpark::Park::new(system as i64, vec![station_ball], destiny::tick_now());
            park.ships.insert(hull as i64, ship);
            world.parks.insert(system as i64, park);
            notifications
        };
        let request = MachoPacket::call_req(Value::None);
        tokio::time::timeout(
            Duration::from_secs(1),
            conn.dock(&request, station as i64, false),
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
            conn.server.world.lock().parks[&(system as i64)].ships[&(hull as i64)].command,
            Command::Stop
        ));

        conn.dock(&request, station as i64, false).await.unwrap();
        assert!(conn.pending_dock.is_some());
        tokio::time::timeout(Duration::from_secs(1), conn.leave_world())
            .await
            .expect("disconnect must not wait for docking");
        assert!(conn.pending_dock.is_none());
        assert!(!conn.server.world.lock().parks[&(system as i64)]
            .ships
            .contains_key(&(hull as i64)));
        assert!(conn.server.world.lock().clients_of(pilot as i64).is_empty());

        // A fresh character selection reconstructs the ship before GetFormations.
        let _reconnected = conn.server.world.lock().connect(conn.client_id);
        conn.session = crate::session::Session {
            user_id: Some(pilot as i64),
            ..Default::default()
        };
        conn.select_character(&request, pilot as i64).await.unwrap();
        assert_eq!(
            conn.session.station_id, None,
            "persisted stationID=0 means space, not station zero"
        );
        assert_eq!(conn.session.solar_system_id, Some(system as i64));
        assert!(conn.server.world.lock().parks[&(system as i64)]
            .ships
            .contains_key(&(hull as i64)));
        conn.dock(&request, station as i64, false).await.unwrap();
        assert!(conn.pending_dock.is_some());
        conn.server
            .world
            .lock()
            .parks
            .get_mut(&(system as i64))
            .unwrap()
            .ships
            .get_mut(&(hull as i64))
            .unwrap()
            .pos = Vec3::new(2000.0, 0.0, 0.0);
        conn.poll_docking().await.unwrap();
        assert!(conn.pending_dock.is_none());
        assert_eq!(conn.session.station_id, Some(station as i64));
        assert!(!conn.server.world.lock().parks[&(system as i64)]
            .ships
            .contains_key(&(hull as i64)));
        let saved: i32 = conn
            .db
            .query_one(
                r#"SELECT "stationID" FROM "character_" WHERE "characterID"=$1"#,
                &[&pilot],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(saved, station);
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn dock_undock_and_reconnect_persist_all_locations_or_none() {
        let db = crate::test_support::Database::load().await;
        let pilot = db.id().await;
        let hull = db.id().await;
        let place=db.db.query_one(r#"SELECT "stationID","solarSystemID","constellationID","regionID" FROM "staStations" ORDER BY "stationID" LIMIT 1"#,&[]).await.unwrap();
        let station = place.get::<_, i32>(0);
        let system = place.get::<_, i32>(1);
        db.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","shipID","stationID","solarSystemID") VALUES ($1,$2,$3,$4,$5)"#,&[&pilot,&format!("Dock fixture {pilot}"),&hull,&station,&system]).await.unwrap();
        db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES ($1,587,$2,$3,4,1,1),($2,1377,$2,$3,0,1,1)"#,&[&hull,&pilot,&station]).await.unwrap();
        let session = crate::session::Session {
            char_id: Some(pilot as i64),
            ship_id: Some(hull as i64),
            station_id: Some(station as i64),
            solar_system_id2: Some(system as i64),
            ..Default::default()
        };
        let mut ship = inflight::ship_for_space(&db.ctx(&session), pilot as i64, hull as i64)
            .await
            .unwrap();
        ship.pos = Vec3::new(1234.0, 5678.0, 9012.0);
        let fight = ship.fight.as_mut().unwrap();
        fight.vitals.armor_damage = 12.0;
        fight.vitals.capacitor.recharge_ms = 0.0;
        fight.vitals.capacitor.set(crate::combat::now_ms(), 37.0);
        persist_undock(&db.url, pilot as i64, station as i64, system as i64, &ship)
            .await
            .unwrap();
        let saved=db.db.query_one(r#"SELECT c."stationID",s."locationID",p."locationID",s.x,s.y,s.z FROM "character_" c JOIN entity s ON s."itemID"=c."shipID" JOIN entity p ON p."itemID"=c."characterID" WHERE c."characterID"=$1"#,&[&pilot]).await.unwrap();
        assert_eq!(saved.get::<_, i32>(0), 0);
        assert_eq!(saved.get::<_, i32>(1), system);
        assert_eq!(saved.get::<_, i32>(2), system);
        assert_eq!(
            (
                saved.get::<_, f64>(3),
                saved.get::<_, f64>(4),
                saved.get::<_, f64>(5)
            ),
            (1234.0, 5678.0, 9012.0)
        );
        assert!(
            persist_undock(&db.url, pilot as i64, station as i64, system as i64, &ship)
                .await
                .is_err()
        );
        let resumed = reconnect_ship(&db.ctx(&session), pilot as i64, hull as i64, system as i64)
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
        db.db
            .execute(
                r#"UPDATE entity SET "ownerID"=0 WHERE "itemID"=$1"#,
                &[&hull],
            )
            .await
            .unwrap();
        assert!(
            persist_dock(&db.url, pilot as i64, system as i64, station as i64, &ship)
                .await
                .is_err()
        );
        assert!(
            reconnect_ship(&db.ctx(&session), pilot as i64, hull as i64, system as i64)
                .await
                .is_err()
        );
        let still: i32 = db
            .db
            .query_one(
                r#"SELECT "stationID" FROM "character_" WHERE "characterID"=$1"#,
                &[&pilot],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(still, 0);
        db.db
            .execute(
                r#"UPDATE entity SET "ownerID"=$2 WHERE "itemID"=$1"#,
                &[&hull, &pilot],
            )
            .await
            .unwrap();
        persist_dock(&db.url, pilot as i64, system as i64, station as i64, &ship)
            .await
            .unwrap();
        let saved=db.db.query_one(r#"SELECT c."stationID",s."locationID",p."locationID",s.flag FROM "character_" c JOIN entity s ON s."itemID"=c."shipID" JOIN entity p ON p."itemID"=c."characterID" WHERE c."characterID"=$1"#,&[&pilot]).await.unwrap();
        assert_eq!(
            (
                saved.get::<_, i32>(0),
                saved.get::<_, i32>(1),
                saved.get::<_, i32>(2),
                saved.get::<_, i32>(3)
            ),
            (station, station, station, 4)
        );
        let charges:i64=db.db.query_one(r#"SELECT count(*) FROM entity_attributes WHERE "itemID"=$1 AND "attributeID" IN (18,264)"#,&[&hull]).await.unwrap().get(0);
        assert_eq!(charges, 0);
        let other=db.db.query_one(r#"SELECT "solarSystemID","constellationID","regionID" FROM "mapSolarSystems" WHERE "solarSystemID"<>$1 ORDER BY "solarSystemID" LIMIT 1"#,&[&system]).await.unwrap();
        ship.pos = Vec3::new(999.0, 888.0, 777.0);
        persist_relocation(
            &db.url,
            pilot as i64,
            system as i64,
            Some(station as i64),
            other.get::<_, i32>(0) as i64,
            None,
            other.get::<_, i32>(1) as i64,
            other.get::<_, i32>(2) as i64,
            &ship,
        )
        .await
        .unwrap();
        assert_eq!(
            reconnect_ship(
                &db.ctx(&session),
                pilot as i64,
                hull as i64,
                other.get::<_, i32>(0) as i64
            )
            .await
            .unwrap()
            .pos,
            ship.pos
        );
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE pointing at a disposable imported database"]
    async fn jump_persistence_moves_pilot_and_ship_or_neither() {
        let fixture = crate::test_support::Database::load().await;
        let pilot = fixture.id().await;
        let ship = fixture.id().await;
        fixture
            .db
            .execute(
                r#"INSERT INTO "character_" ("characterID","characterName","shipID","solarSystemID")
            VALUES ($1,$2,$3,30000142)"#,
                &[&pilot, &format!("Jump test {pilot}"), &ship],
            )
            .await
            .unwrap();
        fixture
            .db
            .execute(
                r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",quantity,singleton)
            VALUES ($1,587,$2,30000142,1,1)"#,
                &[&ship, &pilot],
            )
            .await
            .unwrap();
        let at = Vec3::new(123.0, 456.0, 789.0);
        let mut arrival = JumpArrival {
            from: 30000142,
            system: 30000143,
            constellation: 20000020,
            region: 10000002,
            at,
        };
        persist_jump_state(
            &fixture.url,
            i64::from(pilot),
            i64::from(ship),
            &arrival,
            None,
            None,
            None,
        )
        .await
        .unwrap();
        let saved = fixture
            .db
            .query_one(
                r#"SELECT c."solarSystemID",e."locationID",e.x,e.y,e.z
            FROM "character_" c JOIN entity e ON e."itemID"=c."shipID" WHERE c."characterID"=$1"#,
                &[&pilot],
            )
            .await
            .unwrap();
        assert_eq!(
            (saved.get::<_, i32>(0), saved.get::<_, i32>(1)),
            (30000143, 30000143)
        );
        assert_eq!(
            (
                saved.get::<_, f64>(2),
                saved.get::<_, f64>(3),
                saved.get::<_, f64>(4)
            ),
            (123.0, 456.0, 789.0)
        );
        fixture
            .db
            .execute(
                r#"UPDATE entity SET "ownerID"=0 WHERE "itemID"=$1"#,
                &[&ship],
            )
            .await
            .unwrap();
        arrival.from = 30000143;
        arrival.system = 30000144;
        assert!(persist_jump_state(
            &fixture.url,
            i64::from(pilot),
            i64::from(ship),
            &arrival,
            None,
            None,
            None
        )
        .await
        .is_err());
        let saved = fixture
            .db
            .query_one(
                r#"SELECT "solarSystemID" FROM "character_" WHERE "characterID"=$1"#,
                &[&pilot],
            )
            .await
            .unwrap();
        assert_eq!(
            saved.get::<_, i32>(0),
            30000143,
            "ship write failure rolls back the pilot write"
        );
        assert!(persist_jump_state(
            &fixture.url,
            i64::from(pilot),
            i64::from(ship) + 1,
            &arrival,
            None,
            None,
            None
        )
        .await
        .is_err());
        fixture
            .db
            .execute(
                r#"DELETE FROM "character_" WHERE "characterID"=$1"#,
                &[&pilot],
            )
            .await
            .unwrap();
        fixture
            .db
            .execute(r#"DELETE FROM entity WHERE "itemID"=$1"#, &[&ship])
            .await
            .unwrap();
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE pointing at a disposable imported database"]
    async fn paid_jump_failure_and_duplicate_completion_never_lose_or_repeat_payment() {
        let fixture = crate::test_support::Database::load().await;
        let pilot = fixture.id().await;
        let ship_id = fixture.id().await;
        let fuel = fixture.id().await;
        fixture
            .db
            .execute(
                r#"INSERT INTO "character_" ("characterID","characterName","shipID","solarSystemID")
            VALUES ($1,$2,$3,30000142)"#,
                &[&pilot, &format!("Paid jump {pilot}"), &ship_id],
            )
            .await
            .unwrap();
        fixture
            .db
            .execute(
                r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",quantity,singleton)
            VALUES ($1,587,$2,30000142,1,1)"#,
                &[&ship_id, &pilot],
            )
            .await
            .unwrap();
        fixture
            .db
            .execute(
                r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity)
            VALUES ($1,16273,$2,$3,5,8)"#,
                &[&fuel, &pilot, &ship_id],
            )
            .await
            .unwrap();
        let profile = inflight::profile(
            &fixture.db,
            &fixture.dogma,
            i64::from(pilot),
            i64::from(ship_id),
            &Default::default(),
            crate::combat::now_ms(),
        )
        .await
        .unwrap()
        .unwrap();
        let mut ship = Ship::new(
            i64::from(ship_id),
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
                cargo: i64::from(ship_id),
                type_id: 16273,
                quantity: 3,
                flag: Some(5),
            },
            capacitor: 100.0,
            clients: vec![],
        };
        // The ship was transferred or destroyed during the jump-out delay.
        fixture
            .db
            .execute(
                r#"UPDATE entity SET "ownerID"=0 WHERE "itemID"=$1"#,
                &[&ship_id],
            )
            .await
            .unwrap();
        for _ in 0..2 {
            assert!(
                complete_paid_jump(&fixture.url, i64::from(pilot), &mut ship, &arrival, &jump, None)
                    .await
                    .is_err()
            );
            assert_eq!(ship.fight.as_ref().unwrap().vitals.capacitor, before);
        }
        let have: i32 = fixture
            .db
            .query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#, &[&fuel])
            .await
            .unwrap()
            .get(0);
        assert_eq!(
            have, 8,
            "failed callbacks never debit or manufacture a refund"
        );
        fixture
            .db
            .execute(
                r#"UPDATE entity SET "ownerID"=$2 WHERE "itemID"=$1"#,
                &[&ship_id, &pilot],
            )
            .await
            .unwrap();
        // Fuel available at preparation was consumed during the delay.
        fixture
            .db
            .execute(
                r#"UPDATE entity SET quantity=2 WHERE "itemID"=$1"#,
                &[&fuel],
            )
            .await
            .unwrap();
        assert!(
            complete_paid_jump(&fixture.url, i64::from(pilot), &mut ship, &arrival, &jump, None)
                .await
                .is_err()
        );
        assert_eq!(ship.fight.as_ref().unwrap().vitals.capacitor, before);
        let positions = fixture
            .db
            .query_one(
                r#"SELECT c."solarSystemID",e."locationID" FROM "character_" c
            JOIN entity e ON e."itemID"=c."shipID" WHERE c."characterID"=$1"#,
                &[&pilot],
            )
            .await
            .unwrap();
        assert_eq!(
            (positions.get::<_, i32>(0), positions.get::<_, i32>(1)),
            (30000142, 30000142),
            "fuel refusal rolls back both arrival writes"
        );
        fixture
            .db
            .execute(
                r#"UPDATE entity SET quantity=8 WHERE "itemID"=$1"#,
                &[&fuel],
            )
            .await
            .unwrap();
        complete_paid_jump(&fixture.url, i64::from(pilot), &mut ship, &arrival, &jump, None)
            .await
            .unwrap();
        assert!(
            complete_paid_jump(&fixture.url, i64::from(pilot), &mut ship, &arrival, &jump, None)
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
        let state = fixture
            .db
            .query_one(
                r#"SELECT f.quantity,a."valueFloat" FROM entity f
            JOIN entity_attributes a ON a."itemID"=$2 AND a."attributeID"=18 WHERE f."itemID"=$1"#,
                &[&fuel, &ship_id],
            )
            .await
            .unwrap();
        assert_eq!(
            state.get::<_, i32>(0),
            5,
            "successful completion debits only once"
        );
        assert_eq!(
            state.get::<_, f64>(1),
            900.0,
            "capacitor is saved with the arrival"
        );
        let next = JumpArrival {
            from: 30000143,
            system: 30000144,
            ..arrival
        };
        let (a, b) = tokio::join!(
            persist_jump_state(
                &fixture.url,
                i64::from(pilot),
                i64::from(ship_id),
                &next,
                Some(&jump.fuel),
                Some(800.0),
                None
            ),
            persist_jump_state(
                &fixture.url,
                i64::from(pilot),
                i64::from(ship_id),
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
        let have: i32 = fixture
            .db
            .query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#, &[&fuel])
            .await
            .unwrap()
            .get(0);
        assert_eq!(have, 2);
        fixture
            .db
            .execute(
                r#"DELETE FROM "character_" WHERE "characterID"=$1"#,
                &[&pilot],
            )
            .await
            .unwrap();
        fixture
            .db
            .execute(
                r#"DELETE FROM entity WHERE "itemID"=ANY($1)"#,
                &[&vec![ship_id, fuel]],
            )
            .await
            .unwrap();
    }
}

/// Commit the character and its ship together before publishing a successful
/// jump. A failed write leaves the pilot and ship at their original location.
#[cfg(test)]
async fn persist_jump_state(
    db_url: &str,
    char_id: i64,
    ship_id: i64,
    arrival: &JumpArrival,
    fuel: Option<&FuelCost>,
    capacitor: Option<f64>,
    fighter: Option<&crate::combat::Fighter>,
) -> std::result::Result<Vec<Value>, String> {
    persist_jump_state_guarded(db_url,char_id,ship_id,arrival,fuel,capacitor,fighter,None).await
}

#[cfg(test)]
async fn persist_jump_state_guarded(
    db_url:&str,char_id:i64,ship_id:i64,arrival:&JumpArrival,fuel:Option<&FuelCost>,
    capacitor:Option<f64>,fighter:Option<&crate::combat::Fighter>,departure:Option<&Departure<'_>>,
) -> std::result::Result<Vec<Value>,String> {
    let failed = |error: tokio_postgres::Error| {
        error!("saving jump: {}", crate::logging::chain(&error));
        "Your jump could not be saved. Please try again.".to_string()
    };
    let mut db = crate::db::connect(db_url).await.map_err(failed)?;
    let mut txn = crate::items::ItemTxn::begin(&mut db)
        .await
        .map_err(failed)?;
    if !crate::svc::login::lock_character_lifecycle(txn.tx(),char_id as i32).await.map_err(failed)? {
        return Err("Your character is no longer available for this jump.".into());
    }
    // The source predicates are the completion guard: a repeated callback
    // from the previous system cannot debit fuel or capacitor a second time.
    let pilot=txn.tx().execute(r#"UPDATE "character_" SET "solarSystemID"=$2,"constellationID"=$3,"regionID"=$4
        WHERE "characterID"=$1 AND "shipID"=$5 AND "solarSystemID"=$6 AND COALESCE("stationID",0)=0"#,
        &[&(char_id as i32),&(arrival.system as i32),&(arrival.constellation as i32),&(arrival.region as i32),&(ship_id as i32),&(arrival.from as i32)]).await.map_err(failed)?;
    let ship = txn
        .tx()
        .execute(
            r#"UPDATE entity SET "locationID"=$2,x=$3,y=$4,z=$5
        WHERE "itemID"=$1 AND "ownerID"=$6 AND "locationID"=$7"#,
            &[
                &(ship_id as i32),
                &(arrival.system as i32),
                &arrival.at.x,
                &arrival.at.y,
                &arrival.at.z,
                &(char_id as i32),
                &(arrival.from as i32),
            ],
        )
        .await
        .map_err(failed)?;
    if pilot != 1 || ship != 1 {
        return Err("Your ship has moved or is no longer available for this jump.".into());
    }
    if let Some(departure)=departure {departure.lock_endpoints(txn.tx()).await?;}
    txn.tx()
        .execute(
            r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,
            &[&(char_id as i32), &(arrival.system as i32)],
        )
        .await
        .map_err(failed)?;
    if let Some(fuel) = fuel {
        fuel.debit(&mut txn).await?;
    }
    if let Some(fighter) = fighter {
        inflight::write_vitals(txn.tx(), ship_id, fighter, false)
            .await
            .map_err(failed)?;
    }
    if let Some(capacitor) = capacitor {
        txn.tx()
            .execute(
                r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt","valueFloat")
            VALUES ($1,18,NULL,$2) ON CONFLICT ("itemID","attributeID")
            DO UPDATE SET "valueInt"=NULL,"valueFloat"=$2"#,
                &[&(ship_id as i32), &capacitor],
            )
            .await
            .map_err(failed)?;
    }
    if let Some(departure)=departure {departure.accept()?;}
    txn.commit().await.map_err(failed)
}

fn travel_failed(error: tokio_postgres::Error) -> String {
    error!("saving travel: {}", crate::logging::chain(&error));
    "Your ship's location could not be saved. Please try again.".into()
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
    db_url: &str,
    char_id: i64,
    station: i64,
    system: i64,
    ship: &Ship,
) -> std::result::Result<(), String> {
    let mut db = crate::db::connect(db_url).await.map_err(travel_failed)?;
    let tx = db.transaction().await.map_err(travel_failed)?;
    let pilot=tx.execute(r#"UPDATE "character_" SET "stationID"=0 WHERE "characterID"=$1 AND "shipID"=$2 AND "stationID"=$3 AND "solarSystemID"=$4 AND EXISTS (SELECT 1 FROM "staStations" WHERE "stationID"=$3 AND "solarSystemID"=$4)"#,&[&(char_id as i32),&(ship.id as i32),&(station as i32),&(system as i32)]).await.map_err(travel_failed)?;
    if crate::losses::pending::blocked_character(&tx,char_id).await.map_err(travel_failed)? {
        return Err("Your station arrival is awaiting its final combat state. Try again shortly.".into());
    }
    let moved=tx.execute(r#"UPDATE entity SET "locationID"=$2,flag=0,x=$3,y=$4,z=$5 WHERE "itemID"=$1 AND "ownerID"=$6 AND "locationID"=$7 AND flag=4"#,&[&(ship.id as i32),&(system as i32),&ship.pos.x,&ship.pos.y,&ship.pos.z,&(char_id as i32),&(station as i32)]).await.map_err(travel_failed)?;
    if moved != 1 || pilot != 1 {
        return Err("Your active ship is no longer in that station.".into());
    }
    tx.execute(
        r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,
        &[&(char_id as i32), &(system as i32)],
    )
    .await
    .map_err(travel_failed)?;
    if let Some(fight) = &ship.fight {
        inflight::write_vitals(&tx, ship.id, fight, false)
            .await
            .map_err(travel_failed)?;
    }
    tx.commit().await.map_err(travel_failed)
}

#[cfg(test)]
async fn persist_dock(
    db_url: &str,
    char_id: i64,
    from: i64,
    station: i64,
    ship: &Ship,
) -> std::result::Result<(i64, i64), String> {
    let mut db = crate::db::connect(db_url).await.map_err(travel_failed)?;
    let tx = db.transaction().await.map_err(travel_failed)?;
    let place=tx.query_opt(r#"SELECT "constellationID","regionID" FROM "staStations" WHERE "stationID"=$1 AND "solarSystemID"=$2"#,&[&(station as i32),&(from as i32)]).await.map_err(travel_failed)?.ok_or_else(||"That station is not in this solar system.".to_string())?;
    let pilot=tx.execute(r#"UPDATE "character_" SET "stationID"=$2,"constellationID"=$3,"regionID"=$4 WHERE "characterID"=$1 AND "shipID"=$5 AND "solarSystemID"=$6 AND COALESCE("stationID",0)=0"#,&[&(char_id as i32),&(station as i32),&place.get::<_,i32>(0),&place.get::<_,i32>(1),&(ship.id as i32),&(from as i32)]).await.map_err(travel_failed)?;
    let moved=tx.execute(r#"UPDATE entity SET "locationID"=$2,flag=4 WHERE "itemID"=$1 AND "ownerID"=$3 AND "locationID"=$4"#,&[&(ship.id as i32),&(station as i32),&(char_id as i32),&(from as i32)]).await.map_err(travel_failed)?;
    if moved != 1 || pilot != 1 {
        return Err("Your ship is no longer available to dock.".into());
    }
    tx.execute(
        r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,
        &[&(char_id as i32), &(station as i32)],
    )
    .await
    .map_err(travel_failed)?;
    if let Some(fight) = &ship.fight {
        inflight::write_vitals(&tx, ship.id, fight, true)
            .await
            .map_err(travel_failed)?;
    }
    tx.commit().await.map_err(travel_failed)?;
    Ok((place.get::<_, i32>(0) as i64, place.get::<_, i32>(1) as i64))
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
        .db
        .query_opt(
            r#"SELECT x,y,z FROM entity WHERE "itemID"=$1 AND "ownerID"=$2 AND "locationID"=$3"#,
            &[&(ship_id as i32), &(char_id as i32), &(system as i32)],
        )
        .await
        .map_err(travel_failed)?
        .ok_or_else(|| "Your active ship is no longer in this solar system.".to_string())?;
    let mut ship = inflight::ship_for_space(ctx, char_id, ship_id)
        .await
        .ok_or_else(|| "Your ship could not be loaded.".to_string())?;
    ship.pos = Vec3::new(saved.get(0), saved.get(1), saved.get(2));
    if !ship.pos.x.is_finite() || !ship.pos.y.is_finite() || !ship.pos.z.is_finite() {
        return Err("Your ship's saved position is invalid.".into());
    }
    crate::space_state::restore(ctx,system,&mut ship).await.map_err(travel_failed)?;
    Ok(ship)
}

#[cfg(test)]
async fn persist_relocation(
    db_url: &str,
    char_id: i64,
    from_system: i64,
    from_station: Option<i64>,
    system: i64,
    station: Option<i64>,
    constellation: i64,
    region: i64,
    ship: &Ship,
) -> std::result::Result<(), String> {
    let mut db = crate::db::connect(db_url).await.map_err(travel_failed)?;
    let tx = db.transaction().await.map_err(travel_failed)?;
    let destination = station.unwrap_or(system);
    let source = from_station.unwrap_or(from_system);
    let flag = if station.is_some() { 4_i32 } else { 0 };
    let pilot=tx.execute(r#"UPDATE "character_" SET "stationID"=$2,"solarSystemID"=$3,"constellationID"=$4,"regionID"=$5 WHERE "characterID"=$1 AND "shipID"=$6 AND "solarSystemID"=$7 AND COALESCE("stationID",0)=$8"#,&[&(char_id as i32),&(station.unwrap_or(0) as i32),&(system as i32),&(constellation as i32),&(region as i32),&(ship.id as i32),&(from_system as i32),&(from_station.unwrap_or(0) as i32)]).await.map_err(travel_failed)?;
    let moved=tx.execute(r#"UPDATE entity SET "locationID"=$2,flag=$3,x=$4,y=$5,z=$6 WHERE "itemID"=$1 AND "ownerID"=$7 AND "locationID"=$8"#,&[&(ship.id as i32),&(destination as i32),&flag,&ship.pos.x,&ship.pos.y,&ship.pos.z,&(char_id as i32),&(source as i32)]).await.map_err(travel_failed)?;
    if moved != 1 || pilot != 1 {
        return Err("Your active ship has moved and cannot be relocated.".into());
    }
    tx.execute(
        r#"UPDATE entity SET "locationID"=$2 WHERE "itemID"=$1"#,
        &[&(char_id as i32), &(destination as i32)],
    )
    .await
    .map_err(travel_failed)?;
    if let Some(fight) = &ship.fight {
        inflight::write_vitals(&tx, ship.id, fight, station.is_some())
            .await
            .map_err(travel_failed)?;
    }
    tx.commit().await.map_err(travel_failed)
}

/// Test-only lower-level persistence harness. Production drive/portal travel
/// uses departure::reserved and keeps the live hull in combat until commit.
/// The ship is detached from the ballpark while this runs, so its capacitor
/// cannot change beneath the transaction. On any refusal restore the exact
/// original curve; fuel and all persisted arrival state roll back together.
#[cfg(test)]
async fn complete_paid_jump(
    db_url: &str,
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
        db_url,
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
        let mut lifecycle_db = crate::db::connect(&self.server.db_url).await?;
        let lifecycle = lifecycle_db.transaction().await?;
        if !crate::svc::login::lock_character_lifecycle(&lifecycle, char_id as i32).await? {
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

        let sql = r#"SELECT c."characterID", c."stationID", c."solarSystemID",
                            c."constellationID", c."regionID", c."shipID",
                            c."characterName", c."typeID"
                       FROM "character_" c
                      WHERE c."characterID" = $1 AND c."accountID" = $2"#;
        let user_id = self.session.user_id.unwrap_or(0);
        let rows = match self
            .db
            .query(sql, &[&(char_id as i32), &(user_id as i32)])
            .await
        {
            Ok(r) => r,
            Err(e) => {
                error!("loading character {char_id}: {}", crate::logging::chain(&e));
                let err = custom_error("That character could not be loaded.");
                return self.send_error(req, err).await;
            }
        };
        let Some(row) = rows.first() else {
            warn!("character {char_id} is not on account {user_id}");
            let err = custom_error("That character is not on this account.");
            return self.send_error(req, err).await;
        };

        if crate::losses::journal::blocked_character_in_memory(&self.server.world,char_id)
            || crate::travel::blocked_character_in_memory(&self.server.world,char_id)
            || crate::svc::jumps::portal::blocked_character_in_memory(&self.server.world,char_id)
            || crate::conn::logout::blocked_character_in_memory(&self.server.world,char_id)
            || crate::losses::pending::blocked_character(&*self.db,char_id).await? {
            return self.send_error(req,custom_error("That character's station arrival is awaiting its final combat state. Try again shortly.")).await;
        }

        let opt = |i: usize| crate::svc::rows::int(row, i);
        self.session.char_id = Some(char_id);
        self.session.char_name = crate::svc::rows::text(row, 6).unwrap_or_default();
        self.session.char_type_id = opt(7).unwrap_or(0);
        self.session.station_id = opt(1).filter(|id| *id > 0);
        self.session.solar_system_id = opt(2);
        self.session.constellation_id = opt(3);
        self.session.region_id = opt(4);
        self.session.solar_system_id2 = opt(2);
        if self.session.station_id.is_some() {
            self.session.solar_system_id = None;
        }
        let ship = opt(5);
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
        lifecycle.commit().await?;
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
            crate::svc::pvptrade::left_station(&self.server.db_url, &self.server.world, char_id)
                .await;
        }
        if let Err(reason) =
            persist_undock(&self.server.db_url, char_id, station_id, system_id, &ship).await
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
            if !crate::svc::outposts::may_use(&*self.db, station_id, SERVICE_DOCKING, char_id)
                .await
            {
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
        if let Err(error) = crate::svc::map::record_activity(&self.db, system_id, 1).await {
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
        let place = self
            .db
            .query_opt(r#"SELECT "constellationID", "regionID" FROM "mapSolarSystems" WHERE "solarSystemID" = $1"#, &[&(to as i32)])
            .await
            .ok()
            .flatten();
        let Some(place) = place else {
            return self
                .send_error(req, custom_error("That system does not exist."))
                .await;
        };
        let (constellation, region) = (
            crate::svc::rows::int(&place, 0).unwrap_or(0),
            crate::svc::rows::int(&place, 1).unwrap_or(0),
        );
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
        // Own the payment/publication future: cancellation while PostgreSQL
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
        if let Err(error) = crate::svc::map::record_activity(&self.db, from, 1).await {
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
        self.refresh_database().await?;
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
        const SQL: &str = r#"
            SELECT c."corporationID", co."allianceID", co."stationID",
                   COALESCE(r."rolesAtAll", 0), COALESCE(r."rolesAtHQ", 0),
                   COALESCE(r."rolesAtBase", 0), COALESCE(r."rolesAtOther", 0)
              FROM "character_" c
              LEFT JOIN "corporation" co ON co."corporationID" = c."corporationID"
              LEFT JOIN "chrCorporationRoles" r ON r."characterID" = c."characterID"
             WHERE c."characterID" = $1"#;
        let row = match self.db.query_opt(SQL, &[&(char_id as i32)]).await {
            Ok(Some(row)) => row,
            Ok(None) => return,
            Err(e) => {
                error!(
                    "loading the corporation of {char_id}: {}",
                    crate::logging::chain(&e)
                );
                return;
            }
        };
        let opt = |i: usize| crate::svc::rows::int(&row, i);
        let s = &mut self.session;
        s.corp_id = opt(0);
        s.alliance_id = opt(1).filter(|v| *v != 0);
        s.hq_id = opt(2).filter(|v| *v != 0);
        s.base_id = s.hq_id;
        s.roles_at_all = opt(3).unwrap_or(0);
        s.roles_at_hq = opt(4).unwrap_or(0);
        s.roles_at_base = opt(5).unwrap_or(0);
        s.roles_at_other = opt(6).unwrap_or(0);
    }

    /// Pick the character's ship in `station_id` (preferring the recorded one) and
    /// store it as their active ship.
    async fn resolve_active_ship(&self, char_id: i64, station_id: i64) -> Option<i64> {
        const SQL: &str = r#"
            WITH candidate AS (
                SELECT e."itemID"
                  FROM entity e
                  JOIN "invTypes"  t ON t."typeID"  = e."typeID"
                  JOIN "invGroups" g ON g."groupID" = t."groupID"
                 WHERE e."ownerID"    = $1
                   AND e."locationID" = $2
                   AND g."categoryID" = 6
                   AND e."singleton"  = 1
                 ORDER BY (e."itemID" = (SELECT c."shipID" FROM "character_" c
                                          WHERE c."characterID" = $1)) DESC,
                          e."itemID"
                 LIMIT 1
            )
            UPDATE "character_" SET "shipID" = (SELECT "itemID" FROM candidate)
             WHERE "characterID" = $1
             RETURNING "shipID""#;
        match self
            .db
            .query_opt(SQL, &[&(char_id as i32), &(station_id as i32)])
            .await
        {
            Ok(row) => row.and_then(|r| crate::svc::rows::int(&r, 0)),
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
