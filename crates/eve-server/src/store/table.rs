//! One live table in memory: its rows in the order they were made, and an
//! ordered index for each index the schema declares (the primary key first).

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::schema::{ColType, TableDef};
use super::Value;

/// A row's place in its table. Stable while the server runs; rows are
/// renumbered when the data is loaded, so it is never written to disk.
pub type RowId = u32;

/// An index key: some of a row's values, ordered as a B-tree orders them
/// (NULL first, then booleans, numbers, text).
#[derive(Clone, Debug)]
pub struct Key(pub Box<[Value]>);

fn rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) | Value::Float(_) => 2,
        Value::Text(_) => 3,
    }
}

pub(super) fn compare(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.total_cmp(y),
        (Value::Int(x), Value::Float(y)) => (*x as f64).total_cmp(y),
        (Value::Float(x), Value::Int(y)) => x.total_cmp(&(*y as f64)),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        _ => rank(a).cmp(&rank(b)),
    }
}

impl Ord for Key {
    fn cmp(&self, other: &Self) -> Ordering {
        for (a, b) in self.0.iter().zip(other.0.iter()) {
            match compare(a, b) {
                Ordering::Equal => continue,
                o => return o,
            }
        }
        self.0.len().cmp(&other.0.len())
    }
}
impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Key {}

impl Key {
    /// Whether a unique index treats this key as taken: SQL lets any number
    /// of rows share a key with a NULL in it.
    fn binding(&self) -> bool {
        !self.0.iter().any(Value::is_null)
    }

    fn starts_with(&self, prefix: &[Value]) -> bool {
        self.0.len() >= prefix.len() && self.0.iter().zip(prefix).all(|(a, b)| compare(a, b) == Ordering::Equal)
    }
}

#[derive(Clone)]
pub struct Table {
    pub def: Arc<TableDef>,
    /// Row-major; a deleted row leaves None behind until the next load.
    slots: Vec<Option<Box<[Value]>>>,
    live: usize,
    /// Parallel to `def.indexes`: each key once, with the rows holding it.
    indexes: Vec<BTreeMap<Key, Ids>>,
}

impl Table {
    pub fn new(def: Arc<TableDef>) -> Table {
        let indexes = def.indexes.iter().map(|_| BTreeMap::new()).collect();
        Table { def, slots: Vec::new(), live: 0, indexes }
    }

    pub fn name(&self) -> &str {
        &self.def.name
    }

    pub fn len(&self) -> usize {
        self.live
    }

    /// A column's position. Panics on a column the table lacks: the names
    /// are written in the code, so that is a bug for the tests to catch.
    pub fn col(&self, name: &str) -> usize {
        self.def.col(name)
    }

    pub fn row(&self, id: RowId) -> Option<Row<'_>> {
        let cells = self.slots.get(id as usize)?.as_deref()?;
        Some(Row { table: self, id, cells })
    }

    /// Every row, in the order they were made.
    pub fn rows(&self) -> impl Iterator<Item = Row<'_>> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter_map(move |(i, s)| s.as_deref().map(|cells| Row { table: self, id: i as RowId, cells }))
    }

    /// The row with this primary key.
    pub fn get(&self, key: &[Value]) -> Option<Row<'_>> {
        let index = self.def.primary.expect("a primary-key lookup on a table without one");
        self.ids(index, &Key(key.into())).next().and_then(|id| self.row(id))
    }

    /// The row whose single-column primary key is `id`.
    pub fn get_id(&self, id: i64) -> Option<Row<'_>> {
        self.get(&[Value::Int(id)])
    }

    /// The rows whose `columns` equal `values`, through the first full (not
    /// partial) index that starts with those columns (in index order, then
    /// the order they were made), or by a scan when none does.
    pub fn find<'t>(&'t self, columns: &[&str], values: &[Value]) -> Box<dyn Iterator<Item = Row<'t>> + 't> {
        assert_eq!(columns.len(), values.len(), "{}: find with {} columns and {} values", self.name(), columns.len(), values.len());
        let cols: Vec<usize> = columns.iter().map(|c| self.col(c)).collect();
        // SQL `=` never matches NULL.
        if values.iter().any(Value::is_null) {
            return Box::new(std::iter::empty());
        }
        let values: Box<[Value]> = values.iter().zip(&cols).map(|(v, c)| self.def.columns[*c].ty.coerce(v.clone())).collect();
        // A partial index holds only some rows, so it cannot answer a lookup.
        if let Some(i) = self.def.indexes.iter().position(|ix| ix.filter.is_none() && ix.cols.len() >= cols.len() && ix.cols[..cols.len()] == cols[..]) {
            let start = Key(values.clone());
            let ids = self.indexes[i].range(start..).take_while(move |(k, _)| k.starts_with(&values)).flat_map(|(_, ids)| ids.iter());
            return Box::new(ids.filter_map(move |id| self.row(id)));
        }
        Box::new(self.rows().filter(move |r| cols.iter().zip(values.iter()).all(|(c, v)| compare(&r.cells[*c], v) == Ordering::Equal)))
    }

    /// The first row [`Table::find`] finds.
    pub fn find_one(&self, columns: &[&str], values: &[Value]) -> Option<Row<'_>> {
        self.find(columns, values).next()
    }

    /// The rows index `index` holds under exactly `key`, by row id.
    fn ids<'a>(&'a self, index: usize, key: &Key) -> impl Iterator<Item = RowId> + 'a {
        self.indexes[index].get(key).into_iter().flat_map(Ids::iter)
    }

    fn index_add(&mut self, index: usize, key: Key, id: RowId) {
        self.indexes[index].entry(key).or_insert(Ids::Few(Vec::new())).add(id);
    }

    fn index_remove(&mut self, index: usize, key: &Key, id: RowId) {
        if let Some(ids) = self.indexes[index].get_mut(key) {
            if ids.remove(id) {
                self.indexes[index].remove(key);
            }
        }
    }

    fn key(&self, index: usize, cells: &[Value]) -> Key {
        Key(self.def.indexes[index].cols.iter().map(|c| cells[*c].clone()).collect())
    }

    fn indexed(&self, index: usize, cells: &[Value]) -> bool {
        self.def.indexes[index].filter.is_none_or(|f| f(&RowView { def: &self.def, cells }))
    }

    /// The unique index (if any) another row already holds `cells`' key in.
    pub(super) fn conflict(&self, cells: &[Value], except: Option<RowId>) -> Option<(usize, RowId)> {
        for (i, ix) in self.def.indexes.iter().enumerate() {
            if !ix.unique || !self.indexed(i, cells) {
                continue;
            }
            let key = self.key(i, cells);
            if !key.binding() {
                continue;
            }
            if let Some(id) = self.ids(i, &key).find(|id| Some(*id) != except) {
                return Some((i, id));
            }
        }
        None
    }

    /// The row whose unique index `index` holds `cells`' key.
    pub(super) fn holder(&self, index: usize, cells: &[Value]) -> Option<RowId> {
        let key = self.key(index, cells);
        if !key.binding() || !self.indexed(index, cells) {
            return None;
        }
        self.ids(index, &key).next()
    }

    pub(super) fn put(&mut self, id: Option<RowId>, cells: Box<[Value]>) -> RowId {
        let id = match id {
            Some(id) => id,
            None => {
                self.slots.push(None);
                (self.slots.len() - 1) as RowId
            }
        };
        for i in 0..self.indexes.len() {
            if self.indexed(i, &cells) {
                let key = self.key(i, &cells);
                self.index_add(i, key, id);
            }
        }
        debug_assert!(self.slots[id as usize].is_none());
        self.slots[id as usize] = Some(cells);
        self.live += 1;
        id
    }

    pub(super) fn take(&mut self, id: RowId) -> Option<Box<[Value]>> {
        let cells = self.slots.get_mut(id as usize)?.take()?;
        for i in 0..self.indexes.len() {
            if self.indexed(i, &cells) {
                let key = self.key(i, &cells);
                self.index_remove(i, &key, id);
            }
        }
        self.live -= 1;
        Some(cells)
    }

    /// Row `id`'s values replaced by `cells`, re-indexed only where a key
    /// (or a partial index's condition) changed. Returns the old values.
    pub(super) fn replace(&mut self, id: RowId, cells: Box<[Value]>) -> Box<[Value]> {
        let old = self.slots[id as usize].take().expect("replacing a row that exists");
        for i in 0..self.indexes.len() {
            let (was, is) = (self.indexed(i, &old), self.indexed(i, &cells));
            let (old_key, new_key) = (self.key(i, &old), self.key(i, &cells));
            if was && is && old_key == new_key {
                continue;
            }
            if was {
                self.index_remove(i, &old_key, id);
            }
            if is {
                self.index_add(i, new_key, id);
            }
        }
        self.slots[id as usize] = Some(cells);
        old
    }
}

/// The rows one index key is held by, in row order: a short sorted list,
/// or a tree once many rows share the key (a station's orders), so adding
/// and removing one stays quick either way.
#[derive(Clone)]
enum Ids {
    Few(Vec<RowId>),
    Many(BTreeSet<RowId>),
}

/// Past this many rows a key's list becomes a tree.
const FEW: usize = 32;

impl Ids {
    fn iter(&self) -> Box<dyn Iterator<Item = RowId> + '_> {
        match self {
            Ids::Few(v) => Box::new(v.iter().copied()),
            Ids::Many(s) => Box::new(s.iter().copied()),
        }
    }

    fn add(&mut self, id: RowId) {
        match self {
            Ids::Few(v) => {
                if let Err(at) = v.binary_search(&id) {
                    v.insert(at, id);
                }
                if v.len() > FEW {
                    *self = Ids::Many(v.iter().copied().collect());
                }
            }
            Ids::Many(s) => {
                s.insert(id);
            }
        }
    }

    /// Remove `id`; whether none are left.
    fn remove(&mut self, id: RowId) -> bool {
        match self {
            Ids::Few(v) => {
                if let Ok(at) = v.binary_search(&id) {
                    v.remove(at);
                }
                v.is_empty()
            }
            Ids::Many(s) => {
                s.remove(&id);
                s.is_empty()
            }
        }
    }
}

/// A row's values with their table's columns, for checks and filters.
pub struct RowView<'a> {
    pub def: &'a TableDef,
    pub cells: &'a [Value],
}

impl RowView<'_> {
    pub fn value(&self, column: &str) -> &Value {
        &self.cells[self.def.col(column)]
    }
}

/// One row of a live table, with typed getters like the static data's.
#[derive(Clone, Copy)]
pub struct Row<'a> {
    table: &'a Table,
    pub id: RowId,
    cells: &'a [Value],
}

impl<'a> Row<'a> {
    pub fn table(&self) -> &'a Table {
        self.table
    }

    pub fn cells(&self) -> &'a [Value] {
        self.cells
    }

    fn typed(&self, column: &str, types: &[ColType], how: &str) -> &'a Value {
        let col = self.table.col(column);
        let ty = self.table.def.columns[col].ty;
        if !types.contains(&ty) {
            panic!("{}.{column} is {ty:?}; it cannot be {how}", self.table.name());
        }
        &self.cells[col]
    }

    pub fn value(&self, column: &str) -> &'a Value {
        &self.cells[self.table.col(column)]
    }

    /// An integer column. Panics on any other (a `double precision` column
    /// read this way would always be None).
    pub fn int(&self, column: &str) -> Option<i64> {
        self.typed(column, &[ColType::SmallInt, ColType::Integer, ColType::BigInt], "read with int(); use float()").int()
    }

    /// A number column of either kind, as `::float8` gives it.
    pub fn float(&self, column: &str) -> Option<f64> {
        self.typed(column, &[ColType::SmallInt, ColType::Integer, ColType::BigInt, ColType::Real, ColType::Double], "read with float()").float()
    }

    /// A `boolean` column, or an integer flag (non-zero is true).
    pub fn bool(&self, column: &str) -> Option<bool> {
        self.typed(column, &[ColType::Bool, ColType::SmallInt, ColType::Integer, ColType::BigInt], "read with bool()").bool()
    }

    pub fn text(&self, column: &str) -> Option<&'a str> {
        self.typed(column, &[ColType::Text], "read with text()").text()
    }

    /// One column as `svc::rows::cell` gives it from a SELECT of it.
    pub fn cell(&self, column: &str) -> eve_marshal::Value {
        let i = self.table.col(column);
        self.cells[i].to_cell(self.table.def.columns[i].ty.wire())
    }

    /// The named columns as `svc::rows::line` gives a SELECT of them.
    pub fn line(&self, columns: &[&str]) -> Vec<eve_marshal::Value> {
        columns.iter().map(|c| self.cell(c)).collect()
    }
}
