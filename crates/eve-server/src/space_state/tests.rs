use super::*;
use crate::{
    ballpark::{Command, Park, Vec3},
    session::Session,
    test_support::Database,
};

#[test]
fn old_motion_checkpoints_default_to_no_saved_cloak() {
    let pilot=crate::ballpark::Pilot {type_id:587,group_id:25,name:"Legacy".into(),char_id:7,corp_id:1,alliance_id:None,security:0.0,bounty:0.0};
    let ship=Ship::new(70,1,crate::ballpark::ShipStats::new(300.0,1e6,1.0,100.0,1.0),pilot,Vec3::ZERO,Vec3::ZERO);
    let old=serde_json::to_string(&ship.motion(1)).unwrap();
    let saved:Snapshot=serde_json::from_str(&old).unwrap();
    assert_eq!(saved.cloak,crate::cloak::SavedState::default());
    assert_eq!(saved.motion.tick,1);
    // Existing motion-only inspection tools can still read the new envelope.
    let new=serde_json::to_string(&saved).unwrap();
    assert_eq!(serde_json::from_str::<Motion>(&new).unwrap().tick,1);
}

async fn ship(f: &Database) -> (Session, Ship) {
    let pilot = f.id().await;
    let id = f.id().await;
    let system = 30000142i32;
    f.db.execute(r#"INSERT INTO "character_" ("characterID","characterName","shipID","solarSystemID","stationID") VALUES ($1,$2,$3,$4,0)"#,
        &[&pilot,&format!("Durable warp {pilot}"),&id,&system]).await.unwrap();
    f.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES ($1,587,$2,$3,0,1,1)"#,
        &[&id,&pilot,&system]).await.unwrap();
    let session = Session {
        char_id: Some(pilot as i64),
        ship_id: Some(id as i64),
        solar_system_id: Some(system as i64),
        ..Default::default()
    };
    let ship = inflight::ship_for_space(&f.ctx(&session), pilot as i64, id as i64)
        .await
        .unwrap();
    (session, ship)
}

#[tokio::test]
#[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
async fn paid_warp_restores_after_restart_and_relocation_invalidates_trajectory() {
    let f = Database::load().await;
    let (session, mut original) = ship(&f).await;
    let system = session.solar_system_id.unwrap();
    let id = original.id;
    let pilot = original.pilot.char_id;
    original.stats.warp_capacitor_need = 1.0e-7;
    let cap = &mut original.fight.as_mut().unwrap().vitals.capacitor;
    cap.recharge_ms = 0.0;
    cap.set(combat::now_ms(), 100.0);
    let end = destiny::tick_now();
    let mut live = Park::new(system, vec![], end - 30);
    live.ships.insert(id, original);
    live.command(
        id,
        Command::Warp {
            target: Vec3::new(5.0e12, 2.0e11, 0.0),
            range: 15_000.0,
        },
    );
    live.advance(end - 20);
    assert!(live.ships[&id].in_warp());
    let paid = live.ships[&id]
        .fight
        .as_ref()
        .unwrap()
        .vitals
        .capacitor
        .at(combat::now_ms());
    assert!(paid < 100.0 && paid > 0.0);
    let mut writer = crate::db::connect(&f.url).await.unwrap();
    assert!(
        save_detached(&mut writer, system, &live.ships[&id], live.tick)
            .await
            .unwrap()
    );
    let mut recovered = inflight::ship_for_space(&f.ctx(&session), pilot, id)
        .await
        .unwrap();
    recovered
        .fight
        .as_mut()
        .unwrap()
        .vitals
        .capacitor
        .recharge_ms = 0.0;
    assert!(restore(&f.ctx(&session), system, &mut recovered).await.unwrap());
    live.advance(destiny::tick_now());
    assert!((recovered.pos - live.ships[&id].pos).len() < 0.02);
    assert_eq!(recovered.warp_snapshot(), live.ships[&id].warp_snapshot());
    assert_eq!(
        recovered
            .fight
            .as_ref()
            .unwrap()
            .vitals
            .capacitor
            .at(combat::now_ms()),
        paid,
        "resumption never repays warp cost"
    );

    // A same-system relocation also invalidates the old native curve.
    let arrival = Vec3::new(4321.0, 5678.0, 9012.0);
    f.db.execute(
        r#"UPDATE entity SET x=$2,y=$3,z=$4 WHERE "itemID"=$1"#,
        &[&(id as i32), &arrival.x, &arrival.y, &arrival.z],
    )
    .await
    .unwrap();
    let mut moved = inflight::ship_for_space(&f.ctx(&session), pilot, id)
        .await
        .unwrap();
    assert!(!restore(&f.ctx(&session), system, &mut moved).await.unwrap());
    assert_eq!(moved.pos, arrival);
    assert!(!moved.warping());
    // A failed checkpoint rolls its new coordinates and trigger deletion back.
    let marker = Vec3::new(91.0, 92.0, 93.0);
    moved.arrive(marker);
    assert!(
        save_detached(&mut writer, system, &moved, destiny::tick_now())
            .await
            .unwrap()
    );
    f.db.batch_execute(&format!(r#"ALTER TABLE "shipSpaceMotion" ADD CONSTRAINT motion_test_{id} CHECK ("itemID"<>{id} OR "savedAt"<0) NOT VALID"#)).await.unwrap();
    moved.arrive(Vec3::new(600.0, 700.0, 800.0));
    let failed = save_detached(&mut writer, system, &moved, destiny::tick_now()).await;
    f.db.batch_execute(&format!(
        r#"ALTER TABLE "shipSpaceMotion" DROP CONSTRAINT motion_test_{id}"#
    ))
    .await
    .unwrap();
    assert!(failed.is_err());
    let row=f.db.query_one(r#"SELECT e.x,m.state FROM entity e JOIN "shipSpaceMotion" m ON m."itemID"=e."itemID" WHERE e."itemID"=$1"#,&[&(id as i32)]).await.unwrap();
    assert_eq!(row.get::<_, f64>(0), marker.x);
    let saved: Motion = serde_json::from_str(&row.get::<_, String>(1)).unwrap();
    assert!(moved.restore_motion(&saved, destiny::tick_now()));
    assert_eq!(moved.pos, marker);
}

#[tokio::test]
#[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
async fn queued_checkpoint_uses_fresh_world_state_after_inventory_lock() {
    let f = Database::load().await;
    let (session, original) = ship(&f).await;
    let system = session.solar_system_id.unwrap();
    let id = original.id;
    let pilot = original.pilot.char_id;
    let client = original.client_id;
    let mut park = Park::new(system, vec![], destiny::tick_now());
    park.ships.insert(id, original);
    f.world.lock().parks.insert(system, park);
    let mut locking = crate::db::connect(&f.url).await.unwrap();
    let tx = locking.transaction().await.unwrap();
    tx.query_one(
        r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#,
        &[&(pilot as i32)],
    )
    .await
    .unwrap();
    let mut waiting = crate::db::connect(&f.url).await.unwrap();
    let pid: i32 = waiting
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let run = checkpoint_one(&f.world, &mut waiting, system, id, pilot, client);
    let move_then_unlock = async {
        tokio::time::timeout(std::time::Duration::from_secs(10),async {
            loop {
                if f.db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock')",&[&pid]).await.unwrap().get::<_,bool>(0) {break;}
                tokio::task::yield_now().await;
            }
        }).await.expect("checkpoint queued behind character lock");
        let arrival = Vec3::new(1234.0, 5678.0, 9012.0);
        tx.execute(
            r#"UPDATE entity SET x=$2,y=$3,z=$4 WHERE "itemID"=$1"#,
            &[&(id as i32), &arrival.x, &arrival.y, &arrival.z],
        )
        .await
        .unwrap();
        f.world
            .lock()
            .parks
            .get_mut(&system)
            .unwrap()
            .ships
            .get_mut(&id)
            .unwrap()
            .arrive(arrival);
        tx.commit().await.unwrap();
        arrival
    };
    let (saved, arrival) = tokio::join!(run, move_then_unlock);
    assert!(saved.unwrap());
    let row =
        f.db.query_one(
            r#"SELECT x,y,z FROM entity WHERE "itemID"=$1"#,
            &[&(id as i32)],
        )
        .await
        .unwrap();
    assert_eq!(Vec3::new(row.get(0), row.get(1), row.get(2)), arrival);
    // An identity reservation excludes snapshots during cockpit transfer.
    f.world
        .lock()
        .parks
        .get_mut(&system)
        .unwrap()
        .ships
        .get_mut(&id)
        .unwrap()
        .identity_reserved = true;
    assert!(
        !checkpoint_one(&f.world, &mut waiting, system, id, pilot, client)
            .await
            .unwrap()
    );
    // A queued old client cannot overwrite a different connection's hull.
    let mut world = f.world.lock();
    let live = world
        .parks
        .get_mut(&system)
        .unwrap()
        .ships
        .get_mut(&id)
        .unwrap();
    live.identity_reserved = false;
    live.client_id += 1;
    drop(world);
    assert!(
        !checkpoint_one(&f.world, &mut waiting, system, id, pilot, client)
            .await
            .unwrap()
    );
}
