use super::*;
use crate::{
    ballpark::{Command, Park, Vec3},
    session::Session,
    test_support::Database,
};

#[test]
fn motion_checkpoint_json_preserves_native_coordinates_bit_for_bit() {
    let pilot=crate::ballpark::Pilot {type_id:587,group_id:25,name:"Exact motion".into(),char_id:7,corp_id:1,alliance_id:None,security:0.0,bounty:0.0};
    let mut original=Ship::new(70,1,crate::ballpark::ShipStats::new(300.0,1e6,1.0,100.0,1.0),pilot,
        Vec3::new(127.51127349558429,1.2345678901234567e16,-1.2345678901234567e16),Vec3::new(0.12345678901234567,-0.0,1e-300));
    let mut fields=serde_json::to_value(original.motion(1)).unwrap();
    fields["prev_vel"]=serde_json::to_value(Vec3::new(-3.9633091422683018,-26.083010907094472,0.0)).unwrap();
    fields["goal"]=serde_json::to_value(Vec3::new(157.91809379952542,152.59168855107683,120.0)).unwrap();
    let motion=serde_json::from_value(fields.clone()).unwrap();
    assert!(original.restore_motion(&motion,1));
    let raw=serde_json::to_string(&Snapshot{motion:original.motion(1),cloak:Default::default()}).unwrap();
    let saved:Snapshot=serde_json::from_str(&raw).unwrap();
    let mut restored=original.clone();restored.pos=Vec3::ZERO;restored.vel=Vec3::ZERO;
    assert!(restored.restore_motion(&saved.motion,1));
    let after=serde_json::to_value(restored.motion(1)).unwrap();
    for name in ["pos","vel","prev_vel","goal"] {
        let before:Vec3=serde_json::from_value(fields[name].clone()).unwrap();
        let after:Vec3=serde_json::from_value(after[name].clone()).unwrap();
        assert_eq!([before.x.to_bits(),before.y.to_bits(),before.z.to_bits()],[after.x.to_bits(),after.y.to_bits(),after.z.to_bits()],"{name}");
    }
}

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
    let pilot = i64::from(f.id().await);
    let id = i64::from(f.id().await);
    let system = 30000142;
    f.store.write(|tx| {
        tx.insert("character_", &[("characterID", Value::Int(pilot)), ("characterName", Value::Text(format!("Durable warp {pilot}").into())),
            ("shipID", Value::Int(id)), ("solarSystemID", Value::Int(system)), ("stationID", Value::Int(0))])?;
        tx.insert("entity", &[("itemID", Value::Int(id)), ("typeID", Value::Int(587)), ("ownerID", Value::Int(pilot)), ("locationID", Value::Int(system)),
            ("flag", Value::Int(0)), ("quantity", Value::Int(1)), ("singleton", Value::Int(1))])?;
        Ok::<_, StoreError>(())
    }).unwrap();
    let session = Session {
        char_id: Some(pilot),
        ship_id: Some(id),
        solar_system_id: Some(system),
        ..Default::default()
    };
    let ship = inflight::ship_for_space(&f.ctx(&session), pilot, id)
        .await
        .unwrap();
    (session, ship)
}

fn place(f: &Database, id: i64, at: Vec3) {
    f.store.write(|tx| {
        let row = tx.table("entity").get_id(id).unwrap().id;
        tx.update("entity", row, &[("x", Value::Float(at.x)), ("y", Value::Float(at.y)), ("z", Value::Float(at.z))])
    }).unwrap();
}

fn stored_position(f: &Database, id: i64) -> Vec3 {
    f.store.read(|db| {
        let e = db.table("entity").get_id(id).unwrap();
        Vec3::new(e.float("x").unwrap(), e.float("y").unwrap(), e.float("z").unwrap())
    })
}

#[tokio::test]
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
    assert!(
        save_detached(&f.store, system, &live.ships[&id], live.tick)
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
    place(&f, id, arrival);
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
        save_detached(&f.store, system, &moved, destiny::tick_now())
            .await
            .unwrap()
    );
    moved.arrive(Vec3::new(600.0, 700.0, 800.0));
    let failed: Result<(), StoreError> = f.store.write(|tx| {
        assert!(write(tx, system, &moved, destiny::tick_now())?);
        Err(StoreError { code: crate::store::CHECK_VIOLATION, message: "the checkpoint's write fails after its updates".into() })
    });
    assert!(failed.is_err());
    assert_eq!(stored_position(&f, id).x, marker.x);
    let state = f.store.read(|db| db.table("shipSpaceMotion").get_id(id).unwrap().text("state").unwrap().to_string());
    let saved: Motion = serde_json::from_str(&state).unwrap();
    assert!(moved.restore_motion(&saved, destiny::tick_now()));
    assert_eq!(moved.pos, marker);
}

#[tokio::test]
async fn checkpoint_saves_the_world_state_current_when_it_runs() {
    let f = Database::load().await;
    let (session, original) = ship(&f).await;
    let system = session.solar_system_id.unwrap();
    let id = original.id;
    let pilot = original.pilot.char_id;
    let client = original.client_id;
    let mut park = Park::new(system, vec![], destiny::tick_now());
    park.ships.insert(id, original);
    f.world.lock().parks.insert(system, park);
    // A move committed before the checkpoint runs is what it saves.
    let arrival = Vec3::new(1234.0, 5678.0, 9012.0);
    place(&f, id, arrival);
    f.world
        .lock()
        .parks
        .get_mut(&system)
        .unwrap()
        .ships
        .get_mut(&id)
        .unwrap()
        .arrive(arrival);
    assert!(checkpoint_one(&f.world, &f.store, system, id, pilot, client).await.unwrap());
    assert_eq!(stored_position(&f, id), arrival);
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
        !checkpoint_one(&f.world, &f.store, system, id, pilot, client)
            .await
            .unwrap()
    );
    // An old client cannot overwrite a different connection's hull.
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
        !checkpoint_one(&f.world, &f.store, system, id, pilot, client)
            .await
            .unwrap()
    );
}
