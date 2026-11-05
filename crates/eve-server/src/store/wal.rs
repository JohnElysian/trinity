//! Keeping the live data on disk: a snapshot of every table, and a
//! write-ahead log of the transactions committed since it.
//!
//! The data directory holds `snapshot.<gen>.json.gz` and `wal.<gen>.log`.
//! A snapshot of generation `g` is the data as it was when `wal.<g>.log` was
//! begun, so the data now is the newest complete snapshot with its log, and
//! the logs of any later generations, replayed in order.
//!
//! Each line of a log is one committed transaction, as JSON:
//! `{"c":[change...],"s":{sequence:value...}}`, a change being
//! `["I",table,row]`, `["u",table,key,[[column,value]...]]` (the columns
//! that changed, by position), or `["D",table,key]` (`key` being the row's
//! primary key, or the whole row in a table without one). Older logs may
//! hold `["U",table,key,row]`, a whole new row.
//! Commits are queued for a writer thread that flushes and syncs them in
//! batches, so a crash loses at most the transactions it had not synced
//! yet: never part of one, since a torn last line is ignored.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde_json::{json, Value as Json};

use super::schema::{ColType, TableDef};
use super::table::RowId;
use super::tx::Db;
use super::Value;

/// One change a transaction made, as the log records it.
pub enum Change {
    Insert { table: usize, row: Box<[Value]> },
    Update { table: usize, old: Box<[Value]>, row: Box<[Value]> },
    Delete { table: usize, old: Box<[Value]> },
}

fn encode_value(v: &Value) -> Json {
    match v {
        Value::Null => Json::Null,
        Value::Int(i) => json!(i),
        Value::Float(f) if f.is_finite() => json!(f),
        Value::Float(f) if f.is_nan() => json!("NaN"),
        Value::Float(f) => json!(if *f > 0.0 { "Infinity" } else { "-Infinity" }),
        Value::Bool(b) => json!(b),
        Value::Text(s) => json!(&**s),
    }
}

fn decode_value(j: &Json, ty: ColType) -> Result<Value, String> {
    Ok(match (j, ty) {
        (Json::Null, _) => Value::Null,
        (Json::Number(n), t) if t.is_integer() => Value::Int(n.as_i64().ok_or_else(|| format!("{n} is not an integer"))?),
        (Json::Number(n), ColType::Real | ColType::Double) => Value::Float(n.as_f64().ok_or_else(|| format!("{n} is not a number"))?),
        (Json::String(s), ColType::Real | ColType::Double) => Value::Float(match s.as_str() {
            "NaN" => f64::NAN,
            "Infinity" => f64::INFINITY,
            "-Infinity" => f64::NEG_INFINITY,
            _ => return Err(format!("{s:?} is not a number")),
        }),
        (Json::Bool(b), ColType::Bool) => Value::Bool(*b),
        (Json::String(s), ColType::Text) => Value::Text(s.as_str().into()),
        (j, t) => return Err(format!("{j} does not fit a {t:?} column")),
    })
}

fn encode_row(row: &[Value]) -> Json {
    Json::Array(row.iter().map(encode_value).collect())
}

fn decode_row(def: &TableDef, j: &Json) -> Result<Box<[Value]>, String> {
    let cells = j.as_array().ok_or("a row is not an array")?;
    if cells.len() != def.columns.len() {
        return Err(format!("{}: a row of {} values for {} columns", def.name, cells.len(), def.columns.len()));
    }
    cells.iter().zip(&def.columns).map(|(v, c)| decode_value(v, c.ty).map_err(|e| format!("{}.{}: {e}", def.name, c.name))).collect()
}

fn identity(def: &TableDef, row: &[Value]) -> Json {
    Json::Array(def.identity().into_iter().map(|c| encode_value(&row[c])).collect())
}

/// One committed transaction as a log line.
pub(super) fn encode_commit(db: &Db, changes: &[Change], sequences: bool) -> Vec<u8> {
    let c: Vec<Json> = changes
        .iter()
        .map(|ch| match ch {
            Change::Insert { table, row } => json!(["I", &*db.tables[*table].def.name, encode_row(row)]),
            // Only the columns that changed: a bot moving rewrites two of
            // its character's fifty.
            Change::Update { table, old, row } => {
                let def = &db.tables[*table].def;
                let changed: Vec<Json> = old
                    .iter()
                    .zip(row.iter())
                    .enumerate()
                    .filter(|(_, (a, b))| a != b)
                    .map(|(c, (_, b))| json!([c, encode_value(b)]))
                    .collect();
                json!(["u", &*def.name, identity(def, old), changed])
            }
            Change::Delete { table, old } => {
                let def = &db.tables[*table].def;
                json!(["D", &*def.name, identity(def, old)])
            }
        })
        .collect();
    let mut line = json!({ "c": c });
    if sequences {
        line["s"] = sequences_json(db);
    }
    let mut bytes = serde_json::to_vec(&line).expect("JSON values always serialise");
    bytes.push(b'\n');
    bytes
}

fn sequences_json(db: &Db) -> Json {
    Json::Object(db.sequences.iter().filter_map(|(k, v)| v.map(|v| (k.to_string(), json!(v)))).collect())
}

fn find_by_identity(db: &Db, t: usize, key: &Json) -> Result<RowId, String> {
    let def = &db.tables[t].def;
    let cols = def.identity();
    let key = key.as_array().ok_or("a key is not an array")?;
    let values: Vec<Value> = cols.iter().zip(key).map(|(c, v)| decode_value(v, def.columns[*c].ty)).collect::<Result<_, _>>()?;
    let found = if def.primary.is_some() {
        db.tables[t].get(&values).map(|r| r.id)
    } else {
        // A table without a key is matched on every column, NULLs included.
        db.tables[t]
            .rows()
            .find(|r| r.cells().iter().zip(&values).all(|(a, b)| a.is_null() == b.is_null() && super::table::compare(a, b).is_eq()))
            .map(|r| r.id)
    };
    found.ok_or_else(|| format!("{}: no row {key:?} to change", def.name))
}

/// Apply one log line to `db`, as the transaction that wrote it did.
fn replay(db: &mut Db, line: &Json) -> Result<(), String> {
    for ch in line["c"].as_array().ok_or("a commit without changes")? {
        let op = ch[0].as_str().ok_or("a change without an operation")?;
        let t = db.index_of(ch[1].as_str().ok_or("a change without a table")?);
        let def = db.tables[t].def.clone();
        match op {
            "I" => {
                db.table_mut(t).put(None, decode_row(&def, &ch[2])?);
            }
            "U" => {
                let id = find_by_identity(db, t, &ch[2])?;
                db.table_mut(t).replace(id, decode_row(&def, &ch[3])?);
            }
            "u" => {
                let id = find_by_identity(db, t, &ch[2])?;
                let mut cells: Vec<Value> = db.tables[t].row(id).ok_or("a changed row is gone")?.cells().to_vec();
                for change in ch[3].as_array().ok_or("an update without changes")? {
                    let c = change[0].as_u64().ok_or("a change without a column")? as usize;
                    let col = def.columns.get(c).ok_or("a change to a column the table lacks")?;
                    cells[c] = decode_value(&change[1], col.ty).map_err(|e| format!("{}.{}: {e}", def.name, col.name))?;
                }
                db.table_mut(t).replace(id, cells.into());
            }
            "D" => {
                let id = find_by_identity(db, t, &ch[2])?;
                db.table_mut(t).take(id);
            }
            o => return Err(format!("unknown change {o:?}")),
        }
    }
    if let Some(s) = line.get("s").and_then(Json::as_object) {
        for (k, v) in s {
            if let Some(slot) = db.sequences.get_mut(k.as_str()) {
                *slot = v.as_i64();
            }
        }
    }
    Ok(())
}

fn snapshot_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("snapshot.{generation}.json.gz"))
}

fn log_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("wal.{generation}.log"))
}

/// The generations of the snapshots and logs in `dir`.
fn generations(dir: &Path) -> std::io::Result<(Vec<u64>, Vec<u64>)> {
    let mut snapshots = Vec::new();
    let mut logs = Vec::new();
    for e in fs::read_dir(dir)? {
        let name = e?.file_name().to_string_lossy().into_owned();
        if let Some(g) = name.strip_prefix("snapshot.").and_then(|n| n.strip_suffix(".json.gz")).and_then(|n| n.parse().ok()) {
            snapshots.push(g);
        } else if let Some(g) = name.strip_prefix("wal.").and_then(|n| n.strip_suffix(".log")).and_then(|n| n.parse().ok()) {
            logs.push(g);
        }
    }
    snapshots.sort_unstable();
    logs.sort_unstable();
    Ok((snapshots, logs))
}

/// Write a snapshot of `db` as generation `generation`: to a temporary file,
/// synced, then renamed into place, so a snapshot that exists is whole.
pub(super) fn write_snapshot(dir: &Path, generation: u64, db: &Db) -> Result<(), String> {
    let path = snapshot_path(dir, generation);
    let tmp = path.with_extension("tmp");
    let file = File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let mut out = BufWriter::new(GzEncoder::new(file, flate2::Compression::fast()));
    let io = |e: std::io::Error| format!("{}: {e}", tmp.display());
    serde_json::to_writer(&mut out, &json!({ "sequences": sequences_json(db) })).map_err(|e| e.to_string())?;
    out.write_all(b"\n").map_err(io)?;
    for t in &db.tables {
        serde_json::to_writer(&mut out, &json!({ "table": &*t.def.name, "rows": t.len() })).map_err(|e| e.to_string())?;
        out.write_all(b"\n").map_err(io)?;
        for r in t.rows() {
            serde_json::to_writer(&mut out, &encode_row(r.cells())).map_err(|e| e.to_string())?;
            out.write_all(b"\n").map_err(io)?;
        }
    }
    let file = out.into_inner().map_err(|e| io(e.into_error()))?.finish().map_err(io)?;
    file.sync_all().map_err(io)?;
    fs::rename(&tmp, &path).map_err(io)?;
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

fn read_snapshot(path: &Path, db: &mut Db) -> Result<(), String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut lines = BufReader::new(GzDecoder::new(file)).lines();
    let mut next = || -> Result<Json, String> {
        let line = lines.next().ok_or("the snapshot ends early")?.map_err(|e| e.to_string())?;
        serde_json::from_str(&line).map_err(|e| e.to_string())
    };
    let head = next()?;
    for (k, v) in head["sequences"].as_object().ok_or("a snapshot without sequences")? {
        if let Some(slot) = db.sequences.get_mut(k.as_str()) {
            *slot = v.as_i64();
        }
    }
    for _ in 0..db.tables.len() {
        let head = next()?;
        let name = head["table"].as_str().ok_or("a table without a name")?;
        let rows = head["rows"].as_u64().ok_or("a table without a row count")?;
        // A table the schema no longer has is skipped; one it gained since
        // starts empty.
        let t = db.tables.iter().position(|t| &*t.def.name == name);
        for _ in 0..rows {
            let row = next()?;
            if let Some(t) = t {
                let def = db.tables[t].def.clone();
                db.table_mut(t).put(None, decode_row(&def, &row)?);
            }
        }
    }
    Ok(())
}

fn replay_log(path: &Path, db: &mut Db) -> Result<usize, String> {
    let mut text = String::new();
    File::open(path).and_then(|mut f| f.read_to_string(&mut text)).map_err(|e| format!("{}: {e}", path.display()))?;
    let complete = text.rfind('\n').map_or(0, |i| i + 1);
    let mut n = 0;
    for (i, line) in text[..complete].lines().enumerate() {
        let j: Json = serde_json::from_str(line).map_err(|e| format!("{} line {}: {e}", path.display(), i + 1))?;
        replay(db, &j).map_err(|e| format!("{} line {}: {e}", path.display(), i + 1))?;
        n += 1;
    }
    Ok(n)
}

/// The data on disk in `dir`: the newest snapshot with the logs since, and
/// the generation they reach; None when `dir` holds no snapshot yet.
pub(super) fn recover(dir: &Path, empty: Db) -> Result<Option<(Db, u64, usize)>, String> {
    let (snapshots, logs) = generations(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let Some(&base) = snapshots.last() else { return Ok(None) };
    let mut db = empty;
    read_snapshot(&snapshot_path(dir, base), &mut db)?;
    let mut replayed = 0;
    let mut last = base;
    for g in logs.into_iter().filter(|g| *g >= base) {
        replayed += replay_log(&log_path(dir, g), &mut db)?;
        last = g;
    }
    Ok(Some((db, last, replayed)))
}

/// Remove the snapshots and logs older than generation `keep`.
pub(super) fn prune(dir: &Path, keep: u64) {
    if let Ok((snapshots, logs)) = generations(dir) {
        for g in snapshots.into_iter().filter(|g| *g < keep) {
            let _ = fs::remove_file(snapshot_path(dir, g));
        }
        for g in logs.into_iter().filter(|g| *g < keep) {
            let _ = fs::remove_file(log_path(dir, g));
        }
    }
}

enum Msg {
    Commit(Vec<u8>),
    /// Begin the log of a new generation; later commits go there.
    Rotate(u64),
    /// Answer once everything sent before is synced.
    Sync(mpsc::Sender<()>),
}

/// The writer thread's end of the log.
pub(super) struct Log {
    tx: mpsc::Sender<Msg>,
}

impl Log {
    pub(super) fn open(dir: Arc<PathBuf>, generation: u64) -> Result<Log, String> {
        let path = log_path(&dir, generation);
        // A crash may have left half a line at the end: cut it off, so what
        // is appended now starts on a line of its own.
        if let Ok(bytes) = fs::read(&path) {
            let whole = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            if whole < bytes.len() {
                OpenOptions::new().write(true).open(&path).and_then(|f| f.set_len(whole as u64)).map_err(|e| format!("{}: {e}", path.display()))?;
            }
        }
        let mut file = BufWriter::new(OpenOptions::new().create(true).append(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?);
        let (tx, rx) = mpsc::channel::<Msg>();
        std::thread::Builder::new()
            .name("store-log".into())
            .spawn(move || {
                let fail = |e: std::io::Error| -> ! {
                    // Committed data that cannot be written must not be
                    // acknowledged as kept: stop the server.
                    tracing::error!("writing the store's log in {}: {e}", dir.display());
                    std::process::abort();
                };
                let mut waiting: Vec<mpsc::Sender<()>> = Vec::new();
                while let Ok(first) = rx.recv() {
                    let mut msg = Some(first);
                    loop {
                        match msg.take() {
                            Some(Msg::Commit(bytes)) => file.write_all(&bytes).unwrap_or_else(|e| fail(e)),
                            Some(Msg::Rotate(g)) => {
                                file.flush().unwrap_or_else(|e| fail(e));
                                file.get_ref().sync_data().unwrap_or_else(|e| fail(e));
                                let path = log_path(&dir, g);
                                file = BufWriter::new(OpenOptions::new().create(true).append(true).open(&path).unwrap_or_else(|e| fail(e)));
                            }
                            Some(Msg::Sync(done)) => waiting.push(done),
                            None => {}
                        }
                        match rx.try_recv() {
                            Ok(m) => msg = Some(m),
                            Err(_) => break,
                        }
                    }
                    file.flush().unwrap_or_else(|e| fail(e));
                    file.get_ref().sync_data().unwrap_or_else(|e| fail(e));
                    for w in waiting.drain(..) {
                        let _ = w.send(());
                    }
                }
            })
            .map_err(|e| format!("starting the store's log writer: {e}"))?;
        Ok(Log { tx })
    }

    pub(super) fn commit(&self, line: Vec<u8>) {
        if self.tx.send(Msg::Commit(line)).is_err() {
            tracing::error!("the store's log writer has stopped");
            std::process::abort();
        }
    }

    pub(super) fn rotate(&self, generation: u64) {
        let _ = self.tx.send(Msg::Rotate(generation));
    }

    /// Wait until everything committed so far is on disk.
    pub(super) fn sync(&self) {
        let (done, wait) = mpsc::channel();
        if self.tx.send(Msg::Sync(done)).is_ok() {
            let _ = wait.recv();
        }
    }
}
