use crate::dbrow::DbRowDescriptor;
use crate::error::{Error, Result};
use crate::strings::StringTable;
use crate::tags;
use crate::value::Value;
use crate::{MAGIC, MAX_DEPTH};

pub struct Encoder<'a> {
    out: Vec<u8>,
    table: &'a StringTable,
    /// The shared-object map written after the data: entry k is the slot of
    /// the k-th object flagged shared.
    shared_map: Vec<u32>,
    /// DBRow descriptors already written in this stream, by slot.
    descriptors: Vec<(std::sync::Arc<DbRowDescriptor>, u32)>,
}

impl<'a> Encoder<'a> {
    pub fn new(table: &'a StringTable) -> Self {
        Encoder { out: vec![MAGIC, 0, 0, 0, 0], table, shared_map: Vec::new(), descriptors: Vec::new() }
    }

    pub fn finish(mut self) -> Vec<u8> {
        if !self.shared_map.is_empty() {
            let n = self.shared_map.len() as u32;
            self.out[1..5].copy_from_slice(&n.to_le_bytes());
            for slot in &self.shared_map {
                self.out.extend_from_slice(&slot.to_le_bytes());
            }
        }
        self.out
    }

    /// A row's descriptor: in full the first time this stream meets it, as a
    /// reference to that after. Every row of a rowset repeats it otherwise.
    fn write_descriptor(&mut self, d: &std::sync::Arc<DbRowDescriptor>, depth: u32) -> Result<()> {
        let known = self
            .descriptors
            .iter()
            .find(|(seen, _)| std::sync::Arc::ptr_eq(seen, d) || **seen == **d)
            .map(|(_, slot)| *slot);
        if let Some(slot) = known {
            self.u8(tags::REFERENCE);
            self.extsize(slot as usize);
            return Ok(());
        }
        let slot = self.shared_map.len() as u32 + 1;
        self.shared_map.push(slot);
        self.descriptors.push((std::sync::Arc::clone(d), slot));
        let mark = self.out.len();
        self.write_at(&d.to_value(), depth + 1)?;
        self.out[mark] |= tags::SHARED_FLAG;
        Ok(())
    }

    fn write_iterators(&mut self, items: &[Value], pairs: &[(Value, Value)], depth: u32) -> Result<()> {
        for it in items {
            self.write_at(it, depth + 1)?;
        }
        self.u8(tags::MARKER);
        for (k, v) in pairs {
            self.write_at(k, depth + 1)?;
            self.write_at(v, depth + 1)?;
        }
        self.u8(tags::MARKER);
        Ok(())
    }

    fn u8(&mut self, b: u8) {
        self.out.push(b);
    }

    fn raw(&mut self, b: &[u8]) {
        self.out.extend_from_slice(b);
    }

    fn extsize(&mut self, n: usize) {
        if n < 0xFF {
            self.out.push(n as u8);
        } else {
            self.out.push(0xFF);
            self.out.extend_from_slice(&(n as u32).to_le_bytes());
        }
    }

    pub fn write(&mut self, v: &Value) -> Result<()> {
        self.write_at(v, 0)
    }

    fn write_at(&mut self, v: &Value, depth: u32) -> Result<()> {
        if depth > MAX_DEPTH {
            return Err(Error::DepthLimit);
        }
        match v {
            Value::None => self.u8(tags::NONE),
            Value::Bool(true) => self.u8(tags::TRUE),
            Value::Bool(false) => self.u8(tags::FALSE),
            Value::Marker => self.u8(tags::MARKER),

            Value::Int(i) => match *i {
                -1 => self.u8(tags::INT_MINUS1),
                0 => self.u8(tags::INT_0),
                1 => self.u8(tags::INT_1),
                v if (-128..=127).contains(&v) => {
                    self.u8(tags::INT8);
                    self.u8(v as i8 as u8);
                }
                v if (-32768..=32767).contains(&v) => {
                    self.u8(tags::INT16);
                    self.raw(&(v as i16).to_le_bytes());
                }
                v if (i32::MIN as i64..=i32::MAX as i64).contains(&v) => {
                    self.u8(tags::INT32);
                    self.raw(&(v as i32).to_le_bytes());
                }
                v => {
                    self.u8(tags::INT64);
                    self.raw(&v.to_le_bytes());
                }
            },
            Value::Long(i) => {
                self.u8(tags::INT64);
                self.raw(&i.to_le_bytes());
            }

            Value::Double(d) => {
                if *d == 0.0 && d.is_sign_positive() {
                    self.u8(tags::DOUBLE_0);
                } else {
                    self.u8(tags::DOUBLE);
                    self.raw(&d.to_le_bytes());
                }
            }

            Value::Bytes(b) => self.write_str(b),
            Value::Buffer(b) => {
                self.u8(tags::BUFFER);
                self.extsize(b.len());
                self.raw(b);
            }
            Value::Unicode(s) => self.write_unicode(s),

            Value::Tuple(items) => {
                match items.len() {
                    0 => self.u8(tags::TUPLE_0),
                    1 => self.u8(tags::TUPLE_1),
                    2 => self.u8(tags::TUPLE_2),
                    n => {
                        self.u8(tags::TUPLE);
                        self.extsize(n);
                    }
                }
                for it in items {
                    self.write_at(it, depth + 1)?;
                }
            }
            Value::List(items) => {
                match items.len() {
                    0 => self.u8(tags::LIST_0),
                    1 => self.u8(tags::LIST_1),
                    n => {
                        self.u8(tags::LIST);
                        self.extsize(n);
                    }
                }
                for it in items {
                    self.write_at(it, depth + 1)?;
                }
            }
            Value::Dict(pairs) => {
                self.u8(tags::DICT);
                self.extsize(pairs.len());
                for (k, val) in pairs {
                    self.write_at(val, depth + 1)?;
                    self.write_at(k, depth + 1)?;
                }
            }

            Value::Global(name) => {
                self.u8(tags::GLOBAL);
                self.extsize(name.len());
                self.raw(name);
            }
            Value::Instance { class, state } => {
                self.u8(tags::INSTANCE);
                self.write_at(class, depth + 1)?;
                self.write_at(state, depth + 1)?;
            }
            Value::CallbackObject(id) => {
                self.u8(tags::CALLBACK_OBJECT);
                self.write_at(id, depth + 1)?;
            }
            Value::BlueObject(b) => {
                self.u8(tags::BLUE_OBJECT);
                self.extsize(b.len());
                self.raw(b);
            }
            Value::MarshalStream(blob) => {
                self.u8(tags::MARSHAL_STREAM);
                self.extsize(blob.len());
                self.raw(blob);
            }
            Value::Pickle(b) => {
                self.u8(tags::PICKLE_REST);
                self.raw(b);
            }
            Value::Reduce { callable, args, state, list_items, dict_items } => {
                self.u8(tags::REDUCE);
                let mut head = vec![(**callable).clone(), (**args).clone()];
                if let Some(s) = state {
                    head.push((**s).clone());
                }
                self.write_at(&Value::Tuple(head), depth + 1)?;
                self.write_iterators(list_items, dict_items, depth)?;
            }
            Value::NewObj { args, list_items, dict_items } => {
                self.u8(tags::NEWOBJ);
                self.write_at(args, depth + 1)?;
                self.write_iterators(list_items, dict_items, depth)?;
            }
            Value::DbRow(row) => {
                self.u8(tags::DBROW);
                self.write_descriptor(&row.descriptor, depth)?;
                let (packed, objects) = row.descriptor.pack(&row.values);
                self.extsize(packed.len());
                self.raw(&packed);
                for o in &objects {
                    self.write_at(o, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    fn write_str(&mut self, b: &[u8]) {
        if let Some(idx) = self.table.index_of(b) {
            self.u8(tags::STRING_TABLE);
            self.u8(idx);
        } else if b.is_empty() {
            self.u8(tags::STRING_EMPTY);
        } else if b.len() == 1 {
            self.u8(tags::STRING_CHAR);
            self.u8(b[0]);
        } else if b.len() < 256 {
            self.u8(tags::STRING_SHORT);
            self.u8(b.len() as u8);
            self.raw(b);
        } else {
            self.u8(tags::STRING_LONG);
            self.extsize(b.len());
            self.raw(b);
        }
    }

    fn write_unicode(&mut self, s: &str) {
        let units: Vec<u16> = s.encode_utf16().collect();
        match units.len() {
            0 => self.u8(tags::UNICODE_EMPTY),
            1 => {
                self.u8(tags::UNICODE_CHAR);
                self.raw(&units[0].to_le_bytes());
            }
            n => {
                if s.len() < 2 * n {
                    self.u8(tags::UNICODE_UTF8);
                    self.extsize(s.len());
                    self.raw(s.as_bytes());
                } else {
                    self.u8(tags::UNICODE_UCS2);
                    self.extsize(n);
                    for u in &units {
                        self.raw(&u.to_le_bytes());
                    }
                }
            }
        }
    }
}

pub fn save(v: &Value, table: &StringTable) -> Result<Vec<u8>> {
    let mut e = Encoder::new(table);
    e.write(v)?;
    Ok(e.finish())
}
