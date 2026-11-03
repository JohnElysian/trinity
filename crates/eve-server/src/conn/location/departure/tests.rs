use super::*;
mod reservation;
mod commit_recovery;
use crate::{
    ballpark::{Park, Vec3},
    beacons::{Beacon, Kind},
    session::Session,
    test_support::Database,
};

struct Fixture {
    db: Database,
    session: Session,
    ship: Ship,
    fuel: i32,
    beacon: i64,
    member: i64,
    corp: i32,
}
impl Fixture {
    async fn server(&self)->crate::conn::Server {
        let db=crate::db::connect(&self.db.url).await.unwrap();
        let dogma=crate::dogma::Catalog::load(&db).await.unwrap();
        crate::conn::Server {table:Default::default(),db:crate::db::Shared::new(db,self.db.url.clone()),db_url:self.db.url.clone(),
            node_id:1,bulk:crate::bulkdata::BulkData {entries:Default::default(),node_id:1},dogma,
            world:self.db.world.clone(),services:crate::svc::registry(),dump_dir:Default::default(),alliance_join_delay_secs:0,sov_claim_delay_secs:0}
    }
    async fn new() -> Self {
        let db = Database::load().await;
        let pilot = db.id().await;
        let hull = db.id().await;
        let fuel = db.id().await;
        let corp = db.id().await;
        let beacon = db.id().await;
        let member = db.id().await;
        let alliance = db.id().await;
        db.db.execute(r#"INSERT INTO corporation ("corporationID","corporationName","allianceID") VALUES ($1,$2,$3)"#,&[&corp,&format!("Departure corp {corp}"),&alliance]).await.unwrap();
        db.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","corporationID","shipID","solarSystemID","stationID")
            VALUES ($1,$2,$3,$4,30000142,0)"#,&[&pilot,&format!("Departure pilot {pilot}"),&corp,&hull]).await.unwrap();
        for (id, ty, location, quantity, flag, singleton) in
            [(hull, 587, 30000142, 1, 0, 1), (fuel, 16273, hull, 8, 5, 0)]
        {
            db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",quantity,flag,singleton) VALUES ($1,$2,$3,$4,$5,$6,$7)"#,
                &[&id,&ty,&pilot,&location,&quantity,&flag,&singleton]).await.unwrap();
        }
        let session = Session {
            char_id: Some(pilot as i64),
            ship_id: Some(hull as i64),
            corp_id: Some(corp as i64),
            alliance_id: Some(alliance as i64),
            solar_system_id: Some(30000142),
            ..Default::default()
        };
        let mut ship = inflight::ship_for_space(&db.ctx(&session), pilot as i64, hull as i64)
            .await
            .unwrap();
        ship.fight.as_mut().unwrap().vitals.capacitor =
            crate::combat::Charged::full(1000.0, 0.0, crate::combat::now_ms());
        let mut source = Park::new(30000142, vec![], destiny::tick_now());
        source.ships.insert(hull as i64, ship.clone());
        let mut dest = Park::new(30000143, vec![], destiny::tick_now());
        dest.beacons.insert(
            beacon as i64,
            Beacon::new(
                beacon as i64,
                21094,
                Some(member as i64),
                "Test field",
                Vec3::ZERO,
                1000.0,
                Kind::Cyno {
                    char_id: member as i64,
                    ship_id: member as i64 + 1,
                },
            ),
        );
        {
            let mut w = db.world.lock();
            w.parks.insert(30000142, source);
            w.parks.insert(30000143, dest);
            w.gangs.test_members(vec![pilot as i64, member as i64]);
        }
        Self {
            db,
            session,
            ship,
            fuel,
            beacon: beacon as i64,
            member: member as i64,
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
    async fn unchanged(&self, ship: &Ship) {
        let row=self.db.db.query_one(r#"SELECT c."solarSystemID",e."locationID",f.quantity FROM "character_" c
            JOIN entity e ON e."itemID"=c."shipID" JOIN entity f ON f."itemID"=$2 WHERE c."characterID"=$1"#,
            &[&(self.ship.pilot.char_id as i32),&self.fuel]).await.unwrap();
        assert_eq!(
            (
                row.get::<_, i32>(0),
                row.get::<_, i32>(1),
                row.get::<_, i32>(2)
            ),
            (30000142, 30000142, 8)
        );
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
#[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
async fn field_extinguishing_during_payment_wait_rolls_back_location_fuel_and_capacitor() {
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
    let blocker = crate::db::connect(&f.db.url).await.unwrap();
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker
        .query_one(
            r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,
            &[&f.fuel],
        )
        .await
        .unwrap();
    let pid = blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    let mut ship = f.ship.clone();
    let arrival = f.arrival();
    let quote = f.quote();
    let (result, ()) = tokio::join!(
        complete_paid_jump(
            &f.db.url,
            ship.pilot.char_id,
            &mut ship,
            &arrival,
            &quote,
            Some(&guard)
        ),
        async {
            tokio::time::timeout(Duration::from_secs(10),async {loop {
            if f.db.db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",&[&pid]).await.unwrap().get::<_,bool>(0) {break;}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }}).await.unwrap();
            f.db.world
                .lock()
                .parks
                .get_mut(&30000143)
                .unwrap()
                .beacons
                .remove(&f.beacon);
            blocker.batch_execute("COMMIT").await.unwrap();
        }
    );
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
#[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
async fn fixed_beacon_state_and_alliance_are_locked_and_rechecked_before_payment() {
    let f = Fixture::new().await;
    let ty =
        f.db.db
            .query_one(
                r#"SELECT "typeID" FROM "invTypes" WHERE "groupID"=838 ORDER BY "typeID" LIMIT 1"#,
                &[],
            )
            .await
            .unwrap()
            .get::<_, i32>(0);
    f.db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",quantity,flag,singleton) VALUES ($1,$2,$3,30000143,1,0,1)"#,&[&(f.beacon as i32),&ty,&f.corp]).await.unwrap();
    f.db.db
        .execute(
            r#"INSERT INTO "posStructures" ("itemID",state) VALUES ($1,1)"#,
            &[&(f.beacon as i32)],
        )
        .await
        .unwrap();
    let guard = f.capture(JumpRequest::Alliance { beacon: f.beacon });
    let mut ship = f.ship.clone();
    assert!(complete_paid_jump(
        &f.db.url,
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
    f.db.db
        .execute(
            r#"UPDATE "posStructures" SET state=4 WHERE "itemID"=$1"#,
            &[&(f.beacon as i32)],
        )
        .await
        .unwrap();
    f.db.db
        .execute(
            r#"UPDATE corporation SET "allianceID"=0 WHERE "corporationID"=$1"#,
            &[&f.corp],
        )
        .await
        .unwrap();
    assert!(complete_paid_jump(
        &f.db.url,
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
    f.db.db
        .execute(
            r#"UPDATE corporation SET "allianceID"=$2 WHERE "corporationID"=$1"#,
            &[&f.corp, &(f.session.alliance_id.unwrap() as i32)],
        )
        .await
        .unwrap();
    complete_paid_jump(
        &f.db.url,
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
    let fuel =
        f.db.db
            .query_one(
                r#"SELECT quantity FROM entity WHERE "itemID"=$1"#,
                &[&f.fuel],
            )
            .await
            .unwrap()
            .get::<_, i32>(0);
    assert_eq!(fuel, 5);
}

async fn wait_for_lock(db: &tokio_postgres::Client, pid: i32) {
    tokio::time::timeout(Duration::from_secs(15),async {loop {
        db.query_one("SELECT pg_stat_clear_snapshot()",&[]).await.unwrap();
        if db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",&[&pid]).await.unwrap().get::<_,bool>(0) {break;}
        tokio::time::sleep(Duration::from_millis(10)).await;
    }}).await.expect("departure reaches the held endpoint or payment lock");
}

async fn arrived(f: &Fixture, ship: &Ship, capacitor: f64) {
    let row=f.db.db.query_one(r#"SELECT c."solarSystemID",e."locationID",e.x,e.y,e.z,f.quantity
        FROM "character_" c JOIN entity e ON e."itemID"=c."shipID" JOIN entity f ON f."itemID"=$2 WHERE c."characterID"=$1"#,
        &[&(f.ship.pilot.char_id as i32),&f.fuel]).await.unwrap();
    assert_eq!(
        (
            row.get::<_, i32>(0),
            row.get::<_, i32>(1),
            row.get::<_, i32>(5)
        ),
        (30000143, 30000143, 5)
    );
    assert_eq!(
        Vec3::new(row.get(2), row.get(3), row.get(4)),
        f.arrival().at
    );
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
#[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
async fn portal_departure_rechecks_generator_harmonics_and_live_bridge_before_committing_payment() {
    let f = Fixture::new().await;
    let host = f.db.id().await;
    let hull = f.db.id().await;
    let generator = f.db.id().await;
    f.db.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","corporationID","shipID","solarSystemID","stationID")
        VALUES ($1,$2,$3,$4,30000142,0)"#,&[&host,&format!("Portal host {host}"),&f.corp,&hull]).await.unwrap();
    for (id, ty, location, flag) in [(hull, 22428, 30000142, 0), (generator, 28652, hull, 27)] {
        f.db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES ($1,$2,$3,$4,$5,1,1)"#,
            &[&id,&ty,&host,&location,&flag]).await.unwrap();
    }
    f.db.db.execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt") VALUES ($1,2,1),($2,1253,2)"#,
        &[&generator,&(f.ship.id as i32)]).await.unwrap();
    f.db.db.execute(r#"UPDATE entity SET "ownerID"=$2,"locationID"=$3 WHERE "itemID"=$1"#,
        &[&f.fuel,&host,&hull]).await.unwrap();
    let host_session = Session {
        char_id: Some(host as i64),
        ship_id: Some(hull as i64),
        ..f.session.clone()
    };
    let mut host_ship =
        inflight::ship_for_space(&f.db.ctx(&host_session), host as i64, hull as i64)
            .await
            .unwrap();
    host_ship.pos = f.ship.pos;
    let bridge = Bridge {
        generation: 0, started_ms: 0,
        titan_pilot: host as i64,
        generator_id: generator as i64,
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
            .insert(hull as i64, host_ship);
        w.bridges.insert(hull as i64, bridge.clone());
        w.gangs=Default::default();
        w.gangs
            .test_members(vec![f.ship.pilot.char_id, host as i64, f.member]);
    }
    let guard = f.capture(JumpRequest::Portal {
        pilot: host as i64,
        ship: hull as i64,
        alliance: false,
    });
    let blocker = crate::db::connect(&f.db.url).await.unwrap();
    let pid = blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker
        .execute(
            r#"UPDATE entity_attributes SET "valueInt"=0 WHERE "itemID"=$1 AND "attributeID"=2"#,
            &[&generator],
        )
        .await
        .unwrap();
    let mut ship = f.ship.clone();
    let arrival = f.arrival();
    let mut quote = f.quote();
    quote.fuel.cargo = hull as i64;
    quote.capacitor = 0.0;
    let (result, ()) = tokio::join!(
        complete_paid_jump(
            &f.db.url,
            ship.pilot.char_id,
            &mut ship,
            &arrival,
            &quote,
            Some(&guard)
        ),
        async {
            wait_for_lock(&blocker, pid).await;
            blocker.batch_execute("COMMIT").await.unwrap();
        }
    );
    assert!(result.unwrap_err().contains("online"));
    f.unchanged(&ship).await;
    f.db.db
        .execute(
            r#"UPDATE entity_attributes SET "valueInt"=1 WHERE "itemID"=$1 AND "attributeID"=2"#,
            &[&generator],
        )
        .await
        .unwrap();
    f.db.db
        .execute(
            r#"UPDATE entity_attributes SET "valueInt"=0 WHERE "itemID"=$1 AND "attributeID"=1253"#,
            &[&(ship.id as i32)],
        )
        .await
        .unwrap();
    assert!(complete_paid_jump(
        &f.db.url,
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
    f.db.db
        .execute(
            r#"UPDATE entity_attributes SET "valueInt"=2 WHERE "itemID"=$1 AND "attributeID"=1253"#,
            &[&(ship.id as i32)],
        )
        .await
        .unwrap();
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker
        .query_one(
            r#"SELECT 1 FROM entity WHERE "itemID"=$1 FOR UPDATE"#,
            &[&f.fuel],
        )
        .await
        .unwrap();
    let (result, ()) = tokio::join!(
        complete_paid_jump(
            &f.db.url,
            ship.pilot.char_id,
            &mut ship,
            &arrival,
            &quote,
            Some(&guard)
        ),
        async {
            wait_for_lock(&blocker, pid).await;
            f.db.world.lock().bridges.remove(&(hull as i64));
            blocker.batch_execute("COMMIT").await.unwrap();
        }
    );
    assert!(result.unwrap_err().contains("portal changed"));
    f.unchanged(&ship).await;
    f.db.world.lock().bridges.insert(hull as i64, bridge);
    complete_paid_jump(
        &f.db.url,
        ship.pilot.char_id,
        &mut ship,
        &arrival,
        &quote,
        Some(&guard),
    )
    .await
    .unwrap();
    arrived(&f, &ship, 1000.0).await;
}

#[tokio::test]
#[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
async fn array_departure_rechecks_remote_ownership_state_and_link_before_committing_payment() {
    let f = Fixture::new().await;
    let local = f.db.id().await;
    let remote = f.db.id().await;
    let stranger = f.db.id().await;
    let ty =
        f.db.db
            .query_one(
                r#"SELECT "typeID" FROM "invTypes" WHERE "groupID"=707 ORDER BY "typeID" LIMIT 1"#,
                &[],
            )
            .await
            .unwrap()
            .get::<_, i32>(0);
    f.db.db
        .execute(
            r#"INSERT INTO corporation ("corporationID","corporationName") VALUES ($1,$2)"#,
            &[&stranger, &format!("Unrelated array owner {stranger}")],
        )
        .await
        .unwrap();
    for (id, system) in [(local, 30000142), (remote, 30000143)] {
        f.db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton,x,y,z) VALUES ($1,$2,$3,$4,0,1,1,$5,$6,$7)"#,
            &[&id,&ty,&f.corp,&system,&f.ship.pos.x,&f.ship.pos.y,&f.ship.pos.z]).await.unwrap();
        f.db.db
            .execute(
                r#"INSERT INTO "posStructures" ("itemID",state) VALUES ($1,4)"#,
                &[&id],
            )
            .await
            .unwrap();
    }
    f.db.db
        .execute(
            r#"INSERT INTO "jumpBridgeLinks" ("sourceID","destinationID") VALUES ($1,$2),($2,$1)"#,
            &[&local, &remote],
        )
        .await
        .unwrap();
    f.db.db.execute(r#"UPDATE entity SET "ownerID"=$2,"locationID"=$3,flag=0 WHERE "itemID"=$1"#,
        &[&f.fuel,&f.corp,&local]).await.unwrap();
    for system in [30000142, 30000143] {
        let structures = crate::pos::load(&f.db.db, system).await.unwrap();
        let mut w = f.db.world.lock();
        let park = w.parks.get_mut(&system).unwrap();
        for structure in structures {
            park.structures.insert(structure.id, structure);
        }
    }
    let guard = f.capture(JumpRequest::Structure {
        local: local as i64,
        remote: remote as i64,
    });
    let blocker = crate::db::connect(&f.db.url).await.unwrap();
    let pid = blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get::<_, i32>(0);
    blocker.batch_execute("BEGIN").await.unwrap();
    blocker
        .execute(
            r#"UPDATE entity SET "ownerID"=$2 WHERE "itemID"=$1"#,
            &[&remote, &stranger],
        )
        .await
        .unwrap();
    let mut ship = f.ship.clone();
    let arrival = f.arrival();
    let mut quote = f.quote();
    quote.fuel.cargo = local as i64;
    quote.fuel.flag = None;
    quote.capacitor = 0.0;
    let (result, ()) = tokio::join!(
        complete_paid_jump(
            &f.db.url,
            ship.pilot.char_id,
            &mut ship,
            &arrival,
            &quote,
            Some(&guard)
        ),
        async {
            wait_for_lock(&blocker, pid).await;
            blocker.batch_execute("COMMIT").await.unwrap();
        }
    );
    assert!(result.unwrap_err().contains("same corporation"));
    f.unchanged(&ship).await;
    f.db.db
        .execute(
            r#"UPDATE entity SET "ownerID"=$2 WHERE "itemID"=$1"#,
            &[&remote, &f.corp],
        )
        .await
        .unwrap();
    f.db.db
        .execute(
            r#"UPDATE "posStructures" SET state=1 WHERE "itemID"=$1"#,
            &[&remote],
        )
        .await
        .unwrap();
    assert!(complete_paid_jump(
        &f.db.url,
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
    f.db.db
        .execute(
            r#"UPDATE "posStructures" SET state=4 WHERE "itemID"=$1"#,
            &[&remote],
        )
        .await
        .unwrap();
    f.db.db
        .execute(
            r#"DELETE FROM "jumpBridgeLinks" WHERE "sourceID"=$1"#,
            &[&local],
        )
        .await
        .unwrap();
    assert!(complete_paid_jump(
        &f.db.url,
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
    f.db.db
        .execute(
            r#"INSERT INTO "jumpBridgeLinks" ("sourceID","destinationID") VALUES ($1,$2)"#,
            &[&local, &remote],
        )
        .await
        .unwrap();
    complete_paid_jump(
        &f.db.url,
        ship.pilot.char_id,
        &mut ship,
        &arrival,
        &quote,
        Some(&guard),
    )
    .await
    .unwrap();
    arrived(&f, &ship, 1000.0).await;
}
