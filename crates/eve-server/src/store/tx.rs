//! The live data and changes to it. [`Db`] holds every table and sequence;
//! a [`Tx`] changes them while the store is locked, keeping an undo log so a
//! transaction that fails leaves no trace, and the changes it made for the
//! write-ahead log. The constraints PostgreSQL enforced are enforced here:
//! types, NOT NULL, CHECKs, unique indexes and foreign keys, and the triggers
//! it ran are run here.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use super::schema::{Default, OnDelete, Schema, TableDef};
use super::table::{RowId, RowView, Table};
use super::wal::Change;
use super::Value;

#[derive(Clone)]
pub struct Db {
    pub(super) schema: Arc<Schema>,
    /// Shared until changed: a copy of the data (a test's world, a
    /// snapshot) copies a table only when it is written to.
    pub(super) tables: Vec<Arc<Table>>,
    by_name: HashMap<Arc<str>, usize>,
    /// Each sequence's last value, None before its first.
    pub(super) sequences: HashMap<Arc<str>, Option<i64>>,
    /// For each table, the (table, foreign key) pairs that refer to it.
    referrers: Arc<Vec<Vec<(usize, usize)>>>,
}

/// Why a change was refused. `code` is the SQLSTATE PostgreSQL gave for the
/// same refusal, for code that told them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError {
    pub code: &'static str,
    pub message: String,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StoreError {}

pub const UNIQUE_VIOLATION: &str = "23505";
pub const CHECK_VIOLATION: &str = "23514";
pub const NOT_NULL_VIOLATION: &str = "23502";
pub const FOREIGN_KEY_VIOLATION: &str = "23503";
pub const DATA_EXCEPTION: &str = "22000";

fn refuse(code: &'static str, message: String) -> StoreError {
    StoreError { code, message }
}

impl Db {
    pub(super) fn empty(schema: Arc<Schema>) -> Db {
        let tables: Vec<Arc<Table>> = schema.tables.iter().map(|d| Arc::new(Table::new(d.clone()))).collect();
        let by_name = schema.tables.iter().enumerate().map(|(i, d)| (d.name.clone(), i)).collect();
        let sequences = schema.sequences.iter().map(|s| (s.name.clone(), None)).collect();
        let mut referrers = vec![Vec::new(); schema.tables.len()];
        for (ct, child) in schema.tables.iter().enumerate() {
            for (k, fk) in child.foreign_keys.iter().enumerate() {
                if let Some(p) = schema.tables.iter().position(|t| t.name == fk.references) {
                    referrers[p].push((ct, k));
                }
            }
        }
        Db { schema, tables, by_name, sequences, referrers: Arc::new(referrers) }
    }

    pub(super) fn index_of(&self, name: &str) -> usize {
        *self.by_name.get(name).unwrap_or_else(|| panic!("{name} is not a live table"))
    }

    /// A table by name. Panics on a name that is not a live table: the names
    /// are written in the code, so that is a bug for the tests to catch.
    pub fn table(&self, name: &str) -> &Table {
        &self.tables[self.index_of(name)]
    }

    /// A sequence's last value, None before its first (`currval`, but
    /// without needing this transaction to have advanced it).
    pub fn sequence(&self, name: &str) -> Option<i64> {
        *self.sequences.get(name).unwrap_or_else(|| panic!("{name} is not a sequence"))
    }

    /// Table `t` to change, copied first if another copy of the data shares it.
    pub(super) fn table_mut(&mut self, t: usize) -> &mut Table {
        Arc::make_mut(&mut self.tables[t])
    }
}

enum Undo {
    Insert { table: usize, id: RowId },
    Update { table: usize, id: RowId, old: Box<[Value]> },
    Delete { table: usize, id: RowId, old: Box<[Value]> },
}

/// A write transaction: every change applies at once when it commits, or
/// none does.
pub struct Tx<'d> {
    db: &'d mut Db,
    undo: Vec<Undo>,
    pub(super) log: Vec<Change>,
    pub(super) sequences_moved: bool,
    committed: bool,
}

/// A transaction dropped without committing (it returned an error, or its
/// code panicked) undoes what it did.
impl Drop for Tx<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.undo_all();
        }
    }
}

/// What a write sets: column names and their new values.
pub type Sets<'a> = &'a [(&'a str, Value)];

impl<'d> Tx<'d> {
    pub(super) fn new(db: &'d mut Db) -> Tx<'d> {
        Tx { db, undo: Vec::new(), log: Vec::new(), sequences_moved: false, committed: false }
    }

    /// The data as this transaction has made it so far.
    pub fn db(&self) -> &Db {
        self.db
    }

    pub fn table(&self, name: &str) -> &Table {
        self.db.table(name)
    }

    /// Keep what the transaction did.
    pub(super) fn commit(&mut self) {
        self.committed = true;
        self.undo.clear();
    }

    fn undo_all(&mut self) {
        while let Some(u) = self.undo.pop() {
            match u {
                Undo::Insert { table, id } => {
                    self.db.table_mut(table).take(id);
                }
                Undo::Update { table, id, old } => {
                    self.db.table_mut(table).replace(id, old);
                }
                Undo::Delete { table, id, old } => {
                    self.db.table_mut(table).put(Some(id), old);
                }
            }
        }
    }

    /// The sequence's next value (`nextval`). Like PostgreSQL's, it is not
    /// handed out again when the transaction fails.
    pub fn nextval(&mut self, name: &str) -> Result<i64, StoreError> {
        let def = self
            .db
            .schema
            .sequences
            .iter()
            .find(|s| &*s.name == name)
            .unwrap_or_else(|| panic!("{name} is not a sequence"))
            .clone();
        let slot = self.db.sequences.get_mut(name).expect("every sequence has a slot");
        let next = match *slot {
            None => def.start,
            Some(v) => v.checked_add(def.increment).ok_or_else(|| refuse(DATA_EXCEPTION, format!("sequence {name} overflowed")))?,
        };
        if next > def.max || next < def.min {
            return Err(refuse(DATA_EXCEPTION, format!("nextval: reached maximum value of sequence \"{name}\" ({})", def.max)));
        }
        *slot = Some(next);
        self.sequences_moved = true;
        Ok(next)
    }

    /// Set a sequence's last value (`setval(name, value)`).
    pub fn setval(&mut self, name: &str, value: i64) {
        *self.db.sequences.get_mut(name).unwrap_or_else(|| panic!("{name} is not a sequence")) = Some(value);
        self.sequences_moved = true;
    }

    /// Insert a row: the columns named take the values given, the others
    /// their defaults (or NULL). Returns the new row's id.
    pub fn insert(&mut self, table: &str, sets: Sets) -> Result<RowId, StoreError> {
        let t = self.db.index_of(table);
        let def = self.db.tables[t].def.clone();
        let mut cells: Vec<Value> = vec![Value::Null; def.columns.len()];
        let mut given = vec![false; def.columns.len()];
        for (name, v) in sets {
            let c = def.col(name);
            cells[c] = v.clone();
            given[c] = true;
        }
        for (c, col) in def.columns.iter().enumerate() {
            if given[c] {
                continue;
            }
            cells[c] = match &col.default {
                None => Value::Null,
                Some(Default::Value(v)) => v.clone(),
                Some(Default::Sequence(s)) => Value::Int(self.nextval(s)?),
            };
        }
        let cells = self.validated(&def, cells)?;
        if let Some((ix, _)) = self.db.tables[t].conflict(&cells, None) {
            return Err(unique_violation(&def, ix, &cells));
        }
        self.check_references(&def, &cells, None)?;
        let id = self.db.table_mut(t).put(None, cells.clone());
        self.undo.push(Undo::Insert { table: t, id });
        self.log.push(Change::Insert { table: t, row: cells.clone() });
        self.after(t, None, Some(&cells), &[])?;
        Ok(id)
    }

    /// Insert a row unless one of the table's unique indexes already holds
    /// its key (`ON CONFLICT DO NOTHING`): the new row's id, or None.
    pub fn insert_or_ignore(&mut self, table: &str, sets: Sets) -> Result<Option<RowId>, StoreError> {
        match self.insert(table, sets) {
            Ok(id) => Ok(Some(id)),
            Err(e) if e.code == UNIQUE_VIOLATION => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Insert a row, or where the unique index `on` (named by its columns)
    /// already holds its key, update that row with `update` instead (`ON
    /// CONFLICT (on) DO UPDATE SET update`). Returns the row's id.
    pub fn upsert(&mut self, table: &str, on: &[&str], sets: Sets, update: Sets) -> Result<RowId, StoreError> {
        let t = self.db.index_of(table);
        let def = self.db.tables[t].def.clone();
        let cols: Vec<usize> = on.iter().map(|c| def.col(c)).collect();
        let ix = def
            .indexes
            .iter()
            .position(|i| i.unique && i.cols == cols)
            .unwrap_or_else(|| panic!("{table}: no unique index on {on:?} to resolve a conflict on"));
        let mut probe: Vec<Value> = vec![Value::Null; def.columns.len()];
        for (name, v) in sets {
            let c = def.col(name);
            probe[c] = def.columns[c].ty.coerce(v.clone());
        }
        // A partial index's condition may read columns the insert leaves to
        // their defaults: fill them as the insert would (but not sequences).
        for (c, col) in def.columns.iter().enumerate() {
            if !sets.iter().any(|(n, _)| def.col(n) == c) {
                if let Some(Default::Value(v)) = &col.default {
                    probe[c] = v.clone();
                }
            }
        }
        match self.db.tables[t].holder(ix, &probe) {
            Some(id) => {
                self.update(table, id, update)?;
                Ok(id)
            }
            None => self.insert(table, sets),
        }
    }

    /// Set columns of the row `id` (`UPDATE ... SET`). Triggers that watch a
    /// column fire when it is set, changed or not, as PostgreSQL's `UPDATE
    /// OF` triggers do.
    pub fn update(&mut self, table: &str, id: RowId, sets: Sets) -> Result<(), StoreError> {
        let t = self.db.index_of(table);
        let def = self.db.tables[t].def.clone();
        let old: Box<[Value]> = match self.db.tables[t].row(id) {
            Some(r) => r.cells().into(),
            None => return Err(refuse(DATA_EXCEPTION, format!("{table}: row {id} no longer exists"))),
        };
        let mut cells = old.to_vec();
        let mut set_cols = Vec::with_capacity(sets.len());
        for (name, v) in sets {
            let c = def.col(name);
            cells[c] = v.clone();
            set_cols.push(c);
        }
        let cells = self.validated(&def, cells)?;
        if let Some((ix, _)) = self.db.tables[t].conflict(&cells, Some(id)) {
            return Err(unique_violation(&def, ix, &cells));
        }
        self.check_references(&def, &cells, Some(&old))?;
        self.check_referrers_kept(t, &old, &cells)?;
        self.before(t, &old, &cells, &set_cols)?;
        self.db.table_mut(t).replace(id, cells.clone());
        self.undo.push(Undo::Update { table: t, id, old: old.clone() });
        self.log.push(Change::Update { table: t, old: old.clone(), row: cells.clone() });
        self.after(t, Some(&old), Some(&cells), &set_cols)
    }

    /// Delete the row `id`, and what its foreign keys cascade to.
    pub fn delete(&mut self, table: &str, id: RowId) -> Result<(), StoreError> {
        let t = self.db.index_of(table);
        self.delete_at(t, id)
    }

    fn delete_at(&mut self, t: usize, id: RowId) -> Result<(), StoreError> {
        let Some(old) = self.db.tables[t].row(id).map(|r| Box::<[Value]>::from(r.cells())) else { return Ok(()) };
        self.on_parent_delete(t, &old)?;
        // A cascade may have reached this row already.
        let Some(old) = self.db.table_mut(t).take(id) else { return Ok(()) };
        self.undo.push(Undo::Delete { table: t, id, old: old.clone() });
        self.log.push(Change::Delete { table: t, old: old.clone() });
        self.after(t, Some(&old), None, &[])
    }

    /// Delete every row whose `columns` equal `values`; how many.
    pub fn delete_where(&mut self, table: &str, columns: &[&str], values: &[Value]) -> Result<usize, StoreError> {
        let ids: Vec<RowId> = self.table(table).find(columns, values).map(|r| r.id).collect();
        for id in &ids {
            self.delete(table, *id)?;
        }
        Ok(ids.len())
    }

    /// Coerce, type-check, and check NOT NULL and the CHECK constraints.
    fn validated(&self, def: &TableDef, cells: Vec<Value>) -> Result<Box<[Value]>, StoreError> {
        let mut out = Vec::with_capacity(cells.len());
        for (c, v) in cells.into_iter().enumerate() {
            let col = &def.columns[c];
            let v = col.ty.coerce(v);
            if !col.ty.admits(&v) {
                return Err(refuse(DATA_EXCEPTION, format!("{}.{}: {v:?} does not fit a {:?} column", def.name, col.name, col.ty)));
            }
            if col.not_null && v.is_null() {
                return Err(refuse(
                    NOT_NULL_VIOLATION,
                    format!("null value in column \"{}\" of relation \"{}\" violates not-null constraint", col.name, def.name),
                ));
            }
            out.push(v);
        }
        let view = RowView { def, cells: &out };
        for (name, check) in &def.checks {
            if !check(&view) {
                return Err(refuse(CHECK_VIOLATION, format!("new row for relation \"{}\" violates check constraint \"{name}\"", def.name)));
            }
        }
        Ok(out.into())
    }

    /// The rows `cells` refers to exist (skipping keys that did not change
    /// on an update, as PostgreSQL does).
    fn check_references(&self, def: &TableDef, cells: &[Value], old: Option<&[Value]>) -> Result<(), StoreError> {
        for fk in &def.foreign_keys {
            let key: Vec<Value> = fk.cols.iter().map(|c| cells[*c].clone()).collect();
            if key.iter().any(Value::is_null) {
                continue;
            }
            if let Some(old) = old {
                if fk.cols.iter().all(|c| super::table::compare(&old[*c], &cells[*c]).is_eq()) {
                    continue;
                }
            }
            let parent = self.db.table(&fk.references);
            let ix = parent.def.indexes.iter().position(|i| i.unique && i.filter.is_none() && i.cols == fk.referenced_cols).expect("checked when the schema loaded");
            let mut probe = vec![Value::Null; parent.def.columns.len()];
            for (k, c) in fk.referenced_cols.iter().enumerate() {
                probe[*c] = key[k].clone();
            }
            if parent.holder(ix, &probe).is_none() {
                return Err(refuse(
                    FOREIGN_KEY_VIOLATION,
                    format!("insert or update on table \"{}\" violates a foreign key: no {} row {key:?}", def.name, fk.references),
                ));
            }
        }
        Ok(())
    }

    /// The tables whose foreign keys refer to table `t`, with those keys.
    fn referrers(&self, t: usize) -> Vec<(usize, usize)> {
        self.db.referrers[t].clone()
    }

    fn referring_rows(&self, ct: usize, k: usize, parent: &[Value]) -> Vec<RowId> {
        let child = &self.db.tables[ct];
        let fk = &child.def.foreign_keys[k];
        let key: Vec<Value> = fk.referenced_cols.iter().map(|c| parent[*c].clone()).collect();
        let names: Vec<&str> = fk.cols.iter().map(|c| &*child.def.columns[*c].name).collect();
        child.find(&names, &key).map(|r| r.id).collect()
    }

    fn on_parent_delete(&mut self, t: usize, old: &[Value]) -> Result<(), StoreError> {
        for (ct, k) in self.referrers(t) {
            let rows = self.referring_rows(ct, k, old);
            if rows.is_empty() {
                continue;
            }
            let fk = self.db.tables[ct].def.foreign_keys[k].clone();
            match fk.on_delete {
                OnDelete::Cascade => {
                    for id in rows {
                        self.delete_at(ct, id)?;
                    }
                }
                OnDelete::SetNull => {
                    let child = self.db.tables[ct].def.clone();
                    let sets: Vec<(&str, Value)> = fk.cols.iter().map(|c| (&*child.columns[*c].name, Value::Null)).collect();
                    for id in rows {
                        self.update(&child.name, id, &sets)?;
                    }
                }
                OnDelete::Refuse => {
                    return Err(refuse(
                        FOREIGN_KEY_VIOLATION,
                        format!(
                            "update or delete on table \"{}\" violates a foreign key on table \"{}\"",
                            self.db.tables[t].def.name, self.db.tables[ct].def.name
                        ),
                    ))
                }
            }
        }
        Ok(())
    }

    /// An update may not take away a key other rows refer to.
    fn check_referrers_kept(&self, t: usize, old: &[Value], new: &[Value]) -> Result<(), StoreError> {
        for (ct, k) in self.referrers(t) {
            let fk = &self.db.tables[ct].def.foreign_keys[k];
            if fk.referenced_cols.iter().all(|c| super::table::compare(&old[*c], &new[*c]).is_eq()) {
                continue;
            }
            if !self.referring_rows(ct, k, old).is_empty() {
                return Err(refuse(
                    FOREIGN_KEY_VIOLATION,
                    format!(
                        "update on table \"{}\" violates a foreign key on table \"{}\"",
                        self.db.tables[t].def.name, self.db.tables[ct].def.name
                    ),
                ));
            }
        }
        Ok(())
    }

    /// The BEFORE UPDATE triggers PostgreSQL ran on `entity`.
    fn before(&mut self, t: usize, old: &[Value], new: &[Value], set: &[usize]) -> Result<(), StoreError> {
        if &*self.db.tables[t].def.name != "entity" {
            return Ok(());
        }
        let def = self.db.tables[t].def.clone();
        let set_any = |cols: &[&str]| cols.iter().any(|c| set.contains(&def.col(c)));
        let o = |c: &str| &old[def.col(c)];
        let n = |c: &str| &new[def.col(c)];
        let distinct = |c: &str| !super::table::compare(o(c), n(c)).is_eq() || o(c).is_null() != n(c).is_null();
        let item = o("itemID").clone();
        // invalidate_ship_space_motion: a hull moved or changed keeps no
        // stored motion.
        if set_any(&["ownerID", "locationID", "flag", "typeID", "quantity", "x", "y", "z"]) {
            self.delete_where("shipSpaceMotion", &["itemID"], std::slice::from_ref(&item))?;
        }
        // invalidate_cloak_cooldowns: a module that changes hands or form
        // loses its cooldown; one that leaves its hull loses its binding.
        if set_any(&["ownerID", "typeID", "singleton", "quantity", "locationID", "flag"]) {
            if distinct("ownerID") || distinct("typeID") || distinct("singleton") || distinct("quantity") {
                let ids: Vec<RowId> = self
                    .table("cloakCooldowns")
                    .rows()
                    .filter(|r| r.value("itemID") == &item || r.value("shipID") == &item)
                    .map(|r| r.id)
                    .collect();
                for id in ids {
                    self.delete("cloakCooldowns", id)?;
                }
            } else if distinct("locationID") || (distinct("flag") && !(11..=35).contains(&n("flag").int().unwrap_or(0))) {
                let ids: Vec<RowId> =
                    self.table("cloakCooldowns").find(&["itemID"], std::slice::from_ref(&item)).filter(|r| r.value("shipID") != &item).map(|r| r.id).collect();
                for id in ids {
                    self.delete("cloakCooldowns", id)?;
                }
            }
        }
        Ok(())
    }

    /// The AFTER triggers PostgreSQL ran.
    fn after(&mut self, t: usize, old: Option<&[Value]>, new: Option<&[Value]>, set: &[usize]) -> Result<(), StoreError> {
        let def = self.db.tables[t].def.clone();
        match &*def.name {
            // invalidate_drone_launch / invalidate_maintenance_custody: an
            // item whose custody or form changes drops what depended on it.
            "entity" => {
                let (Some(old), Some(new)) = (old, new) else { return Ok(()) };
                let watched = ["ownerID", "locationID", "flag", "typeID", "quantity", "singleton"];
                if !watched.iter().any(|c| set.contains(&def.col(c))) {
                    return Ok(());
                }
                let changed = watched.iter().any(|c| {
                    let i = def.col(c);
                    !super::table::compare(&old[i], &new[i]).is_eq() || old[i].is_null() != new[i].is_null()
                });
                if changed {
                    let item = old[def.col("itemID")].clone();
                    self.delete_where("launched_drone_state", &["drone_id"], std::slice::from_ref(&item))?;
                    self.delete_where("maintenance_custody", &["item_id"], std::slice::from_ref(&item))?;
                }
                Ok(())
            }
            // sync_alliance_shortnames: alliance_shortnames mirrors each
            // alliance's short name.
            "alliances" => {
                let id = |row: &[Value]| row[def.col("allianceID")].clone();
                match (old, new) {
                    (Some(old), None) => {
                        self.delete_where("alliance_shortnames", &["allianceID"], &[id(old)])?;
                    }
                    (_, Some(new)) => {
                        let short = new[def.col("shortName")].clone();
                        self.upsert("alliance_shortnames", &["allianceID"], &[("allianceID", id(new)), ("shortName", short.clone())], &[("shortName", short)])?;
                    }
                    (None, None) => {}
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

fn unique_violation(def: &TableDef, ix: usize, cells: &[Value]) -> StoreError {
    let index = &def.indexes[ix];
    let key: Vec<String> = index.cols.iter().map(|c| format!("{}={:?}", def.columns[*c].name, cells[*c])).collect();
    refuse(UNIQUE_VIOLATION, format!("duplicate key value violates unique constraint \"{}\" ({})", index.name, key.join(", ")))
}
