//! One static table held in memory: its columns with their SQL types, its rows,
//! and indexes on integer columns built the first time a lookup needs one.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::copy;

/// A column's type as the dump declares it, which decides how the client
/// sees its values (see [`Value::to_cell`] and [`Value::to_bulk`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColType {
    SmallInt,
    Integer,
    BigInt,
    Double,
    Bool,
    Text,
}

impl ColType {
    pub(super) fn parse(sql: &str) -> Option<ColType> {
        Some(match sql {
            "smallint" => ColType::SmallInt,
            "integer" => ColType::Integer,
            "bigint" => ColType::BigInt,
            "double precision" => ColType::Double,
            "boolean" => ColType::Bool,
            "text" => ColType::Text,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(Box<str>),
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// An integer, or a boolean as 0/1.
    pub fn int(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    /// A number of either kind, as `::float8` would give it.
    pub fn float(&self) -> Option<f64> {
        match self {
            Value::Float(v) => Some(*v),
            Value::Int(v) => Some(*v as f64),
            _ => None,
        }
    }

    pub fn bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Int(v) => Some(*v != 0),
            _ => None,
        }
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }

    /// As `svc::rows::cell` gives the same column read out of PostgreSQL:
    /// what service calls send.
    pub fn to_cell(&self, ty: ColType) -> eve_marshal::Value {
        use eve_marshal::Value as M;
        match (self, ty) {
            (Value::Null, _) => M::None,
            (Value::Int(v), ColType::BigInt) => M::Long(*v),
            (Value::Int(v), _) => M::Int(*v),
            (Value::Bool(b), _) => M::Bool(*b),
            (Value::Float(v), _) => M::Double(*v),
            (Value::Text(s), _) => M::text(&**s),
        }
    }

    /// As `bulkdata` gives the same column read out of PostgreSQL: booleans
    /// as 0/1 and text always as bytes.
    pub fn to_bulk(&self, ty: ColType) -> eve_marshal::Value {
        use eve_marshal::Value as M;
        match (self, ty) {
            (Value::Bool(b), _) => M::Int(*b as i64),
            (Value::Text(s), _) => M::Bytes(s.as_bytes().to_vec()),
            _ => self.to_cell(ty),
        }
    }
}

pub struct Column {
    pub name: Box<str>,
    pub ty: ColType,
}

pub struct StaticTable {
    pub name: &'static str,
    pub columns: Vec<Column>,
    /// Row-major: row `i` is `cells[i * width .. (i + 1) * width]`.
    cells: Vec<Value>,
    width: usize,
    /// Per column: each integer value's rows, in dump order.
    indexes: Vec<OnceLock<HashMap<i64, Vec<u32>>>>,
}

impl StaticTable {
    pub(super) fn read(name: &'static str, gz: &[u8]) -> Result<StaticTable, String> {
        let t = copy::Table::from_gz(name, gz)?;
        let columns: Vec<Column> = t
            .columns()
            .iter()
            .zip(t.types())
            .map(|(n, ty)| Column { name: n.as_str().into(), ty: *ty })
            .collect();
        let width = columns.len();
        let mut cells = Vec::new();
        for row in t.rows() {
            for (i, c) in columns.iter().enumerate() {
                cells.push(match c.ty {
                    ColType::SmallInt | ColType::Integer | ColType::BigInt => row.opt_int(i)?.map_or(Value::Null, Value::Int),
                    ColType::Double => row.opt_float(i)?.map_or(Value::Null, Value::Float),
                    ColType::Bool => row.opt_bool(i)?.map_or(Value::Null, Value::Bool),
                    ColType::Text => row.opt_text(i)?.map_or(Value::Null, |s| Value::Text(s.into())),
                });
            }
        }
        cells.shrink_to_fit();
        let indexes = (0..width).map(|_| OnceLock::new()).collect();
        Ok(StaticTable { name, columns, cells, width, indexes })
    }

    pub fn len(&self) -> usize {
        if self.width == 0 { 0 } else { self.cells.len() / self.width }
    }

    /// A column's position. Panics on a column the table lacks: the names
    /// are written in the code, so that is a bug for the tests to catch.
    pub fn col(&self, name: &str) -> usize {
        self.columns
            .iter()
            .position(|c| &*c.name == name)
            .unwrap_or_else(|| panic!("static table {} has no column {name:?}", self.name))
    }

    pub fn row(&self, i: usize) -> Row<'_> {
        Row { table: self, cells: &self.cells[i * self.width..(i + 1) * self.width] }
    }

    /// Every row, in dump order.
    pub fn rows(&self) -> impl Iterator<Item = Row<'_>> + '_ {
        (0..self.len()).map(move |i| self.row(i))
    }

    /// The rows whose integer column `column` equals `key`, in dump order.
    /// The first lookup on a column indexes it.
    pub fn find(&self, column: &str, key: i64) -> impl Iterator<Item = Row<'_>> + '_ {
        let col = self.col(column);
        self.expect(col, &[ColType::SmallInt, ColType::Integer, ColType::BigInt], "find by");
        let index = self.index(col);
        index.get(&key).map_or(&[][..], Vec::as_slice).iter().map(move |i| self.row(*i as usize))
    }

    /// The first row whose integer column `column` equals `key`: the row by
    /// its key, on a key column.
    pub fn get(&self, column: &str, key: i64) -> Option<Row<'_>> {
        self.find(column, key).next()
    }

    /// Panics when column `col` is not one of `types`: reading a `double
    /// precision` column as an integer, say, would quietly find nothing.
    fn expect(&self, col: usize, types: &[ColType], how: &str) {
        let ty = self.columns[col].ty;
        if !types.contains(&ty) {
            panic!("{}.{} is {ty:?}; it cannot be {how} that way", self.name, self.columns[col].name);
        }
    }

    fn index(&self, col: usize) -> &HashMap<i64, Vec<u32>> {
        self.indexes[col].get_or_init(|| {
            let mut index: HashMap<i64, Vec<u32>> = HashMap::new();
            for i in 0..self.len() {
                if let Some(k) = self.cells[i * self.width + col].int() {
                    index.entry(k).or_default().push(i as u32);
                }
            }
            index
        })
    }

    /// A column's values set to `f(row)` for the rows `f` answers for; for
    /// fix-ups made once while the server starts, before anything reads.
    pub(super) fn update(&mut self, column: &str, mut f: impl FnMut(Row<'_>) -> Option<Value>) {
        let col = self.col(column);
        let updates: Vec<(usize, Value)> = (0..self.len()).filter_map(|i| f(self.row(i)).map(|v| (i, v))).collect();
        for (i, v) in updates {
            self.cells[i * self.width + col] = v;
        }
        self.indexes[col] = OnceLock::new();
    }
}

#[derive(Clone, Copy)]
pub struct Row<'a> {
    table: &'a StaticTable,
    cells: &'a [Value],
}

impl<'a> Row<'a> {
    pub fn table(&self) -> &'a StaticTable {
        self.table
    }

    pub fn value(&self, column: &str) -> &'a Value {
        &self.cells[self.table.col(column)]
    }

    fn typed(&self, column: &str, types: &[ColType], how: &str) -> &'a Value {
        let col = self.table.col(column);
        self.table.expect(col, types, how);
        &self.cells[col]
    }

    /// An integer column (`smallint`, `integer`, `bigint`). Panics on any
    /// other: a `double precision` column read this way would always be None.
    pub fn int(&self, column: &str) -> Option<i64> {
        self.typed(column, &[ColType::SmallInt, ColType::Integer, ColType::BigInt], "read with int(); use float()").int()
    }

    /// A number column of either kind, as `::float8` gives it.
    pub fn float(&self, column: &str) -> Option<f64> {
        self.typed(column, &[ColType::SmallInt, ColType::Integer, ColType::BigInt, ColType::Double], "read with float()").float()
    }

    /// A `boolean` column, or an integer flag (non-zero is true).
    pub fn bool(&self, column: &str) -> Option<bool> {
        self.typed(column, &[ColType::Bool, ColType::SmallInt, ColType::Integer, ColType::BigInt], "read with bool()").bool()
    }

    pub fn text(&self, column: &str) -> Option<&'a str> {
        self.typed(column, &[ColType::Text], "read with text()").text()
    }

    /// The named columns as `svc::rows::line` gives a `SELECT` of them:
    /// a line for a service call's rowset.
    pub fn line(&self, columns: &[&str]) -> Vec<eve_marshal::Value> {
        columns.iter().map(|c| self.cell(c)).collect()
    }

    /// One column as `svc::rows::cell` gives it.
    pub fn cell(&self, column: &str) -> eve_marshal::Value {
        let i = self.table.col(column);
        self.cells[i].to_cell(self.table.columns[i].ty)
    }

    /// The named columns as bulkdata reads them.
    pub fn bulk_line(&self, columns: &[&str]) -> Vec<eve_marshal::Value> {
        columns
            .iter()
            .map(|c| {
                let i = self.table.col(c);
                self.cells[i].to_bulk(self.table.columns[i].ty)
            })
            .collect()
    }
}
