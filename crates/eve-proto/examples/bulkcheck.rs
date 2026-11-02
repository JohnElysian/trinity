//! Decode a client's own `bulkdata/*.cache` files with this crate's marshal
//! codec, re-encode each payload and decode it again, as a check that the
//! codec reads and writes what the client does (DBRows, shared descriptors,
//! reduces).
//!
//! usage: cargo run -p eve-proto --example bulkcheck -- <client bulkdata dir> <strings.txt>

use std::io::Read;

use eve_marshal::{load, save, StringTable, Value};

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("bulkdata directory");
    let table = StringTable::parse(&std::fs::read(args.next().expect("strings.txt")).expect("strings"));
    let mut failures = 0;
    let mut entries: Vec<_> = std::fs::read_dir(&dir).expect("dir").flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("cache") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        match check(&std::fs::read(&path).expect("read"), &table) {
            Ok(summary) => println!("ok   {name}: {summary}"),
            Err(e) => {
                failures += 1;
                println!("FAIL {name}: {e}");
            }
        }
    }
    std::process::exit(if failures == 0 { 0 } else { 1 });
}

fn check(raw: &[u8], table: &StringTable) -> Result<String, String> {
    let outer = load(raw, table).map_err(|e| format!("outer: {e}"))?;
    let items = outer.seq().ok_or("outer is not (key, CachedObject)")?;
    let key = items.first().and_then(|k| k.as_bytes()).map(String::from_utf8_lossy).unwrap_or_default();
    let Some(Value::Instance { state, .. }) = items.get(1) else { return Err("no CachedObject".into()) };
    let fields = state.seq().ok_or("CachedObject state")?;
    let blob = fields.iter().find_map(|f| match f {
        Value::Bytes(b) if b.len() > 2 && (b[0] == 0x78 || b[0] == b'~') => Some(b.clone()),
        _ => None,
    });
    let blob = blob.ok_or("no payload in the CachedObject")?;
    let inner = if blob[0] == 0x78 {
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(&blob[..]).read_to_end(&mut out).map_err(|e| e.to_string())?;
        out
    } else {
        blob
    };
    let value = load(&inner, table).map_err(|e| format!("inner: {e}"))?;
    let again = load(&save(&value, table).map_err(|e| format!("re-encode: {e}"))?, table)
        .map_err(|e| format!("re-decode: {e}"))?;
    if again != value {
        return Err("re-encoded payload decodes differently".into());
    }
    let rows = count_rows(&value);
    Ok(format!("{key} ({} -> {} bytes, {rows} rows)", inner.len(), save(&value, table).unwrap().len()))
}

fn count_rows(v: &Value) -> usize {
    match v {
        Value::List(items) => items.len(),
        Value::Tuple(items) if items.len() == 2 => count_rows(&items[1]),
        Value::Instance { state, .. } => state.get_str("lines").map_or(0, count_rows),
        _ => 0,
    }
}
