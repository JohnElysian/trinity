use std::collections::HashMap;

use eve_marshal::{StringTable, Value};
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::Write;

use crate::sde;
use crate::store::{Db, Row, Store};

#[derive(Copy, Clone, PartialEq)]
pub enum Shape {
    Tuple,
    Rowset,
    /// A bare list of packed `blue.DBRow`s, as Trinity's own cache holds
    /// `dgmtypeattribs` and `dgmtypeeffects`; cfg indexes them by `row['typeID']`
    /// through util.IndexedRowLists.
    DbRows,
    /// `(blue.DBRowDescriptor, [blue.DBRow, ...])`, Trinity's `ramtypematerials`
    /// and `ramtyperequirements`; cfg walks `[1]` reading row.typeID.
    DescriptorRows,
}

pub struct Spec {
    pub key: &'static str,
    pub shape: Shape,
    pub source: Source,
    /// The column names.
    pub header: &'static [&'static str],
}

/// Where a bulk object's rows come from: the static data (`sde`), or the
/// live tables the game changes (the store).
pub enum Source {
    /// Rows put together from live tables, as their query gave them.
    Live(fn(&Db) -> Vec<Value>),
    /// These columns of a static table, every row in dump order: what a bare
    /// `SELECT` of them gave.
    Table(&'static str, &'static [&'static str]),
    /// Static rows put together in code, where the query that made them
    /// joined, filtered, sorted or computed.
    Built(fn() -> Vec<Value>),
    /// No rows.
    Empty,
}

const fn live(key: &'static str, shape: Shape, header: &'static [&'static str], build: fn(&Db) -> Vec<Value>) -> Spec {
    Spec { key, shape, source: Source::Live(build), header }
}

const fn t(key: &'static str, shape: Shape, table: &'static str, columns: &'static [&'static str]) -> Spec {
    Spec { key, shape, source: Source::Table(table, columns), header: columns }
}

const fn built(key: &'static str, shape: Shape, header: &'static [&'static str], build: fn() -> Vec<Value>) -> Spec {
    Spec { key, shape, source: Source::Built(build), header }
}

const fn empty(key: &'static str, shape: Shape, header: &'static [&'static str]) -> Spec {
    Spec { key, shape, source: Source::Empty, header }
}

// Trinity's login set: every key cfg.GotInitData reads (common/script/sys/cfg.py),
// each with the columns and shape of the client's own bulkdata/*.cache copy.
pub const SPECS: &[Spec] = &[
    t("config.BulkData.graphics", Shape::Tuple, "eveGraphics",
      &["graphicID", "url3D", "urlWeb", "icon", "urlSound", "explosionID"]),
    t("config.BulkData.units", Shape::Tuple, "eveUnits", &["unitID", "unitName", "displayName"]),
    empty("config.BulkData.owners", Shape::Tuple, &["ownerID", "ownerName", "typeID"]),
    live("config.BulkData.tickernames", Shape::Tuple, TICKERNAMES_HEADER, tickernames),
    empty("config.BulkData.allianceshortnames", Shape::Tuple, &["allianceID", "shortName"]),
    empty("config.BulkData.locations", Shape::Tuple,
          &["locationID", "locationName", "x", "y", "z"]),
    t("config.BulkData.categories", Shape::Tuple, "invCategories",
      &["categoryID", "categoryName", "description", "graphicID", "published"]),
    t("config.BulkData.metagroups", Shape::Tuple, "invMetaGroups",
      &["metaGroupID", "metaGroupName", "description", "graphicID"]),
    t("config.BulkData.groups", Shape::Tuple, "invGroups",
      &["groupID", "categoryID", "groupName", "description", "graphicID", "useBasePrice", "allowManufacture", "allowRecycler", "anchored", "anchorable", "fittableNonSingleton", "published"]),
    t("config.BulkData.types", Shape::Tuple, "invTypes",
      &["typeID", "groupID", "typeName", "description", "graphicID", "radius", "mass", "volume", "capacity", "portionSize", "raceID", "basePrice", "published", "marketGroupID", "chanceOfDuplicating"]),
    // The blueprint's own name, description, icon and price come with it.
    built("config.BulkData.bptypes", Shape::Tuple, BPTYPES_HEADER, blueprint_types),
    t("config.BulkData.invtypereactions", Shape::Tuple, "invTypeReactions",
      &["reactionTypeID", "input", "typeID", "quantity"]),
    t("config.BulkData.invmetatypes", Shape::Tuple, "invMetaTypes", &["typeID", "parentTypeID", "metaGroupID"]),
    t("config.BulkData.dgmattribs", Shape::Tuple, "dgmAttributeTypes",
      &["attributeID", "attributeName", "attributeCategory", "description", "maxAttributeID", "attributeIdx", "graphicID", "chargeRechargeTimeID", "defaultValue", "published", "unitID", "displayName", "stackable", "highIsGood"]),
    t("config.BulkData.dgmeffects", Shape::Tuple, "dgmEffects",
      &["effectID", "effectName", "effectCategory", "preExpression", "postExpression", "description", "guid", "graphicID", "isOffensive", "isAssistance", "durationAttributeID", "trackingSpeedAttributeID", "dischargeAttributeID", "rangeAttributeID", "falloffAttributeID", "published", "displayName", "isWarpSafe", "rangeChance", "electronicChance", "propulsionChance", "distribution", "sfxName", "npcUsageChanceAttributeID", "npcActivationChanceAttributeID", "fittingUsageChanceAttributeID"]),
    // The client-shipped subset of dgmTypeAttributes (the client's own copy of
    // this object holds the same rows; tools/trinity_static.py loads them).
    built("config.BulkData.dgmtypeattribs", Shape::DbRows, &["typeID", "attributeID", "value"], type_attribs),
    built("config.BulkData.dgmtypeeffects", Shape::DbRows, &["typeID", "effectID", "isDefault"], type_effects),
    t("config.BulkData.shiptypes", Shape::Tuple, "invShipTypes",
      &["shipTypeID", "weaponTypeID", "miningTypeID", "skillTypeID"]),
    t("config.BulkData.billtypes", Shape::Tuple, "billTypes", &["billTypeID", "billTypeName", "description"]),
    // The name twice: the client reads it as typeName too.
    Spec {
        key: "config.BulkData.ramaltypes",
        shape: Shape::Tuple,
        source: Source::Table("ramAssemblyLineTypes",
            &["assemblyLineTypeID", "assemblyLineTypeName", "assemblyLineTypeName", "description", "activityID", "baseTimeMultiplier", "baseMaterialMultiplier", "volume"]),
        header: &["assemblyLineTypeID", "assemblyLineTypeName", "typeName", "description", "activityID", "baseTimeMultiplier", "baseMaterialMultiplier", "volume"],
    },
    built("config.BulkData.ramaltypesdetailpercategory", Shape::Tuple,
          &["assemblyLineTypeID", "activityID", "categoryID", "timeMultiplier", "materialMultiplier"], line_details_per_category),
    built("config.BulkData.ramaltypesdetailpergroup", Shape::Tuple,
          &["assemblyLineTypeID", "activityID", "groupID", "timeMultiplier", "materialMultiplier"], line_details_per_group),
    t("config.BulkData.ramactivities", Shape::Tuple, "ramActivities",
      &["activityID", "activityName", "iconNo", "description", "published"]),
    t("config.BulkData.ramcompletedstatuses", Shape::Tuple, "ramCompletedStatuses",
      &["completedStatusID", "completedStatusName"]),
    // A product's base materials, keyed by the product: what it is built from
    // and what reprocessing gives back. In this data they are the
    // product-keyed activity-6 rows consumed whole; skills (damage 0) and tools
    // (fractional damage) are requirements, not materials.
    built("config.BulkData.ramtypematerials", Shape::DescriptorRows, &["typeID", "materialTypeID", "quantity"], type_materials),
    // Extra requirements per (blueprint, activity); cfg keys them by
    // (row.typeID, row.activity).
    built("config.BulkData.ramtyperequirements", Shape::DescriptorRows,
          &["typeID", "activity", "requiredTypeID", "quantity", "damagePerJob", "recycle"], type_requirements),
    // Encyclopedia text for a few celestials. Not in the base data; the client
    // looks a celestial up and does without when it is missing.
    empty("config.BulkData.mapcelestialdescriptions", Shape::Tuple, &["celestialID", "description"]),
    t("config.InvContrabandTypes", Shape::Rowset, "invContrabandTypes",
      &["factionID", "typeID", "standingLoss", "confiscateMinSec", "fineByValue", "attackMinSec"]),
    t("config.Roles", Shape::Rowset, "eveRoles", &["roleID", "roleName", "description"]),
    t("config.Flags", Shape::Rowset, "eveFlags", &["flagID", "flagName", "flagText", "flagType", "orderID"]),
    t("config.Races", Shape::Rowset, "chrRaces", &["raceID", "raceName", "description", "graphicID"]),
    t("config.Bloodlines", Shape::Rowset, "chrBloodlines",
      &["bloodlineID", "bloodlineName", "raceID", "description", "maleDescription", "femaleDescription", "shipTypeID", "corporationID"]),
    t("config.Statistics", Shape::Rowset, "eveStatistics",
      &["statisticID", "statisticName", "statisticType", "description", "orderID", "graphicID"]),
    t("config.Units", Shape::Rowset, "eveUnits", &["unitID", "unitName", "displayName", "description"]),
    live("config.ChannelTypes", Shape::Rowset, CHANNEL_TYPES_HEADER, channel_types),
    empty("config.EncyclopediaTypes", Shape::Rowset,
          &["encyclopediaTypeID", "encyclopediaTypeName"]),
    // Sent whole at login, so within the 1 MB a message may be: bots are left
    // out (the client asks for them by GetMultiOwnersEx, which finds them).
    live("config.StaticOwners", Shape::Rowset, STATIC_OWNERS_HEADER, static_owners),
    t("config.StaticLocations", Shape::Rowset, "eveStaticLocations", &["locationID", "locationName", "x", "y", "z"]),
];

const TICKERNAMES_HEADER: &[&str] = &["corporationID", "tickerName", "shape1", "shape2", "shape3", "color1", "color2", "color3"];
const CHANNEL_TYPES_HEADER: &[&str] = &["channelTypeID", "channelTypeName", "defaultName"];
const STATIC_OWNERS_HEADER: &[&str] = &["ownerID", "ownerName", "typeID"];

/// A live row's columns as the bulk data sends them: what reading them out
/// of PostgreSQL gave (bigint as long, booleans as 0/1, text as bytes).
fn live_line(row: &Row, columns: &[&str]) -> Value {
    let table = row.table();
    Value::List(columns.iter().map(|c| row.value(c).to_bulk(table.def.columns[table.col(c)].ty.wire())).collect())
}

/// Every corporation's ticker and logo, in table order.
fn tickernames(db: &Db) -> Vec<Value> {
    db.table("corporation").rows().map(|r| live_line(&r, TICKERNAMES_HEADER)).collect()
}

fn channel_types(db: &Db) -> Vec<Value> {
    db.table("channelTypes").rows().map(|r| live_line(&r, CHANNEL_TYPES_HEADER)).collect()
}

/// The static owners, bots left out.
fn static_owners(db: &Db) -> Vec<Value> {
    let bots = db.table("bots");
    db.table("eveStaticOwners")
        .rows()
        .filter(|o| bots.find(&["characterID"], &[o.value("ownerID").clone()]).next().is_none())
        .map(|r| live_line(&r, STATIC_OWNERS_HEADER))
        .collect()
}

const BPTYPES_HEADER: &[&str] = &[
    "blueprintTypeName", "description", "graphicID", "basePrice", "blueprintTypeID", "parentBlueprintTypeID",
    "productTypeID", "productionTime", "techLevel", "researchProductivityTime", "researchMaterialTime",
    "researchCopyTime", "researchTechTime", "productivityModifier", "materialModifier", "wasteFactor",
    "chanceOfReverseEngineering", "maxProductionLimit",
];

/// Each blueprint with its own type's name, description, icon and price,
/// blueprints without a type left out (the inner join with invTypes this
/// was), in invTypes' order, which PostgreSQL scanned while it looked the
/// blueprints up.
fn blueprint_types() -> Vec<Value> {
    let blueprints = sde::table("invBlueprintTypes");
    sde::table("invTypes")
        .rows()
        .filter_map(|ty| {
            let b = blueprints.get("blueprintTypeID", ty.int("typeID")?)?;
            let mut line = ty.bulk_line(&["typeName", "description", "graphicID", "basePrice"]);
            line.extend(b.bulk_line(&BPTYPES_HEADER[4..]));
            Some(Value::List(line))
        })
        .collect()
}

/// By type, then attribute. The value is text in the data, sent as the
/// number it holds.
fn type_attribs() -> Vec<Value> {
    let mut rows: Vec<_> = sde::table("dgmTypeAttribs").rows().collect();
    rows.sort_by_key(|r| (nulls_last(r.int("typeID")), nulls_last(r.int("attributeID"))));
    rows.iter()
        .map(|r| {
            let mut line = r.bulk_line(&["typeID", "attributeID"]);
            line.push(r.text("value").map_or(Value::None, numeric_value));
            Value::List(line)
        })
        .collect()
}

/// By type, then effect; isDefault as 0/1, unset counting as 0.
fn type_effects() -> Vec<Value> {
    let mut rows: Vec<_> = sde::table("dgmTypeEffects").rows().collect();
    rows.sort_by_key(|r| (nulls_last(r.int("typeID")), nulls_last(r.int("effectID"))));
    rows.iter()
        .map(|r| {
            let mut line = r.bulk_line(&["typeID", "effectID"]);
            line.push(Value::Int((r.int("isDefault").unwrap_or(0) != 0) as i64));
            Value::List(line)
        })
        .collect()
}

fn line_details_per_category() -> Vec<Value> {
    line_details("ramAssemblyLineTypeDetailPerCategory", "categoryID")
}

fn line_details_per_group() -> Vec<Value> {
    line_details("ramAssemblyLineTypeDetailPerGroup", "groupID")
}

/// An assembly line type's multipliers per category or group, each with the
/// line type's activity: in the details' order, those whose line type is
/// unknown left out (the inner join this was).
fn line_details(table: &str, by: &str) -> Vec<Value> {
    let types = sde::table("ramAssemblyLineTypes");
    sde::table(table)
        .rows()
        .filter_map(|d| {
            let ty = types.get("assemblyLineTypeID", d.int("assemblyLineTypeID")?)?;
            let mut line = d.bulk_line(&["assemblyLineTypeID"]);
            line.extend(ty.bulk_line(&["activityID"]));
            line.extend(d.bulk_line(&[by, "timeMultiplier", "materialMultiplier"]));
            Some(Value::List(line))
        })
        .collect()
}

fn type_materials() -> Vec<Value> {
    let mut rows: Vec<_> = sde::table("TL2MaterialsForTypeWithActivity")
        .rows()
        .filter(|r| r.int("activity") == Some(6) && r.float("damagePerJob") == Some(1.0))
        .collect();
    rows.sort_by_key(|r| (nulls_last(r.int("typeID")), nulls_last(r.int("requiredTypeID"))));
    rows.iter().map(|r| Value::List(r.bulk_line(&["typeID", "requiredTypeID", "quantity"]))).collect()
}

/// Recycled is whether the job consumes it whole (unknown where the damage
/// is).
fn type_requirements() -> Vec<Value> {
    let mut rows: Vec<_> = sde::table("TL2MaterialsForTypeWithActivity")
        .rows()
        .filter(|r| r.int("activity").is_some_and(|a| (1..=5).contains(&a)))
        .collect();
    rows.sort_by_key(|r| (nulls_last(r.int("typeID")), nulls_last(r.int("activity")), nulls_last(r.int("requiredTypeID"))));
    rows.iter()
        .map(|r| {
            let mut line = r.bulk_line(&["typeID", "activity", "requiredTypeID", "quantity", "damagePerJob"]);
            line.push(r.float("damagePerJob").map_or(Value::None, |d| Value::Int((d == 1.0) as i64)));
            Value::List(line)
        })
        .collect()
}

/// A sort key that puts NULL after every value, as ORDER BY does.
fn nulls_last(v: Option<i64>) -> (bool, i64) {
    (v.is_none(), v.unwrap_or(0))
}

pub struct Entry {
    pub key: String,
    pub version: (i64, i64),
    pub pickle: Vec<u8>,
    pub compressed: bool,
    pub rows: usize,
}

pub struct BulkData {
    pub entries: HashMap<String, Entry>,
    pub node_id: i64,
}

const CACHED_OBJECT_GUID: &[u8] = b"util.CachedObject";
const PAYLOAD_GUID: &[u8] = b"objectCaching.CachedObject";
const ROWSET_GUID: &[u8] = b"util.Rowset";

pub fn crc_hqx(data: &[u8], seed: u16) -> u16 {
    let mut crc = seed;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
    }
    crc
}

pub const CRC_SEED: u16 = 170_611_u32 as u16;

impl BulkData {
    pub fn load(store: &Store, table: &StringTable, node_id: i64, now: i64) -> Result<Self, String> {
        let mut entries = HashMap::new();
        for spec in SPECS {
            let named = || spec.header.iter().map(|h| Value::bytes(*h)).collect::<Vec<_>>();
            let (header, lines) = match spec.source {
                Source::Live(build) => (named(), store.read(build)),
                Source::Table(table, columns) => {
                    (named(), sde::table(table).rows().map(|r| Value::List(r.bulk_line(columns))).collect())
                }
                Source::Built(build) => (named(), build()),
                Source::Empty => (named(), Vec::new()),
            };
            let rows = lines.len();
            let payload = match spec.shape {
                Shape::Tuple => {
                    Value::Tuple(vec![Value::List(header), Value::List(lines)])
                }
                Shape::Rowset => Value::Instance {
                    class: Box::new(Value::Bytes(ROWSET_GUID.to_vec())),
                    state: Box::new(Value::Dict(vec![
                        (Value::bytes("header"), Value::List(header)),
                        (Value::bytes("lines"), Value::List(lines)),
                    ])),
                },
                Shape::DbRows | Shape::DescriptorRows => {
                    let descriptor = std::sync::Arc::new(descriptor_for(&header, &lines));
                    let rows = Value::List(
                        lines
                            .into_iter()
                            .map(|line| {
                                Value::DbRow(Box::new(eve_marshal::DbRow {
                                    descriptor: std::sync::Arc::clone(&descriptor),
                                    values: line.seq().map(<[Value]>::to_vec).unwrap_or_default(),
                                }))
                            })
                            .collect(),
                    );
                    if spec.shape == Shape::DbRows {
                        rows
                    } else {
                        Value::Tuple(vec![descriptor.to_value(), rows])
                    }
                }
            };

            let inner = eve_marshal::save(&payload, table).expect("bulk payload is encodable");
            let checksum = crc_hqx(&inner, CRC_SEED) as i64;
            let (pickle, compressed) = maybe_deflate(&inner);

            entries.insert(
                spec.key.to_string(),
                Entry {
                    key: spec.key.to_string(),
                    version: (now, checksum),
                    pickle,
                    compressed,
                    rows,
                },
            );
        }
        Ok(BulkData { entries, node_id })
    }

    pub fn handle(&self, entry: &Entry) -> Value {
        Value::Instance {
            class: Box::new(Value::Bytes(CACHED_OBJECT_GUID.to_vec())),
            state: Box::new(Value::Tuple(vec![
                Value::Bytes(entry.key.as_bytes().to_vec()),
                Value::Int(self.node_id),
                version_value(entry.version),
            ])),
        }
    }

    pub fn init_vals(&self) -> Value {
        let mut pairs: Vec<(Value, Value)> = self
            .entries
            .values()
            .map(|e| (Value::Bytes(e.key.as_bytes().to_vec()), self.handle(e)))
            .collect();
        pairs.sort_by(|a, b| a.0.as_bytes().cmp(&b.0.as_bytes()));
        Value::Dict(pairs)
    }

    pub fn payload(&self, entry: &Entry) -> Value {
        Value::Instance {
            class: Box::new(Value::Bytes(PAYLOAD_GUID.to_vec())),
            state: Box::new(Value::Tuple(vec![
                version_value(entry.version),
                Value::None,
                Value::Int(self.node_id),
                Value::Int(1),
                Value::Bytes(entry.pickle.clone()),
                Value::Int(entry.compressed as i64),
                Value::Bytes(entry.key.as_bytes().to_vec()),
            ])),
        }
    }
}

/// A DBRow descriptor for query results: each column the narrowest ADO type
/// that holds every value in it. A packed field cannot be null, so a column
/// with a null (or text) in it is an object column.
pub(crate) fn descriptor_for(header: &[Value], lines: &[Value]) -> eve_marshal::DbRowDescriptor {
    use eve_marshal::dbrow::dbtype;
    let columns = header
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let mut t: Option<i64> = None;
            for line in lines {
                let v = line.seq().and_then(|l| l.get(i)).unwrap_or(&Value::None);
                let this = match v {
                    Value::Bool(_) => dbtype::BOOL,
                    Value::Int(n) | Value::Long(n) if i32::try_from(*n).is_ok() => dbtype::I4,
                    Value::Int(_) | Value::Long(_) => dbtype::I8,
                    Value::Double(_) => dbtype::R8,
                    _ => dbtype::WSTR,
                };
                t = Some(match (t, this) {
                    (None, x) => x,
                    (Some(a), b) if a == b => a,
                    (Some(dbtype::WSTR), _) | (_, dbtype::WSTR) => dbtype::WSTR,
                    (Some(dbtype::R8), _) | (_, dbtype::R8) => dbtype::R8,
                    (Some(dbtype::I8), _) | (_, dbtype::I8) => dbtype::I8,
                    _ => dbtype::I4,
                });
            }
            (name.as_bytes().unwrap_or_default().to_vec(), t.unwrap_or(dbtype::WSTR))
        })
        .collect();
    eve_marshal::DbRowDescriptor::new(columns)
}

fn version_value(v: (i64, i64)) -> Value {
    Value::Tuple(vec![Value::Long(v.0), Value::Int(v.1)])
}

fn maybe_deflate(data: &[u8]) -> (Vec<u8>, bool) {
    if data.len() <= 200 {
        return (data.to_vec(), false);
    }
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::new(9));
    if enc.write_all(data).is_err() {
        return (data.to_vec(), false);
    }
    match enc.finish() {
        Ok(out) if out.len() < data.len() => (out, true),
        _ => (data.to_vec(), false),
    }
}

fn numeric_value(s: &str) -> Value {
    let t = s.trim();
    if let Ok(i) = t.parse::<i64>() {
        return Value::Int(i);
    }
    match t.parse::<f64>() {
        Ok(f) => Value::Double(f),
        Err(_) => Value::Bytes(s.as_bytes().to_vec()),
    }
}

pub fn filetime_now() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    const EPOCH_DIFF_SECS: i64 = 11_644_473_600;
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs() as i64 + EPOCH_DIFF_SECS) * 10_000_000 + (d.subsec_nanos() as i64 / 100)
}

pub fn service_info() -> Value {
    Value::Dict(Vec::new())
}
