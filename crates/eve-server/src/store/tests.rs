use std::path::PathBuf;

use super::*;

fn int(v: i64) -> Value {
    Value::Int(v)
}

fn text(s: &str) -> Value {
    Value::Text(s.into())
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("trinity-store-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// A ship (itemID 900) and a module fitted to it (901).
fn with_ship(store: &Store) {
    store
        .write(|tx| -> Result<(), StoreError> {
            for (id, loc, flag) in [(900, 60000004, 4), (901, 900, 27)] {
                tx.insert(
                    "entity",
                    &[("itemID", int(id)), ("typeID", int(587)), ("ownerID", int(1)), ("locationID", int(loc)), ("flag", int(flag)), ("quantity", int(1)), ("singleton", int(1))],
                )?;
            }
            Ok(())
        })
        .unwrap();
}

#[test]
fn inserts_take_defaults_and_sequences_and_find_by_index_prefix() {
    let store = Store::in_memory(empty());
    with_ship(&store);
    let id = store.write(|tx| tx.insert("entity", &[("typeID", int(34)), ("ownerID", int(1)), ("locationID", int(900)), ("flag", int(5)), ("quantity", int(10))])).unwrap();
    store.read(|db| {
        let e = db.table("entity");
        let row = e.row(id).unwrap();
        // A sequence default (dyn_id_seq starts at 140000000).
        assert!(row.int("itemID").unwrap() >= 140_000_000);
        assert_eq!(e.find(&["locationID"], &[int(900)]).count(), 2);
        assert_eq!(e.find(&["locationID", "flag"], &[int(900), int(5)]).count(), 1);
        assert_eq!(e.find(&["locationID"], &[Value::Null]).count(), 0);
        assert_eq!(e.get_id(901).unwrap().int("flag"), Some(27));
    });
}

#[test]
fn a_failed_transaction_leaves_no_trace() {
    let store = Store::in_memory(empty());
    with_ship(&store);
    let before = store.read(|db| db.table("entity").len());
    let err = store
        .write(|tx| -> Result<(), StoreError> {
            let id = tx.table("entity").get_id(900).unwrap().id;
            tx.update("entity", id, &[("quantity", int(7))])?;
            tx.insert("entity", &[("itemID", int(902)), ("typeID", int(34)), ("ownerID", int(1)), ("locationID", int(900)), ("flag", int(5)), ("quantity", int(1))])?;
            // The same key again.
            tx.insert("entity", &[("itemID", int(902)), ("typeID", int(34)), ("ownerID", int(1)), ("locationID", int(900)), ("flag", int(5)), ("quantity", int(1))])?;
            Ok(())
        })
        .unwrap_err();
    assert_eq!(err.code, UNIQUE_VIOLATION);
    store.read(|db| {
        let e = db.table("entity");
        assert_eq!(e.len(), before);
        assert_eq!(e.get_id(900).unwrap().int("quantity"), Some(1));
        assert!(e.get_id(902).is_none());
        assert_eq!(e.find(&["locationID"], &[int(900)]).count(), 1);
    });
}

#[test]
fn a_panicking_transaction_is_undone() {
    let store = Store::in_memory(empty());
    with_ship(&store);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = store.write(|tx| -> Result<(), StoreError> {
            let id = tx.table("entity").get_id(900).unwrap().id;
            tx.update("entity", id, &[("quantity", int(7))])?;
            panic!("in the middle");
        });
    }));
    assert!(result.is_err());
    assert_eq!(store.read(|db| db.table("entity").get_id(900).unwrap().int("quantity")), Some(1));
}

#[test]
fn constraints_refuse_what_postgresql_refused() {
    let store = Store::in_memory(empty());
    let not_null = store.write(|tx| tx.insert("corporation_wallets", &[("corporationID", int(1))])).unwrap_err();
    assert_eq!(not_null.code, NOT_NULL_VIOLATION);
    let check = store.write(|tx| tx.insert("corporation_wallets", &[("corporationID", int(1)), ("accountKey", int(1001)), ("balance", Value::Float(-1.0))])).unwrap_err();
    assert_eq!(check.code, CHECK_VIOLATION);
    let range = store.write(|tx| tx.insert("corporation_wallets", &[("corporationID", int(1)), ("accountKey", int(999))])).unwrap_err();
    assert_eq!(range.code, CHECK_VIOLATION);
    let motion: Vec<(&str, Value)> =
        vec![("itemID", int(12345)), ("characterID", int(1)), ("solarSystemID", int(1)), ("tick", int(0)), ("savedAt", int(0)), ("state", text(""))];
    let fk = store.write(|tx| tx.insert("shipSpaceMotion", &motion)).unwrap_err();
    assert_eq!(fk.code, FOREIGN_KEY_VIOLATION);
    let ty = store.write(|tx| tx.insert("corporation_wallets", &[("corporationID", text("x")), ("accountKey", int(1001))])).unwrap_err();
    assert_eq!(ty.code, DATA_EXCEPTION);
}

#[test]
fn deleting_an_item_cascades_and_moving_it_fires_the_triggers() {
    let store = Store::in_memory(empty());
    with_ship(&store);
    let motion = |store: &Store| store.read(|db| db.table("shipSpaceMotion").len());
    let insert_motion = |store: &Store| {
        let cols: Vec<(&str, Value)> = store.read(|db| {
            let def = &db.table("shipSpaceMotion").def;
            def.columns.iter().filter(|c| c.not_null && c.default.is_none()).map(|c| (Box::leak(c.name.to_string().into_boxed_str()) as &str, if c.ty == ColType::Text { text("") } else if c.ty.is_integer() { int(0) } else { Value::Float(0.0) })).collect()
        });
        store
            .write(|tx| {
                let mut sets: Vec<(&str, Value)> = cols.iter().filter(|(n, _)| *n != "itemID").cloned().collect();
                sets.push(("itemID", int(900)));
                tx.insert("shipSpaceMotion", &sets)
            })
            .unwrap();
    };
    insert_motion(&store);
    assert_eq!(motion(&store), 1);
    // Setting a watched column, even to its own value, drops the motion.
    store.write(|tx| { let id = tx.table("entity").get_id(900).unwrap().id; tx.update("entity", id, &[("x", Value::Float(0.0))]) }).unwrap();
    assert_eq!(motion(&store), 0);
    insert_motion(&store);
    // Setting an unwatched column does not.
    store.write(|tx| { let id = tx.table("entity").get_id(900).unwrap().id; tx.update("entity", id, &[("itemName", text("Mine"))]) }).unwrap();
    assert_eq!(motion(&store), 1);
    // Deleting the ship cascades to its motion.
    store.write(|tx| { let id = tx.table("entity").get_id(900).unwrap().id; tx.delete("entity", id) }).unwrap();
    assert_eq!(motion(&store), 0);
}

#[test]
fn alliance_short_names_follow_their_alliance() {
    let store = Store::in_memory(empty());
    let cols: Vec<String> = store.read(|db| db.table("alliances").def.columns.iter().filter(|c| c.not_null && c.default.is_none()).map(|c| c.name.to_string()).collect());
    let sets: Vec<(&str, Value)> = cols.iter().map(|c| (c.as_str(), if c == "shortName" { text("ABC") } else if c == "allianceName" { text("A") } else { int(7) })).collect();
    let mut sets = sets;
    if !sets.iter().any(|(c, _)| *c == "shortName") {
        sets.push(("shortName", text("ABC")));
    }
    if !sets.iter().any(|(c, _)| *c == "allianceID") {
        sets.push(("allianceID", int(7)));
    }
    let id = store.write(|tx| tx.insert("alliances", &sets)).unwrap();
    assert_eq!(store.read(|db| db.table("alliance_shortnames").get_id(7).and_then(|r| r.text("shortName").map(String::from))), Some("ABC".into()));
    store.write(|tx| tx.update("alliances", id, &[("shortName", text("XYZ"))])).unwrap();
    assert_eq!(store.read(|db| db.table("alliance_shortnames").get_id(7).and_then(|r| r.text("shortName").map(String::from))), Some("XYZ".into()));
    store.write(|tx| tx.delete("alliances", id)).unwrap();
    assert_eq!(store.read(|db| db.table("alliance_shortnames").len()), 0);
}

#[test]
fn upsert_resolves_on_a_partial_unique_index() {
    let store = Store::in_memory(empty());
    let order = |seed: bool, price: f64| -> Vec<(&'static str, Value)> {
        vec![
            ("typeID", int(34)), ("charID", int(1000002)), ("regionID", int(1)), ("stationID", int(60000004)), ("range", int(-1)), ("bid", int(0)),
            ("price", Value::Float(price)), ("volEntered", int(1)), ("volRemaining", int(1)), ("issued", int(0)), ("orderState", int(0)), ("minVolume", int(1)),
            ("contraband", int(0)), ("accountID", int(1000)), ("duration", int(365)), ("isCorp", int(1)), ("solarSystemID", int(30000001)), ("escrow", Value::Float(0.0)),
            ("jumps", int(0)), ("starterSeed", Value::Bool(seed)),
        ]
    };
    for price in [5.0, 6.0] {
        store.write(|tx| tx.upsert("market_orders", &["stationID", "typeID", "bid"], &order(true, price), &[("price", Value::Float(price))])).unwrap();
    }
    // A second order of the same station, type and side outside the seed
    // is not held by the partial index.
    store.write(|tx| tx.upsert("market_orders", &["stationID", "typeID", "bid"], &order(false, 9.0), &[("price", Value::Float(9.0))])).unwrap();
    store.read(|db| {
        let orders: Vec<f64> = db.table("market_orders").rows().map(|r| r.float("price").unwrap()).collect();
        assert_eq!(orders, vec![6.0, 9.0]);
        // A lookup finds rows outside a partial index too.
        assert_eq!(db.table("market_orders").find(&["stationID", "typeID", "bid"], &[int(60000004), int(34), int(0)]).count(), 2);
    });
}

#[test]
fn committed_data_survives_a_restart_and_a_torn_log_line() {
    let dir = temp_dir("restart");
    {
        let store = Store::open(&dir, || Ok(empty())).unwrap();
        with_ship(&store);
        store.write(|tx| { let id = tx.table("entity").get_id(901).unwrap().id; tx.update("entity", id, &[("flag", int(28))]) }).unwrap();
        store.write(|tx| { let id = tx.table("entity").get_id(900).unwrap().id; tx.update("entity", id, &[("itemName", text("tab\there \"quoted\" ünicode"))]) }).unwrap();
        store.write(|tx| tx.nextval("dyn_id_seq")).unwrap();
        store.sync();
    }
    // A crash in the middle of writing a line.
    let log = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).find(|p| p.extension().is_some_and(|e| e == "log")).unwrap();
    std::fs::OpenOptions::new().append(true).open(&log).unwrap().write_all(b"{\"c\":[[\"D\",\"entity\"").unwrap();
    let store = Store::open(&dir, || panic!("the data is there")).unwrap();
    store.read(|db| {
        let e = db.table("entity");
        assert_eq!(e.len(), 2);
        assert_eq!(e.get_id(901).unwrap().int("flag"), Some(28));
        assert_eq!(e.get_id(900).unwrap().text("itemName"), Some("tab\there \"quoted\" ünicode"));
        assert_eq!(db.sequence("dyn_id_seq"), Some(140_000_000));
    });
    // The torn line is gone and new commits follow on lines of their own.
    store.write(|tx| { let id = tx.table("entity").get_id(900).unwrap().id; tx.update("entity", id, &[("flag", int(5))]) }).unwrap();
    store.sync();
    drop(store);
    let store = Store::open(&dir, || panic!("the data is there")).unwrap();
    assert_eq!(store.read(|db| db.table("entity").get_id(900).unwrap().int("flag")), Some(5));
    // And again, after a snapshot and more changes.
    store.checkpoint().unwrap();
    store.write(|tx| { let id = tx.table("entity").get_id(901).unwrap().id; tx.delete("entity", id) }).unwrap();
    store.sync();
    drop(store);
    let store = Store::open(&dir, || panic!("the data is there")).unwrap();
    assert_eq!(store.read(|db| db.table("entity").len()), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_fresh_world_loads_from_the_dumps() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../db");
    let mut dumps = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.to_string_lossy().ends_with(".sql.gz") {
            dumps.push(read_dump(&p).unwrap());
        }
    }
    let rss = || std::fs::read_to_string("/proc/self/status").ok().and_then(|s| s.lines().find(|l| l.starts_with("VmRSS")).map(String::from)).unwrap_or_default();
    let before = rss();
    let started = std::time::Instant::now();
    let db = from_dumps(&dumps).unwrap();
    let rows: usize = db.tables.iter().map(|t| t.len()).sum();
    eprintln!("fresh world: {rows} rows in {:?}; {before} -> {}", started.elapsed(), rss());
    let mut big: Vec<(usize, &str)> = db.tables.iter().map(|t| (t.len(), t.name())).collect();
    big.sort_unstable_by(|a, b| b.cmp(a));
    eprintln!("largest: {:?}", &big[..6]);
    assert!(db.table("entity").len() > 1000);
    assert!(db.table("mapSolarSystems").get_id(30000142).is_some());
    assert!(db.sequence("dyn_id_seq").is_some());
    // A column added after the dump was cut is numbered from its sequence,
    // as PostgreSQL numbered the rows when the column was added.
    let properties = db.table("petitionCategoryProperties");
    let ids: std::collections::HashSet<i64> = properties.rows().filter_map(|r| r.int("propertyID")).collect();
    assert!(properties.len() > 0);
    assert_eq!(ids.len(), properties.len());
}

use std::io::Write as _;

