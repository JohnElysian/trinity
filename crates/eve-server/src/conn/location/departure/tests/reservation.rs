use super::*;
use crate::combat::{Active,Action,Lock};

fn arm(f:&Fixture,enemy:i64) {
    let module=|id,target| Active {module_id:id,type_id:3651,effect_id:4,guid:"effects.Test".into(),offensive:true,
        warp_safe:false,disallow_warp_alignment:false,action:Action::Passive,target,ammo:None,duration_ms:1000,
        cap_need:0.0,first_cycle_prepaid:false,started_ms:crate::combat::now_ms(),next_ms:i64::MAX,cycles:1,
        repeat:1000,stopping:false,name:"Regression weapon".into(),reach:None,landed:true};
    let mut w=f.db.world.lock();let p=w.parks.get_mut(&30000142).unwrap();
    let mut attacker=f.ship.clone();attacker.id=enemy;attacker.pilot.char_id=enemy;attacker.client_id=0;
    let offense=attacker.fight.as_mut().unwrap();offense.locks.insert(f.ship.id,Lock::Locked);offense.modules.push(module(enemy+1,Some(f.ship.id)));offense.applied.insert(enemy+1);
    p.ships.insert(enemy,attacker);
    let defense=p.ships.get_mut(&f.ship.id).unwrap().fight.as_mut().unwrap();defense.locks.insert(enemy,Lock::Locked);defense.modules.push(module(enemy+2,None));defense.applied.insert(enemy+2);
    p.missiles.push(crate::npc::Missile {attacker:crate::npc::combat_ref::Attacker {ship_id:enemy,char_id:enemy,client_id:None,ship_type:587,weapon_type:3651},
        target:f.ship.id,impact_ms:i64::MAX,damage:[1.0,0.0,0.0,0.0],cloud:100.0,aoe_velocity:100.0,reaches:true,weapon:"Regression missile".into()});
}

#[tokio::test]
async fn refused_reserved_jump_keeps_live_combat_and_refunds_only_the_reserved_capacitor() {
    let f=Fixture::new().await;let enemy=i64::from(f.db.id().await);arm(&f,enemy);
    // Damage and capacitor use before the departure stay; the fuel is short,
    // so the payment is refused after the jump's capacitor was reserved.
    update(&f.db,"entity",&[int(f.fuel)],&[("quantity",int(2))]);
    {
        let mut w=f.db.world.lock();let fighter=w.parks.get_mut(&30000142).unwrap().ships.get_mut(&f.ship.id).unwrap().fight.as_mut().unwrap();
        fighter.vitals.hull_damage=17.0;assert!(fighter.vitals.capacitor.spend(crate::combat::now_ms(),50.0));
    }
    let server=f.server().await;let arrival=f.arrival();let quote=f.quote();
    let result=super::super::reserved::run(&server,&f.session,1,JumpRequest::Beacon {member:f.member,beacon:f.beacon},&arrival,&quote,"effects.JumpDriveIn").await;
    assert!(result.unwrap_err().to_string().contains("fuel"));
    {
        let w=f.db.world.lock();let p=&w.parks[&30000142];let ship=&p.ships[&f.ship.id];let fight=ship.fight.as_ref().unwrap();
        assert!(!ship.identity_reserved);assert_eq!(fight.vitals.hull_damage,17.0);
        assert_eq!(fight.vitals.capacitor.at(crate::combat::now_ms()),950.0);
        assert_eq!(fight.modules.len(),1);assert_eq!(fight.locks.get(&enemy),Some(&Lock::Locked));
        assert_eq!(p.ships[&enemy].fight.as_ref().unwrap().locks.get(&f.ship.id),Some(&Lock::Locked));
        assert_eq!(p.ships[&enemy].fight.as_ref().unwrap().modules.len(),1);assert_eq!(p.missiles.len(),1);
        assert!(!w.parks[&30000143].ships.contains_key(&f.ship.id));
    }
    let (system,location,fuel,_)=f.saved();
    assert_eq!((system,location,fuel),(Some(30000142),Some(30000142),Some(2)));
}

#[tokio::test]
async fn accepted_jump_publishes_the_latest_live_combat_damage() {
    let f=Fixture::new().await;let enemy=i64::from(f.db.id().await);arm(&f,enemy);
    f.db.world.lock().parks.get_mut(&30000142).unwrap().ships.get_mut(&f.ship.id).unwrap().fight.as_mut().unwrap().vitals.hull_damage=19.0;
    let server=f.server().await;let arrival=f.arrival();let quote=f.quote();
    super::super::reserved::run(&server,&f.session,1,JumpRequest::Beacon {member:f.member,beacon:f.beacon},&arrival,&quote,"effects.JumpDriveIn").await.unwrap();
    {let w=f.db.world.lock();assert!(!w.parks[&30000142].ships.contains_key(&f.ship.id));
        assert!(w.pending_jobs.iter().any(|job|matches!(job,crate::heartbeat::Job::Refit {ship_id,..} if *ship_id==enemy)),"accepted removal retains other ships' modifier cleanup jobs");
        let ship=&w.parks[&30000143].ships[&f.ship.id];assert!(!ship.identity_reserved);assert_eq!(ship.fight.as_ref().unwrap().vitals.hull_damage,19.0);}
    let damage=f.db.store.read(|db|db.table("entity_attributes").get(&[int(f.ship.id),int(3)]).and_then(|a|a.float("valueFloat").or_else(||a.float("valueInt"))));
    assert_eq!(damage,Some(19.0),"destination checkpoint stores the latest accepted live snapshot");
}

#[tokio::test]
async fn lethal_damage_before_reserved_jump_payment_refuses_departure_without_erasing_combat() {
    let f=Fixture::new().await;let enemy=i64::from(f.db.id().await);arm(&f,enemy);
    {
        let mut w=f.db.world.lock();let ship=w.parks.get_mut(&30000142).unwrap().ships.get_mut(&f.ship.id).unwrap();
        let vitals=&mut ship.fight.as_mut().unwrap().vitals;vitals.hull_damage=vitals.hull_max;
    }
    let server=f.server().await;let arrival=f.arrival();let quote=f.quote();
    let result=super::super::reserved::run(&server,&f.session,1,JumpRequest::Beacon {member:f.member,beacon:f.beacon},&arrival,&quote,"effects.JumpDriveIn").await;
    assert!(result.unwrap_err().to_string().contains("destroyed"));
    {
        let w=f.db.world.lock();let p=&w.parks[&30000142];let ship=&p.ships[&f.ship.id];let fight=ship.fight.as_ref().unwrap();
        assert!(!ship.identity_reserved);assert_eq!(fight.vitals.hull_damage,fight.vitals.hull_max);
        assert_eq!(fight.vitals.capacitor.at(crate::combat::now_ms()),1000.0);
        assert_eq!(fight.modules.len(),1);assert_eq!(fight.locks.get(&enemy),Some(&Lock::Locked));
        assert_eq!(p.ships[&enemy].fight.as_ref().unwrap().locks.get(&f.ship.id),Some(&Lock::Locked));assert_eq!(p.missiles.len(),1);
        assert!(!w.parks[&30000143].ships.contains_key(&f.ship.id));
    }
    f.unchanged(&f.ship).await;
}
