use super::*;
mod reservation;
mod commit_recovery;
use crate::{
    ballpark::{Park, Vec3},
    beacons::{Beacon, Kind},
    session::Session,
    store::Value as StoreValue,
    test_support::Database,
};

fn int(v: i64) -> StoreValue {
    StoreValue::Int(v)
}

/// INSERT one fixture row.
fn insert(db: &Database, table: &str, sets: &[(&str, StoreValue)]) {
    db.store.write(|tx| tx.insert(table, sets).map(|_| ())).unwrap();
}

/// UPDATE the row with primary key `key`.
fn update(db: &Database, table: &str, key: &[StoreValue], sets: &[(&str, StoreValue)]) {
    db.store
        .write(|tx| {
            let row = tx.table(table).get(key).unwrap().id;
            tx.update(table, row, sets)
        })
        .unwrap();
}

struct Fixture {
    db: Database,
    session: Session,
    ship: Ship,
    fuel: i64,
    beacon: i64,
    member: i64,
    corp: i64,
}
impl Fixture {
    async fn server(&self)->crate::conn::Server {
        let dogma=crate::dogma::Catalog::new();
        crate::conn::Server {table:Default::default(),store:self.db.store.clone(),
            node_id:1,bulk:crate::bulkdata::BulkData {entries:Default::default(),node_id:1},dogma,
            world:self.db.world.clone(),services:crate::svc::registry(),dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0}
    }
    async fn new() -> Self {
        let db = Database::load().await;
        let pilot = i64::from(db.id().await);
        let hull = i64::from(db.id().await);
        let fuel = i64::from(db.id().await);
        let corp = i64::from(db.id().await);
        let beacon = i64::from(db.id().await);
        let member = i64::from(db.id().await);
        let alliance = i64::from(db.id().await);
        insert(&db, "corporation", &[("corporationID", int(corp)), ("corporationName", StoreValue::Text(format!("Departure corp {corp}").into())),
            ("allianceID", int(alliance))]);
        insert(&db, "character_", &[("characterID", int(pilot)), ("characterName", StoreValue::Text(format!("Departure pilot {pilot}").into())),
            ("corporationID", int(corp)), ("shipID", int(hull)), ("solarSystemID", int(30000142)), ("stationID", int(0))]);
        for (id, ty, location, quantity, flag, singleton) in
            [(hull, 587, 30000142, 1, 0, 1), (fuel, 16273, hull, 8, 5, 0)]
        {
            insert(&db, "entity", &[("itemID", int(id)), ("typeID", int(ty)), ("ownerID", int(pilot)), ("locationID", int(location)),
                ("quantity", int(quantity)), ("flag", int(flag)), ("singleton", int(singleton))]);
        }
        let session = Session {
            char_id: Some(pilot),
            ship_id: Some(hull),
            corp_id: Some(corp),
            alliance_id: Some(alliance),
            solar_system_id: Some(30000142),
            ..Default::default()
        };
        let mut ship = inflight::ship_for_space(&db.ctx(&session), pilot, hull)
            .await
            .unwrap();
        ship.fight.as_mut().unwrap().vitals.capacitor =
            crate::combat::Charged::full(1000.0, 0.0, crate::combat::now_ms());
        let mut source = Park::new(30000142, vec![], destiny::tick_now());
        source.ships.insert(hull, ship.clone());
        let mut dest = Park::new(30000143, vec![], destiny::tick_now());
        dest.beacons.insert(
            beacon,
            Beacon::new(
                beacon,
                21094,
                Some(member),
                "Test field",
                Vec3::ZERO,
                1000.0,
                Kind::Cyno {
                    char_id: member,
                    ship_id: member + 1,
                },
            ),
        );
        {
            let mut w = db.world.lock();
            w.parks.insert(30000142, source);
            w.parks.insert(30000143, dest);
            w.gangs.test_members(vec![pilot, member]);
        }
        Self {
            db,
            session,
            ship,
            fuel,
            beacon,
            member,
            corp,
        }
    }
    fn capture(&self, request: JumpRequest) -> Departure<'_> {
        Departure::capture(
            &self.db.ctx(&self.session),
            &self.db.world.lock(),
            request,
            30000143,
        )
        .unwrap()
    }
    fn arrival(&self) -> JumpArrival {
        JumpArrival {
            from: 30000142,
            system: 30000143,
            constellation: 20000020,
            region: 10000002,
            at: Vec3::new(4000.0, 0.0, 0.0),
        }
    }
    fn quote(&self) -> PreparedJump {
        PreparedJump {
            at: self.arrival().at,
            fuel: FuelCost {
                cargo: self.ship.id,
                type_id: 16273,
                quantity: 3,
                flag: Some(5),
            },
            capacitor: 100.0,
            clients: vec![1],
        }
    }
    /// The pilot's system, its active ship's location and the fuel left.
    fn saved(&self) -> (Option<i64>, Option<i64>, Option<i64>, Vec3) {
        self.db.store.read(|db| {
            let c = db.table("character_").get_id(self.ship.pilot.char_id).unwrap();
            let e = db.table("entity").get_id(c.int("shipID").unwrap()).unwrap();
            let fuel = db.table("entity").get_id(self.fuel).unwrap();
            let at = |column: &str| e.float(column).unwrap();
            (c.int("solarSystemID"), e.int("locationID"), fuel.int("quantity"), Vec3::new(at("x"), at("y"), at("z")))
        })
    }
    async fn unchanged(&self, ship: &Ship) {
        let (system, location, fuel, _) = self.saved();
        assert_eq!((system, location, fuel), (Some(30000142), Some(30000142), Some(8)));
        assert_eq!(
            ship.fight
                .as_ref()
                .unwrap()
                .vitals
                .capacitor
                .at(crate::combat::now_ms()),
            1000.0
        );
    }
}

#[tokio::test]
async fn field_extinguishing_before_payment_rolls_back_location_fuel_and_capacitor() {
    let f = Fixture::new().await;
    f.db.world.lock().maintenance.insert(
        f.ship.id,
        crate::svc::maintenance::Configuration {
            pilot: f.ship.pilot.char_id,
            system: 30000142,
            allow_fleet: true,
        },
    );
    let guard = f.capture(JumpRequest::Beacon {
        member: f.member,
        beacon: f.beacon,
    });
    space::remove_ship(&mut f.db.world.lock(), 30000142, f.ship.id).unwrap();
    let mut ship = f.ship.clone();
    let arrival = f.arrival();
    let quote = f.quote();
    // The field goes out after the departure was captured, before its
    // payment is written: the live recheck in that write refuses it.
    f.db.world
        .lock()
        .parks
        .get_mut(&30000143)
        .unwrap()
        .beacons
        .remove(&f.beacon);
    let result = complete_paid_jump(
        &f.db.store,
        ship.pilot.char_id,
        &mut ship,
        &arrival,
        &quote,
        Some(&guard),
    )
    .await;
    assert!(result.unwrap_err().contains("gone out"));
    f.unchanged(&ship).await;
    restore_departure(&mut f.db.world.lock(), 30000142, ship, guard.configuration);
    assert!(
        f.db.world.lock().maintenance[&f.ship.id].allow_fleet,
        "refused travel preserves the same-system maintenance preference"
    );
    assert!(Departure::capture(
        &f.db.ctx(&f.session),
        &f.db.world.lock(),
        JumpRequest::Beacon {
            member: f.member,
            beacon: f.beacon
        },
        30000143
    )
    .is_err());
}

#[tokio::test]
async fn fixed_beacon_state_and_alliance_are_rechecked_before_payment() {
    let f = Fixture::new().await;
    let ty = i64::from(crate::sde::get().types_in_group(838)[0]);
    insert(&f.db, "entity", &[("itemID", int(f.beacon)), ("typeID", int(ty)), ("ownerID", int(f.corp)), ("locationID", int(30000143)),
        ("quantity", int(1)), ("flag", int(0)), ("singleton", int(1))]);
    insert(&f.db, "posStructures", &[("itemID", int(f.beacon)), ("state", int(1))]);
    let guard = f.capture(JumpRequest::Alliance { beacon: f.beacon });
    let mut ship = f.ship.clone();
    assert!(complete_paid_jump(
        &f.db.store,
        ship.pilot.char_id,
        &mut ship,
        &f.arrival(),
        &f.quote(),
        Some(&guard)
    )
    .await
    .unwrap_err()
    .contains("online"));
    f.unchanged(&ship).await;
    update(&f.db, "posStructures", &[int(f.beacon)], &[("state", int(4))]);
    update(&f.db, "corporation", &[int(f.corp)], &[("allianceID", int(0))]);
    assert!(complete_paid_jump(
        &f.db.store,
        ship.pilot.char_id,
        &mut ship,
        &f.arrival(),
        &f.quote(),
        Some(&guard)
    )
    .await
    .unwrap_err()
    .contains("alliance"));
    f.unchanged(&ship).await;
    update(&f.db, "corporation", &[int(f.corp)], &[("allianceID", int(f.session.alliance_id.unwrap()))]);
    complete_paid_jump(
        &f.db.store,
        ship.pilot.char_id,
        &mut ship,
        &f.arrival(),
        &f.quote(),
        Some(&guard),
    )
    .await
    .unwrap();
    assert_eq!(
        ship.fight
            .as_ref()
            .unwrap()
            .vitals
            .capacitor
            .at(crate::combat::now_ms()),
        900.0
    );
    assert_eq!(f.saved().2, Some(5));
}

fn arrived(f: &Fixture, ship: &Ship, capacitor: f64) {
    let (system, location, fuel, at) = f.saved();
    assert_eq!((system, location, fuel), (Some(30000143), Some(30000143), Some(5)));
    assert_eq!(at, f.arrival().at);
    assert_eq!(
        ship.fight
            .as_ref()
            .unwrap()
            .vitals
            .capacitor
            .at(crate::combat::now_ms()),
        capacitor
    );
}

#[tokio::test]
async fn portal_departure_rechecks_generator_harmonics_and_live_bridge_before_committing_payment() {
    let f = Fixture::new().await;
    let host = i64::from(f.db.id().await);
    let hull = i64::from(f.db.id().await);
    let generator = i64::from(f.db.id().await);
    insert(&f.db, "character_", &[("characterID", int(host)), ("characterName", StoreValue::Text(format!("Portal host {host}").into())),
        ("corporationID", int(f.corp)), ("shipID", int(hull)), ("solarSystemID", int(30000142)), ("stationID", int(0))]);
    for (id, ty, location, flag) in [(hull, 22428, 30000142, 0), (generator, 28652, hull, 27)] {
        insert(&f.db, "entity", &[("itemID", int(id)), ("typeID", int(ty)), ("ownerID", int(host)), ("locationID", int(location)),
            ("flag", int(flag)), ("quantity", int(1)), ("singleton", int(1))]);
    }
    insert(&f.db, "entity_attributes", &[("itemID", int(generator)), ("attributeID", int(2)), ("valueInt", int(1))]);
    insert(&f.db, "entity_attributes", &[("itemID", int(f.ship.id)), ("attributeID", int(1253)), ("valueInt", int(2))]);
    update(&f.db, "entity", &[int(f.fuel)], &[("ownerID", int(host)), ("locationID", int(hull))]);
    let host_session = Session {
        char_id: Some(host),
        ship_id: Some(hull),
        ..f.session.clone()
    };
    let mut host_ship =
        inflight::ship_for_space(&f.db.ctx(&host_session), host, hull)
            .await
            .unwrap();
    host_ship.pos = f.ship.pos;
    let bridge = Bridge {
        generation: 0, started_ms: 0,
        titan_pilot: host,
        generator_id: generator,
        system: 30000142,
        to_system: 30000143,
        beacon: f.beacon,
        member: Some(f.member),
        until_ms: crate::combat::now_ms() + 60000,
        mass_factor: 1.8e-7,
        harmonics: 2,
    };
    {
        let mut w = f.db.world.lock();
        w.parks
            .get_mut(&30000142)
            .unwrap()
            .ships
            .insert(hull, host_ship);
        w.bridges.insert(hull, bridge.clone());
        w.gangs=Default::default();
        w.gangs
            .test_members(vec![f.ship.pilot.char_id, host, f.member]);
    }
    let guard = f.capture(JumpRequest::Portal {
        pilot: host,
        ship: hull,
        alliance: false,
    });
    let generator_online = |online: i64| update(&f.db, "entity_attributes", &[int(generator), int(2)], &[("valueInt", int(online))]);
    let harmonics = |level: i64| update(&f.db, "entity_attributes", &[int(f.ship.id), int(1253)], &[("valueInt", int(level))]);
    // The generator goes offline after the departure was captured.
    generator_online(0);
    let mut ship = f.ship.clone();
    let arrival = f.arrival();
    let mut quote = f.quote();
    quote.fuel.cargo = hull;
    quote.capacitor = 0.0;
    let result = complete_paid_jump(&f.db.store, ship.pilot.char_id, &mut ship, &arrival, &quote, Some(&guard)).await;
    assert!(result.unwrap_err().contains("online"));
    f.unchanged(&ship).await;
    generator_online(1);
    harmonics(0);
    assert!(complete_paid_jump(
        &f.db.store,
        ship.pilot.char_id,
        &mut ship,
        &arrival,
        &quote,
        Some(&guard)
    )
    .await
    .unwrap_err()
    .contains("harmonics"));
    f.unchanged(&ship).await;
    harmonics(2);
    // The bridge closes after the departure was captured.
    f.db.world.lock().bridges.remove(&hull);
    let result = complete_paid_jump(&f.db.store, ship.pilot.char_id, &mut ship, &arrival, &quote, Some(&guard)).await;
    assert!(result.unwrap_err().contains("portal changed"));
    f.unchanged(&ship).await;
    f.db.world.lock().bridges.insert(hull, bridge);
    complete_paid_jump(
        &f.db.store,
        ship.pilot.char_id,
        &mut ship,
        &arrival,
        &quote,
        Some(&guard),
    )
    .await
    .unwrap();
    arrived(&f, &ship, 1000.0);
}

#[tokio::test]
async fn array_departure_rechecks_remote_ownership_state_and_link_before_committing_payment() {
    let f = Fixture::new().await;
    let local = i64::from(f.db.id().await);
    let remote = i64::from(f.db.id().await);
    let stranger = i64::from(f.db.id().await);
    let ty = i64::from(crate::sde::get().types_in_group(707)[0]);
    insert(&f.db, "corporation", &[("corporationID", int(stranger)),
        ("corporationName", StoreValue::Text(format!("Unrelated array owner {stranger}").into()))]);
    for (id, system) in [(local, 30000142), (remote, 30000143)] {
        insert(&f.db, "entity", &[("itemID", int(id)), ("typeID", int(ty)), ("ownerID", int(f.corp)), ("locationID", int(system)),
            ("flag", int(0)), ("quantity", int(1)), ("singleton", int(1)),
            ("x", StoreValue::Float(f.ship.pos.x)), ("y", StoreValue::Float(f.ship.pos.y)), ("z", StoreValue::Float(f.ship.pos.z))]);
        insert(&f.db, "posStructures", &[("itemID", int(id)), ("state", int(4))]);
    }
    let link = || {
        insert(&f.db, "jumpBridgeLinks", &[("sourceID", int(local)), ("destinationID", int(remote))]);
    };
    link();
    insert(&f.db, "jumpBridgeLinks", &[("sourceID", int(remote)), ("destinationID", int(local))]);
    update(&f.db, "entity", &[int(f.fuel)], &[("ownerID", int(f.corp)), ("locationID", int(local)), ("flag", int(0))]);
    for system in [30000142, 30000143] {
        let structures = f.db.store.read(|db| crate::pos::load(db, system)).unwrap();
        let mut w = f.db.world.lock();
        let park = w.parks.get_mut(&system).unwrap();
        for structure in structures {
            park.structures.insert(structure.id, structure);
        }
    }
    let guard = f.capture(JumpRequest::Structure {
        local,
        remote,
    });
    // The remote array changes hands after the departure was captured.
    update(&f.db, "entity", &[int(remote)], &[("ownerID", int(stranger))]);
    let mut ship = f.ship.clone();
    let arrival = f.arrival();
    let mut quote = f.quote();
    quote.fuel.cargo = local;
    quote.fuel.flag = None;
    quote.capacitor = 0.0;
    let result = complete_paid_jump(&f.db.store, ship.pilot.char_id, &mut ship, &arrival, &quote, Some(&guard)).await;
    assert!(result.unwrap_err().contains("same corporation"));
    f.unchanged(&ship).await;
    update(&f.db, "entity", &[int(remote)], &[("ownerID", int(f.corp))]);
    update(&f.db, "posStructures", &[int(remote)], &[("state", int(1))]);
    assert!(complete_paid_jump(
        &f.db.store,
        ship.pilot.char_id,
        &mut ship,
        &arrival,
        &quote,
        Some(&guard)
    )
    .await
    .unwrap_err()
    .contains("online"));
    f.unchanged(&ship).await;
    update(&f.db, "posStructures", &[int(remote)], &[("state", int(4))]);
    f.db.store.write(|tx| tx.delete_where("jumpBridgeLinks", &["sourceID"], &[int(local)])).unwrap();
    assert!(complete_paid_jump(
        &f.db.store,
        ship.pilot.char_id,
        &mut ship,
        &arrival,
        &quote,
        Some(&guard)
    )
    .await
    .unwrap_err()
    .contains("link"));
    f.unchanged(&ship).await;
    link();
    complete_paid_jump(
        &f.db.store,
        ship.pilot.char_id,
        &mut ship,
        &arrival,
        &quote,
        Some(&guard),
    )
    .await
    .unwrap();
    arrived(&f, &ship, 1000.0);
}
