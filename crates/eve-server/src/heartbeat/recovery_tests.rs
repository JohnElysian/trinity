use super::*;

#[test]
fn confirmed_loss_blocks_relogin_before_its_database_job_is_dispatched() {
    let world=World::default();
    let loss=crate::losses::Loss {client_id:10,char_id:100,ship_id:101,ship_type:587,system:30_000_001,
        pos:Vec3::ZERO,arrival_custody:None,capsule:false,unpiloted:false,final_blow:None,attackers:vec![],items:vec![]};
    world.lock().pending_jobs.push(Job::Lost(Box::new(loss)));
    let jobs=pulse(&world,combat::now_ms());
    assert!(matches!(jobs.as_slice(),[Job::Lost(_)]),"the loss job remains undispatched");
    assert!(crate::losses::journal::blocked_character_in_memory(&world,100));
    assert!(crate::losses::journal::blocked_in_memory(&world,101));
}
