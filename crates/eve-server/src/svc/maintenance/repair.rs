//! The item move commits before its live hull disappears. Preserve damage that
//! arrived during commit with a guarded final flush; never resurrect old custody.
use super::*;

#[derive(Debug,Clone)]
pub struct Repair {
    pub(super) id:i64,
    pub(super) host:i64,
    pub(super) owner:i64,
    pub(super) flag:i64,
    pub(super) generation:i64,
    pub(super) at_ms:i64,
    pub(super) fighter:crate::combat::Fighter,
}

/// `maintenance_custody`, its generation sequence and the entity trigger
/// that drops custody an item leaves are part of the store's schema.
pub fn ensure(_tx:&mut Tx<'_>)->Result<(),store::StoreError>{
    Ok(())
}

pub(super) fn begin(tx:&mut Tx<'_>,id:i64,host:i64,owner:i64,flag:i64)->Result<i64,MoveError>{
    let sets=[("host_id",store::Value::Int(host)),("owner_id",store::Value::Int(owner)),("flag",store::Value::Int(flag))];
    let row=match tx.table("maintenance_custody").get_id(id).map(|r|r.id) {
        Some(row)=>{
            let generation=tx.nextval("maintenance_generation")?;
            let mut update=sets.to_vec();update.push(("generation",store::Value::Int(generation)));
            tx.update("maintenance_custody",row,&update)?;row
        }
        None=>{
            let mut insert=sets.to_vec();insert.push(("item_id",store::Value::Int(id)));
            tx.insert("maintenance_custody",&insert)?
        }
    };
    Ok(tx.table("maintenance_custody").row(row).and_then(|r|r.int("generation")).unwrap_or(0))
}

pub(super) fn ready(world:&World,id:i64)->Result<(),MoveError>{
    if world.lock().maintenance_repairs.contains_key(&id){return refuse("The ship is finishing its maintenance-bay transfer. Try again shortly.");}
    Ok(())
}

fn flush(store:&store::Store,r:&Repair)->Result<(),store::StoreError>{
    store.write(|tx|{
        let held=tx.table("entity").get_id(r.id).map(|e|e.int("ownerID")==Some(r.owner) && e.int("locationID")==Some(r.host) && e.int("flag")==Some(r.flag));
        if held!=Some(true){return Ok(());}
        let custody=tx.table("maintenance_custody").get_id(r.id).is_some_and(|c|c.int("generation")==Some(r.generation)
            && c.int("host_id")==Some(r.host) && c.int("owner_id")==Some(r.owner) && c.int("flag")==Some(r.flag));
        if !custody{return Ok(());}
        inflight::write_vitals_at(tx,r.id,&r.fighter,false,r.at_ms)
    })
}

pub(super) async fn attempt(world:&World,store:&store::Store,r:Repair){
    match flush(store,&r) {
        Ok(())=>{let mut w=world.lock();if w.maintenance_repairs.get(&r.id).is_some_and(|p|p.generation==r.generation){w.maintenance_repairs.remove(&r.id);}},
        Err(e)=>tracing::warn!("final maintenance snapshot {} is queued for retry: {}",r.id,crate::logging::chain(&e)),
    }
}

pub async fn retry_repairs(world:&World,store:&store::Store){
    let pending:Vec<_>=world.lock().maintenance_repairs.values().cloned().collect();
    for r in pending{attempt(world,store,r).await;}
}
