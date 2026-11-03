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

pub async fn ensure(db:&tokio_postgres::Client)->Result<(),tokio_postgres::Error>{
    db.batch_execute(r#"
        CREATE SEQUENCE IF NOT EXISTS maintenance_generation;
        CREATE TABLE IF NOT EXISTS maintenance_custody (
            item_id integer PRIMARY KEY REFERENCES entity("itemID") ON DELETE CASCADE,
            generation bigint NOT NULL DEFAULT nextval('maintenance_generation'),
            host_id integer NOT NULL, owner_id integer NOT NULL, flag integer NOT NULL);
        CREATE OR REPLACE FUNCTION invalidate_maintenance_custody() RETURNS trigger LANGUAGE plpgsql AS $$
        BEGIN
            IF ROW(OLD."ownerID",OLD."locationID",OLD.flag,OLD."typeID",OLD.quantity,OLD.singleton)
                IS DISTINCT FROM ROW(NEW."ownerID",NEW."locationID",NEW.flag,NEW."typeID",NEW.quantity,NEW.singleton) THEN
                DELETE FROM maintenance_custody WHERE item_id=OLD."itemID";
            END IF;
            RETURN NEW;
        END $$;
        DO $$ BEGIN
            IF NOT EXISTS (SELECT 1 FROM pg_trigger WHERE tgname='invalidate_maintenance_custody') THEN
                CREATE TRIGGER invalidate_maintenance_custody AFTER UPDATE OF "ownerID","locationID",flag,"typeID",quantity,singleton
                    ON entity FOR EACH ROW EXECUTE FUNCTION invalidate_maintenance_custody();
            END IF;
        END $$;
    "#).await
}

pub(super) async fn begin(db:&impl GenericClient,id:i64,host:i64,owner:i64,flag:i64)->Result<i64,MoveError>{
    Ok(db.query_one("INSERT INTO maintenance_custody(item_id,host_id,owner_id,flag) VALUES($1,$2,$3,$4)
        ON CONFLICT(item_id) DO UPDATE SET generation=nextval('maintenance_generation'),host_id=EXCLUDED.host_id,owner_id=EXCLUDED.owner_id,flag=EXCLUDED.flag RETURNING generation",
        &[&(id as i32),&(host as i32),&(owner as i32),&(flag as i32)]).await?.get(0))
}

pub(super) fn ready(world:&World,id:i64)->Result<(),MoveError>{
    if world.lock().maintenance_repairs.contains_key(&id){return refuse("The ship is finishing its maintenance-bay transfer. Try again shortly.");}
    Ok(())
}

async fn flush(db:&mut tokio_postgres::Client,r:&Repair)->Result<(),tokio_postgres::Error>{
    let tx=db.transaction().await?;
    let row=tx.query_opt(r#"SELECT "ownerID","locationID",flag FROM entity WHERE "itemID"=$1 FOR UPDATE"#,&[&(r.id as i32)]).await?;
    let Some(row)=row else{return Ok(());};
    if row.get::<_,i32>(0) as i64!=r.owner || row.get::<_,i32>(1) as i64!=r.host || row.get::<_,i32>(2) as i64!=r.flag{return Ok(());}
    if tx.query_opt("SELECT 1 FROM maintenance_custody WHERE item_id=$1 AND generation=$2 AND host_id=$3 AND owner_id=$4 AND flag=$5 FOR UPDATE",
        &[&(r.id as i32),&r.generation,&(r.host as i32),&(r.owner as i32),&(r.flag as i32)]).await?.is_none(){return Ok(());}
    inflight::write_vitals_at(&tx,r.id,&r.fighter,false,r.at_ms).await?;
    tx.commit().await
}

pub(super) async fn attempt(world:&World,url:&str,r:Repair){
    let result=async{let mut db=crate::db::connect(url).await?;flush(&mut db,&r).await}.await;
    match result {
        Ok(())=>{let mut w=world.lock();if w.maintenance_repairs.get(&r.id).is_some_and(|p|p.generation==r.generation){w.maintenance_repairs.remove(&r.id);}},
        Err(e)=>tracing::warn!("final maintenance snapshot {} is queued for retry: {}",r.id,crate::logging::chain(&e)),
    }
}

pub async fn retry_repairs(world:&World,url:&str){
    let pending:Vec<_>=world.lock().maintenance_repairs.values().cloned().collect();
    for r in pending{attempt(world,url,r).await;}
}
