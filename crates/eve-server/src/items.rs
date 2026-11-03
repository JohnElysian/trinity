//! Item changes made in one database transaction, and the `OnItemChange`
//! events that tell the client about them.
//!
//! Every item an operation touches is locked and snapshotted the first time it
//! is read; on commit each one is compared with how it ended up, so an
//! operation only says what to do and the events follow from the data
//! (invCache.py:111 reads them: an item at quantity 0 is gone, one whose old
//! location is 0 is new).

use std::collections::{HashMap,HashSet};

use eve_marshal::Value;
use tokio_postgres::{Client, Transaction};

use crate::svc::inventory::{invitem_cells, item_change, INVITEM_SELECT, IX_LOCATION_ID, IX_QUANTITY};

pub type DbResult<T> = Result<T, tokio_postgres::Error>;

/// What an operation needs to know about an item.
#[derive(Debug, Clone)]
pub struct Item {
    pub id: i64,
    pub type_id: i64,
    pub owner_id: i64,
    pub location_id: i64,
    pub flag: i64,
    pub singleton: bool,
    pub quantity: i64,
    pub category_id: i64,
    /// Volume of one unit.
    pub volume: f64,
}

/// The item row columns compared on commit: itemID up to quantity.
const COMPARED_COLUMNS: usize = 8;

pub struct ItemTxn<'c> {
    tx: Transaction<'c>,
    /// Each touched item as it was before this transaction; None if it is new.
    before: HashMap<i64, Option<Vec<Value>>>,
    order: Vec<i64>,
}

impl<'c> ItemTxn<'c> {
    pub async fn begin(client: &'c mut Client) -> DbResult<Self> {
        Ok(ItemTxn { tx: client.transaction().await?, before: HashMap::new(), order: Vec::new() })
    }

    /// The transaction, for writes that are not item changes (wallets, orders).
    pub fn tx(&self) -> &Transaction<'c> {
        &self.tx
    }

    /// Lock `id` and read it; the first read is the snapshot.
    pub async fn item(&mut self, id: i64) -> DbResult<Option<Item>> {
        let sql = format!(
            r#"{} e."itemID" = $1 FOR UPDATE OF e"#,
            INVITEM_SELECT.replace(r#"e."customInfo""#, r#"e."customInfo", t."volume""#)
        );
        let Some(r) = self.tx.query_opt(&sql, &[&(id as i32)]).await? else { return Ok(None) };
        if let std::collections::hash_map::Entry::Vacant(slot) = self.before.entry(id) {
            slot.insert(Some(invitem_cells(&r)));
            self.order.push(id);
        }
        let int = |i: usize| r.get::<_, i32>(i) as i64;
        Ok(Some(Item {
            id,
            type_id: int(1),
            owner_id: int(2),
            location_id: int(3),
            flag: int(4),
            singleton: int(6) != 0,
            quantity: int(7),
            category_id: int(9),
            volume: r.get::<_, Option<f64>>(11).unwrap_or(0.0),
        }))
    }

    /// Lock and read several items; missing ones are left out.
    pub async fn items(&mut self, ids: &[i64]) -> DbResult<Vec<Item>> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.extend(self.item(*id).await?);
        }
        Ok(out)
    }

    async fn touch(&mut self, id: i64) -> DbResult<()> {
        if !self.before.contains_key(&id) {
            self.item(id).await?;
        }
        Ok(())
    }

    pub async fn move_to(&mut self, id: i64, location_id: i64, flag: i64) -> DbResult<()> {
        self.touch(id).await?;
        self.tx
            .execute(
                r#"UPDATE entity SET "locationID" = $2, "flag" = $3 WHERE "itemID" = $1"#,
                &[&(id as i32), &(location_id as i32), &(flag as i32)],
            )
            .await?;
        Ok(())
    }

    /// Set a stack's size; 0 destroys the item.
    pub async fn set_quantity(&mut self, id: i64, quantity: i64) -> DbResult<()> {
        if quantity <= 0 {
            return self.destroy(id).await;
        }
        self.touch(id).await?;
        self.tx
            .execute(r#"UPDATE entity SET "quantity" = $2 WHERE "itemID" = $1"#, &[&(id as i32), &(quantity as i32)])
            .await?;
        Ok(())
    }

    pub async fn set_owner(&mut self, id: i64, owner_id: i64) -> DbResult<()> {
        self.touch(id).await?;
        self.tx
            .execute(r#"UPDATE entity SET "ownerID" = $2 WHERE "itemID" = $1"#, &[&(id as i32), &(owner_id as i32)])
            .await?;
        Ok(())
    }

    /// Where an item floats in space (not compared on commit: the client
    /// learns positions from destiny).
    pub async fn set_position(&mut self, id: i64, pos: (f64, f64, f64)) -> DbResult<()> {
        self.tx
            .execute(r#"UPDATE entity SET "x" = $2, "y" = $3, "z" = $4 WHERE "itemID" = $1"#, &[&(id as i32), &pos.0, &pos.1, &pos.2])
            .await?;
        Ok(())
    }

    /// Where every item touched so far was before this transaction.
    pub fn origins(&self) -> Vec<i64> {
        let mut out: Vec<i64> = self
            .before
            .values()
            .flatten()
            .filter_map(|cells| cells.get(IX_LOCATION_ID as usize).and_then(|v| v.as_i64()))
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    pub async fn set_singleton(&mut self, id: i64, singleton: bool) -> DbResult<()> {
        self.touch(id).await?;
        self.tx
            .execute(
                r#"UPDATE entity SET "singleton" = $2 WHERE "itemID" = $1"#,
                &[&(id as i32), &i32::from(singleton)],
            )
            .await?;
        Ok(())
    }

    pub async fn destroy(&mut self, id: i64) -> DbResult<()> {
        self.touch(id).await?;
        self.tx.execute(r#"DELETE FROM entity_attributes WHERE "itemID" = $1"#, &[&(id as i32)]).await?;
        self.tx.execute(r#"DELETE FROM entity WHERE "itemID" = $1"#, &[&(id as i32)]).await?;
        Ok(())
    }

    /// Destroy only the subtree still attached when each row is locked. A
    /// transfer that wins a child lock may move its entire subtree away; do
    /// not follow or delete it using a stale list collected before the wait.
    /// `protected` lets ship loss retain its pilot even in malformed nesting.
    pub async fn destroy_tree(&mut self, root:i64, expected_parent:Option<i64>, protected:&[i64]) -> DbResult<()> {
        let mut pending=vec![(root,expected_parent)];
        let mut seen=HashSet::new();
        while let Some((id,parent))=pending.pop() {
            if protected.contains(&id) || seen.contains(&id) { continue; }
            let Some(item)=self.item(id).await? else { continue; };
            if parent.is_some_and(|parent|item.location_id!=parent) { continue; }
            seen.insert(id);
            let children=self.tx.query(r#"SELECT "itemID" FROM entity WHERE "locationID"=$1 ORDER BY "itemID""#,
                &[&(id as i32)]).await?;
            for row in children.into_iter().rev() { pending.push((i64::from(row.get::<_,i32>(0)),Some(id))); }
            self.destroy(id).await?;
        }
        Ok(())
    }

    /// Hand over the subtree still attached when each item is locked. Children
    /// moved elsewhere while we waited are no longer part of the handover.
    /// False means custody was invalid; callers must roll back the transaction.
    pub async fn transfer_tree(&mut self, root:i64, from:i64, to:i64) -> DbResult<bool> {
        let mut pending=vec![(root,None)];
        let mut seen=HashSet::new();
        while let Some((id,parent))=pending.pop() {
            if !seen.insert(id) { return Ok(false); }
            let Some(item)=self.item(id).await? else {
                if id==root { return Ok(false); }
                continue;
            };
            if parent.is_some_and(|parent|item.location_id!=parent) { continue; }
            if item.owner_id!=from || matches!(item.flag,7|36|57|61|88|89) {
                return Ok(false);
            }
            // Characters and their active ships cannot be handed over as cargo.
            if self.tx.query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 OR "shipID"=$1 LIMIT 1"#,
                &[&(id as i32)]).await?.is_some() { return Ok(false); }
            let children=self.tx.query(r#"SELECT "itemID" FROM entity WHERE "locationID"=$1 ORDER BY "itemID""#,
                &[&(id as i32)]).await?;
            for row in children.into_iter().rev() { pending.push((i64::from(row.get::<_,i32>(0)),Some(id))); }
            self.set_owner(id,to).await?;
        }
        Ok(true)
    }

    /// A new item; returns its ID.
    pub async fn create(&mut self, new: NewItem) -> DbResult<i64> {
        let r = self
            .tx
            .query_one(
                r#"INSERT INTO entity ("itemID","itemName","typeID","ownerID","locationID","flag",
                                       "contraband","singleton","quantity","x","y","z","customInfo")
                   VALUES (nextval('dyn_id_seq'), '', $1, $2, $3, $4, 0, $5, $6, 0, 0, 0, NULL)
                   RETURNING "itemID""#,
                &[
                    &(new.type_id as i32),
                    &(new.owner_id as i32),
                    &(new.location_id as i32),
                    &(new.flag as i32),
                    &i32::from(new.singleton),
                    &(new.quantity as i32),
                ],
            )
            .await?;
        let id = r.get::<_, i32>(0) as i64;
        self.before.insert(id, None);
        self.order.push(id);
        Ok(id)
    }

    /// Add `quantity` of a type to a stack at (owner, location, flag), making
    /// one if there is none. Returns the stack's ID.
    pub async fn stack_onto(&mut self, new: NewItem) -> DbResult<i64> {
        let existing = self
            .tx
            .query_opt(
                r#"SELECT "itemID" FROM entity
                    WHERE "ownerID" = $1 AND "locationID" = $2 AND "flag" = $3
                      AND "typeID" = $4 AND "singleton" = 0
                    ORDER BY "itemID" LIMIT 1 FOR UPDATE"#,
                &[
                    &(new.owner_id as i32),
                    &(new.location_id as i32),
                    &(new.flag as i32),
                    &(new.type_id as i32),
                ],
            )
            .await?;
        match existing {
            Some(r) => {
                let id = r.get::<_, i32>(0) as i64;
                let Some(item)=self.item(id).await? else { return self.create(new).await; };
                self.set_quantity(id, item.quantity + new.quantity).await?;
                Ok(id)
            }
            None => self.create(new).await,
        }
    }

    /// Commit, and return the `OnItemChange` argument tuples for everything
    /// that changed.
    pub async fn commit(self) -> DbResult<Vec<Value>> {
        let events=self.prepare_events().await?;
        self.commit_prepared().await?;
        Ok(events)
    }

    /// Capture notifications before an operation's final acceptance check.
    /// Durable operation owners retain these across a lost COMMIT reply;
    /// publishing them still requires proof that the transaction committed.
    pub(crate) async fn prepare_events(&self) -> DbResult<Vec<Value>> {
        let ids: Vec<i32> = self.order.iter().map(|i| *i as i32).collect();
        let sql = format!(r#"{INVITEM_SELECT} e."itemID" = ANY($1)"#);
        let after: HashMap<i64, Vec<Value>> = self
            .tx
            .query(&sql, &[&ids])
            .await?
            .iter()
            .map(|r| (r.get::<_, i32>(0) as i64, invitem_cells(r)))
            .collect();
        let mut events = Vec::new();
        for id in &self.order {
            let before = self.before.get(id).cloned().flatten();
            let event = match (before, after.get(id)) {
                (Some(old), Some(now)) => {
                    let changed: Vec<(i64, Value)> = (0..COMPARED_COLUMNS)
                        .filter(|&i| old[i] != now[i])
                        .map(|i| (i as i64, old[i].clone()))
                        .collect();
                    (!changed.is_empty()).then(|| item_change(now.clone(), changed))
                }
                (Some(mut old), None) => {
                    let quantity = old[IX_QUANTITY as usize].clone();
                    old[IX_QUANTITY as usize] = Value::Int(0);
                    Some(item_change(old, vec![(IX_QUANTITY, quantity)]))
                }
                (None, Some(now)) => Some(item_change(
                    now.clone(),
                    vec![(IX_LOCATION_ID, Value::Int(0)), (IX_QUANTITY, Value::Int(0))],
                )),
                (None, None) => None,
            };
            events.extend(event);
        }
        Ok(events)
    }

    /// Send only COMMIT, with no intervening query after final validation.
    pub(crate) async fn commit_prepared(self) -> DbResult<()> {
        self.tx.commit().await
    }
}

pub struct NewItem {
    pub type_id: i64,
    pub owner_id: i64,
    pub location_id: i64,
    pub flag: i64,
    pub quantity: i64,
    pub singleton: bool,
}

/// Queue `OnItemChange` events for one client.
pub fn notify(world: &crate::world::World, client_id: i64, events: Vec<Value>) {
    let world = world.lock();
    for event in events {
        world.notify(&[client_id], "OnItemChange", event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wait on PostgreSQL's lock graph, not a timing assumption: the competing
    /// operation must already have reached the locked row before we commit.
    async fn wait_for_lock(db:&Client,pid:i32) {
        tokio::time::timeout(std::time::Duration::from_secs(10),async {
            loop {
                if db.query_one("SELECT cardinality(pg_blocking_pids($1))>0",&[&pid]).await.unwrap().get::<_,bool>(0) { break; }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await.expect("worker reaches the competing transaction's row lock");
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn recursive_cleanup_keeps_a_subtree_moved_while_waiting_for_its_lock() {
        let mut fixture=crate::test_support::Database::load().await;
        let owner=i64::from(fixture.id().await);
        let mut seed=ItemTxn::begin(&mut fixture.db).await.unwrap();
        let root=seed.create(NewItem {type_id:3293,owner_id:owner,location_id:30000142,flag:0,quantity:1,singleton:true}).await.unwrap();
        let moved=seed.create(NewItem {type_id:3293,owner_id:owner,location_id:root,flag:0,quantity:1,singleton:true}).await.unwrap();
        let nested=seed.create(NewItem {type_id:34,owner_id:owner,location_id:moved,flag:5,quantity:12,singleton:false}).await.unwrap();
        let attached=seed.create(NewItem {type_id:35,owner_id:owner,location_id:root,flag:0,quantity:3,singleton:false}).await.unwrap();
        seed.tx().execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt") VALUES ($1,3,1)"#,&[&(nested as i32)]).await.unwrap();
        seed.commit().await.unwrap();
        let mut moving_db=crate::db::connect(&fixture.url).await.unwrap();
        let mut moving=ItemTxn::begin(&mut moving_db).await.unwrap();
        moving.move_to(moved,60014689,4).await.unwrap();
        let mut deleting_db=crate::db::connect(&fixture.url).await.unwrap();
        let pid:i32=deleting_db.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
        let cleanup=tokio::spawn(async move {
            let mut tx=ItemTxn::begin(&mut deleting_db).await.unwrap();
            tx.destroy_tree(root,None,&[]).await.unwrap();
            tx.commit().await.unwrap()
        });
        wait_for_lock(&fixture.db,pid).await;
        moving.commit().await.unwrap();
        cleanup.await.unwrap();
        let rows=fixture.db.query(r#"SELECT "itemID","locationID" FROM entity WHERE "itemID"=ANY($1) ORDER BY "itemID""#,
            &[&vec![root as i32,moved as i32,nested as i32,attached as i32]]).await.unwrap();
        assert_eq!(rows.len(),2,"only the moved container and its contents survive");
        assert_eq!((rows[0].get::<_,i32>(0) as i64,rows[0].get::<_,i32>(1)),(moved,60014689));
        assert_eq!((rows[1].get::<_,i32>(0) as i64,rows[1].get::<_,i32>(1) as i64),(nested,moved));
        assert!(fixture.db.query_opt(r#"SELECT 1 FROM entity_attributes WHERE "itemID"=$1"#,&[&(nested as i32)]).await.unwrap().is_some());
        // A stale root expectation also protects the complete subtree.
        let mut tx=ItemTxn::begin(&mut fixture.db).await.unwrap();
        tx.destroy_tree(moved,Some(root),&[]).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(fixture.db.query_one(r#"SELECT count(*) FROM entity WHERE "itemID"=ANY($1)"#,&[&vec![moved as i32,nested as i32]]).await.unwrap().get::<_,i64>(0),2);
        let mut tx=ItemTxn::begin(&mut fixture.db).await.unwrap();
        tx.destroy_tree(moved,None,&[]).await.unwrap();
        tx.commit().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn stacking_recreates_a_target_deleted_while_waiting_instead_of_losing_goods() {
        let mut fixture=crate::test_support::Database::load().await;
        let owner=i64::from(fixture.id().await);
        let mut seed=ItemTxn::begin(&mut fixture.db).await.unwrap();
        let old=seed.create(NewItem {type_id:34,owner_id:owner,location_id:60014689,flag:4,quantity:5,singleton:false}).await.unwrap();
        seed.commit().await.unwrap();
        let mut deleting_db=crate::db::connect(&fixture.url).await.unwrap();
        let mut deleting=ItemTxn::begin(&mut deleting_db).await.unwrap();
        deleting.destroy(old).await.unwrap();
        let mut adding_db=crate::db::connect(&fixture.url).await.unwrap();
        let pid:i32=adding_db.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
        let adding=tokio::spawn(async move {
            let mut tx=ItemTxn::begin(&mut adding_db).await.unwrap();
            let id=tx.stack_onto(NewItem {type_id:34,owner_id:owner,location_id:60014689,flag:4,quantity:7,singleton:false}).await.unwrap();
            tx.commit().await.unwrap();
            id
        });
        wait_for_lock(&fixture.db,pid).await;
        deleting.commit().await.unwrap();
        let new=adding.await.unwrap();
        assert_ne!(new,old);
        let rows=fixture.db.query(r#"SELECT "itemID",quantity FROM entity WHERE "ownerID"=$1 AND "locationID"=60014689 AND "typeID"=34"#,&[&(owner as i32)]).await.unwrap();
        assert_eq!(rows.len(),1);
        assert_eq!((rows[0].get::<_,i32>(0) as i64,rows[0].get::<_,i32>(1)),(new,7));
        fixture.db.execute(r#"DELETE FROM entity WHERE "itemID"=$1"#,&[&(new as i32)]).await.unwrap();
    }
}
