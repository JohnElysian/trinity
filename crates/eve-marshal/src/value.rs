#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    None,
    Bool(bool),
    Int(i64),
    Long(i64),
    Double(f64),
    Bytes(Vec<u8>),
    Buffer(Vec<u8>),
    Unicode(String),
    Tuple(Vec<Value>),
    List(Vec<Value>),
    Dict(Vec<(Value, Value)>),
    Global(Vec<u8>),
    Instance {
        class: Box<Value>,
        state: Box<Value>,
    },
    CallbackObject(Box<Value>),
    BlueObject(Vec<u8>),
    MarshalStream(Vec<u8>),
    Pickle(Vec<u8>),
    Marker,
    /// `TYPE_REDUCE` (0x22): a new-style object rebuilt as `callable(*args)`,
    /// then `__setstate__(state)`, list appends and dict sets. Python 2.5
    /// exceptions travel this way (`UserError`, `GPSTransportClosed`), as does
    /// `blue.DBRowDescriptor`.
    Reduce {
        callable: Box<Value>,
        args: Box<Value>,
        state: Option<Box<Value>>,
        list_items: Vec<Value>,
        dict_items: Vec<(Value, Value)>,
    },
    /// `TYPE_NEWOBJ` (0x23): `cls.__new__(cls, *args[1:])` with the class first
    /// in `args`.
    NewObj {
        args: Box<Value>,
        list_items: Vec<Value>,
        dict_items: Vec<(Value, Value)>,
    },
    /// `TYPE_DBROW` (0x2A): a packed `blue.DBRow`. Rows of one rowset share
    /// their descriptor, which the encoder writes once per stream.
    DbRow(Box<crate::dbrow::DbRow>),
}

impl Value {
    /// `callable(*args)` for a global name the client can import, such as
    /// `ccp_exceptions.UserError`.
    pub fn reduce(global: &str, args: Vec<Value>) -> Value {
        Value::Reduce {
            callable: Box::new(Value::Global(global.as_bytes().to_vec())),
            args: Box::new(Value::Tuple(args)),
            state: None,
            list_items: Vec::new(),
            dict_items: Vec::new(),
        }
    }

    /// The global a reduce calls, if it names one.
    pub fn reduce_global(&self) -> Option<&[u8]> {
        match self {
            Value::Reduce { callable, .. } => match &**callable {
                Value::Global(g) => Some(g),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn bytes(s: impl Into<Vec<u8>>) -> Value {
        Value::Bytes(s.into())
    }

    pub fn unicode(s: impl Into<String>) -> Value {
        Value::Unicode(s.into())
    }

    /// Text for the client: plain ASCII as `str`, anything else as `unicode`, which
    /// the client renders correctly where UTF-8 bytes would come out garbled.
    pub fn text(s: impl Into<String>) -> Value {
        let s = s.into();
        if s.is_ascii() {
            Value::Bytes(s.into_bytes())
        } else {
            Value::Unicode(s)
        }
    }

    /// An `int`, or `None` when absent.
    pub fn int_or_none(v: Option<i64>) -> Value {
        v.map_or(Value::None, Value::Int)
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Value::None => "NoneType",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Long(_) => "long",
            Value::Double(_) => "float",
            Value::Bytes(_) => "str",
            Value::Buffer(_) => "buffer",
            Value::Unicode(_) => "unicode",
            Value::Tuple(_) => "tuple",
            Value::List(_) => "list",
            Value::Dict(_) => "dict",
            Value::Global(_) => "global",
            Value::Instance { .. } => "instance",
            Value::CallbackObject(_) => "callback",
            Value::BlueObject(_) => "blue object",
            Value::MarshalStream(_) => "blue.MarshalStream",
            Value::Pickle(_) => "pickle",
            Value::Marker => "marker",
            Value::Reduce { .. } => "reduce",
            Value::NewObj { .. } => "newobj",
            Value::DbRow(_) => "blue.DBRow",
        }
    }

    pub fn get(&self, key: &Value) -> Option<&Value> {
        match self {
            Value::Dict(pairs) => pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_str(&self, key: &str) -> Option<&Value> {
        self.get(&Value::Bytes(key.as_bytes().to_vec()))
    }

    pub fn seq(&self) -> Option<&[Value]> {
        match self {
            Value::Tuple(v) | Value::List(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match *self {
            Value::Int(v) | Value::Long(v) => Some(v),
            Value::Bool(b) => Some(b as i64),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match *self {
            Value::Double(v) => Some(v),
            _ => self.as_i64().map(|v| v as f64),
        }
    }

    /// A `str`, `buffer` or `unicode` as a Rust string (bytes decoded lossily as UTF-8).
    pub fn as_string(&self) -> Option<String> {
        match self {
            Value::Bytes(b) | Value::Buffer(b) => Some(String::from_utf8_lossy(b).into_owned()),
            Value::Unicode(s) => Some(s.clone()),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) | Value::Buffer(b) => Some(b),
            _ => None,
        }
    }
}
