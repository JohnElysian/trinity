//! Vectors produced by original 45017 x86 instructions, not a Rust formula copy.
use super::*;
use serde_json::Value;

fn scalar(value:&Value,key:&str,default:f64)->f64 {value[key].as_f64().unwrap_or(default)}
fn vector(value:&Value)->Vec3 {
    value.as_array().map_or(Vec3::ZERO,|a|Vec3::new(a[0].as_f64().unwrap(),a[1].as_f64().unwrap(),a[2].as_f64().unwrap()))
}
fn body(value:&Value)->Body {
    let mass=scalar(value,"mass",1100000.0);let free=value["free"].as_bool().unwrap_or(true);
    Body {id:value["id"].as_i64().unwrap_or(1),pos:vector(&value["pos"]),vel:vector(&value["vel"]),
        thrust:vector(&value["thrust"]),radius:scalar(value,"radius",37.0),
        inertia:if free {mass*scalar(value,"agility",3.1)}else{IMMOVABLE},mass,free,cloaked:false,key:(-1,-1,-1),field:None}
}
fn ship(value:&Value)->Ship {
    let stats=ShipStats::new(scalar(value,"radius",37.0),scalar(value,"mass",1100000.0),scalar(value,"agility",3.1),scalar(value,"max_velocity",320.0),1.0);
    let mut result=Ship::new(value["id"].as_i64().unwrap_or(1),1,stats,Pilot {type_id:587,group_id:25,name:"Native vector".into(),char_id:1,corp_id:1,alliance_id:None,security:0.0,bounty:0.0},vector(&value["pos"]),vector(&value["vel"]));
    result.speed_fraction=scalar(value,"speed_fraction",1.0);result
}
fn close(actual:Vec3,expected:Vec3,name:&str) {
    let tolerance=1e-8*expected.len().max(1.0);
    assert!((actual-expected).len()<=tolerance,"{name}: actual {actual:?}; native {expected:?}");
}
fn vectors()->Value {
    let value:Value=serde_json::from_str(include_str!("../../../../compatibility/destiny-45017/native-movement-vectors.json")).unwrap();
    assert_eq!(value["x87_precision_bits"],53,"oracle must use original client startup precision");
    assert_eq!(value["x87_control_word"],"0x023f");value
}

#[test]
fn steering_matches_original_45017_instruction_vectors() {
    let cases=vectors();
    for case in cases["follow"].as_array().unwrap() {
        let ship=ship(&case["a"]);let target=body(&case["b"]);
        let goal=follow_point(&ship,target.pos,ship.stats.radius+target.radius+scalar(&case["a"],"range",0.0));
        close(goal,vector(&case["goal"]),case["name"].as_str().unwrap());
        close(steer(&ship,goal),vector(&case["thrust"]),case["name"].as_str().unwrap());
    }
    for case in cases["orbit"].as_array().unwrap() {
        let mut ship=ship(&case["a"]);let target=body(&case["b"]);
        let reach=ship.stats.radius+target.radius+scalar(&case["a"],"range",0.0);
        let thrust=orbit(&mut ship,target.pos,reach,case["stamp"].as_i64().unwrap() as i32);
        close(thrust,vector(&case["thrust"]),case["name"].as_str().unwrap());
        close(ship.goal,vector(&case["goal"]),case["name"].as_str().unwrap());
    }
}

#[test]
fn orbit_target_removal_matches_original_45017_release_vectors() {
    for case in vectors()["orbit_release"].as_array().unwrap() {
        let mut ship=ship(&case["a"]);ship.command=Command::Orbit {target:2,range:500.0};
        let mut park=Park::new(1,vec![],0);park.ships.insert(ship.id,ship);
        let forces=park.thrusts(0);let ship=park.ships.values().next().unwrap();
        close(ship.goal,vector(&case["goal"]),case["name"].as_str().unwrap());
        assert!(matches!(ship.command,Command::Direction(_)));assert_eq!(case["mode"].as_i64(),Some(0));
        assert_eq!(ship.speed_fraction,case["speed_fraction"].as_f64().unwrap());
        close(forces[&ship.id],steer(ship,ship.goal),"first tick after native RemoveBall");
        park.advance(1);let ship=park.ships.values().next().unwrap();
        close(ship.pos,vector(&case["next_position"]),"native Evolve position after removal");
        close(ship.vel,vector(&case["next_velocity"]),"native Evolve velocity after removal");
    }
}

#[test]
fn orbit_warp_release_reopens_only_zero_throttle_and_followers_stop() {
    for fraction in [0.0,0.25] {
        let mut park=Park::new(1,vec![],0);
        let mut orbiter=ship(&serde_json::json!({"id":1,"speed_fraction":fraction,"vel":[12.0,-8.0,3.0]}));
        orbiter.command=Command::Orbit {target:2,range:500.0};park.ships.insert(1,orbiter);
        let mut follower=ship(&serde_json::json!({"id":3,"speed_fraction":fraction}));
        follower.command=Command::Follow {target:2,range:500.0};park.ships.insert(3,follower);
        park.let_go_of(2);
        assert!(matches!(park.ships[&1].command,Command::Direction(_)));
        assert_eq!(park.ships[&1].speed_fraction,if fraction==0.0 {1.0}else{fraction});
        assert_eq!(park.ships[&3].command,Command::Stop);assert_eq!(park.ships[&3].speed_fraction,fraction);
    }
}

#[test]
fn collisions_match_original_45017_instruction_vectors() {
    for case in vectors()["collision"].as_array().unwrap() {
        let name=case["name"].as_str().unwrap();let expected=case["contact"].as_f64().unwrap();
        let result=bounce(&body(&case["a"]),vector(&case["a"]["prev_vel"]),&body(&case["b"]));
        if expected<0.0 {assert!(result.is_none(),"{name}");continue;}
        let (contact,force)=result.expect(name);
        assert!((contact-expected).abs()<1e-9,"{name}: contact {contact} versus native {expected}");
        close(force*BOUNCE,vector(&case["correction"]),name);
    }
}

#[test]
fn candidate_order_matches_original_constructor_insertion_and_bucket_splits() {
    for case in vectors()["candidate_order"].as_array().unwrap() {
        let ids=case["ids"].as_array().unwrap().iter().map(|id|id.as_i64().unwrap());
        let expected:Vec<_>=case["order"].as_array().unwrap().iter().map(|id|id.as_i64().unwrap()).collect();
        assert_eq!(candidates::order(ids),expected,"{}",case["name"]);
    }
}

#[test]
fn partition_graph_matches_original_membership_query_and_pointer_layouts() {
    for case in vectors()["partition"].as_array().unwrap() {
        let mut index=partition::Index::default();
        let key=|value:&Value| partition::Key {level:value["level"].as_u64().unwrap() as u32,
            at:std::array::from_fn(|axis|value["coordinates"][axis].as_i64().unwrap())};
        index.cell_tokens=case["allocations"].as_array().unwrap().iter().map(|v|(key(v),v["token"].as_u64().unwrap() as u32)).collect();
        let balls:BTreeMap<_,_>=case["balls"].as_array().unwrap().iter().map(|v|(v["id"].as_i64().unwrap(),v)).collect();
        index.ball_tokens=balls.iter().map(|(id,v)|(*id,v["token"].as_u64().unwrap() as u32)).collect();
        for id in case["registration"].as_array().unwrap() {
            let id=id.as_i64().unwrap();let value=balls[&id];
            let ball=partition::Ball {id,pos:vector(&value["pos"]),vel:vector(&value["vel"]),radius:scalar(value,"radius",37.0)};
            assert_eq!(partition::membership(ball).0,key(&case["memberships"][id.to_string()]["primary"]),"{} ball{id}",case["name"]);
            index.update(ball);
        }
        assert!(index.cell_tokens.is_empty(),"{}: native cell allocations unused",case["name"]);
        for (id,query) in case["queries"].as_object().unwrap() {
            let actual=index.candidates(id.parse().unwrap(),|other| {
                let value=balls[&other];value["massive"].as_bool().unwrap_or(true) && !value["cloaked"].as_bool().unwrap_or(false)
            });
            let expected:Vec<_>=query["order"].as_array().unwrap().iter().map(|v|v.as_i64().unwrap()).collect();
            assert_eq!(actual,expected,"{} query{id}",case["name"]);
        }
    }
}

#[test]
fn sequential_collision_dispatch_integration_and_next_ticks_match_original() {
    let cases=vectors();
    for case in cases["multi_collision_ticks"].as_array().unwrap().iter().chain(std::iter::once(&cases["precision_boundary"])) {
        let name=case["name"].as_str().unwrap();
        let mut park=Park::new(1,vec![],0);
        let key=|value:&Value| partition::Key {level:value["level"].as_u64().unwrap() as u32,
            at:std::array::from_fn(|axis|value["coordinates"][axis].as_i64().unwrap())};
        park.partition.cell_tokens=case["allocations"].as_array().unwrap().iter()
            .map(|v|(key(v),v["token"].as_u64().unwrap() as u32)).collect();
        let balls:BTreeMap<_,_>=case["balls"].as_array().unwrap().iter().map(|v|(v["id"].as_i64().unwrap(),v)).collect();
        park.partition.ball_tokens=balls.iter().map(|(id,v)|(*id,v["token"].as_u64().unwrap() as u32)).collect();
        for id in case["registration"].as_array().unwrap() {
            let id=id.as_i64().unwrap();let value=balls[&id];
            park.partition.update(partition::Ball {id,pos:vector(&value["pos"]),vel:vector(&value["vel"]),radius:scalar(value,"radius",37.0)});
            if value["free"].as_bool().unwrap_or(true) {
                let mut ship=ship(value);ship.prev_vel=vector(&value["prev_vel"]);park.ships.insert(id,ship);
            } else if value["kind"].as_str()==Some("rock") {
                park.rocks.insert(id,Rock {id,type_id:1230,name:"Native rock".into(),pos:vector(&value["pos"]),
                    radius:scalar(value,"radius",37.0),quantity:1,volume:1.0});
            } else {
                park.celestials.push(Celestial {id,type_id:16,group_id:10,name:"Native gate".into(),
                    pos:vector(&value["pos"]),radius:scalar(value,"radius",37.0),jump:None});
            }
        }
        for (tick,step) in case["steps"].as_array().unwrap().iter().enumerate() {
            park.advance(tick as i64+1);
            for (id,expected) in step.as_object().unwrap() {
                let ship=&park.ships[&id.parse::<i64>().unwrap()];
                close(ship.pos,vector(&expected["pos"]),&format!("{name} tick{tick} position"));
                close(ship.vel,vector(&expected["vel"]),&format!("{name} tick{tick} velocity"));
                close(ship.prev_vel,vector(&expected["prev_vel"]),&format!("{name} tick{tick} previous velocity"));
            }
        }
        assert!(park.partition.cell_tokens.is_empty(),"{name}: native cell allocations unused");
    }
}

#[test]
fn near_equal_nonaxis_forces_use_the_calibrated_client_precision_winner() {
    let cases=vectors();let case=&cases["precision_boundary"];let values=case["balls"].as_array().unwrap();
    let mover=body(&values[0]);let mut winner=None;let mut force=Vec3::ZERO;
    for value in &values[1..] {
        let (contact,next)=bounce(&mover,Vec3::ZERO,&body(value)).unwrap();assert_eq!(contact,0.0);
        let next=next*BOUNCE;
        if winner.is_none() || next.len_sq()>force.len_sq() {winner=Some(value["id"].as_i64().unwrap());force=next;}
    }
    assert_eq!(winner,Some(50_000_003));
    assert_eq!(force,vector(&case["steps"][0]["100"]["correction"]));
}
