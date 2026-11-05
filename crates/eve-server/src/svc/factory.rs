//! `factory`: blueprint and material information for the info window (infosvc.py)
//! and the facility window (facility.py).
//!
//! A blueprint's research and copy state is its invBlueprints row; one without
//! is an original at material and productivity level 0 with unlimited runs,
//! the same defaults the client itself assumes for a blueprint type with no item.

use eve_marshal::Value;

use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db};
use crate::svc::rows::rowset;

const CATEGORY_SKILL: i64 = 16;
const ACTIVITY_MANUFACTURING: i64 = 1;

pub async fn factory(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetBlueprintAttributes" => blueprint_attributes(ctx, call.arg_id(0)).await,
        // (blueprintTypeID, activity); activity -1 means every activity.
        "GetMaterialsForTypeWithActivity" => {
            let activity = call.arg_i64(1).unwrap_or(-1) as i32 as i64;
            let header = ["activity", "requiredTypeID", "quantity", "damagePerJob"];
            let mut rows: Vec<_> = crate::sde::table("TL2MaterialsForTypeWithActivity")
                .find("typeID", call.arg_id(0) as i32 as i64)
                .filter(|m| activity == -1 || m.int("activity") == Some(activity))
                .collect();
            // ORDER BY "activity","requiredTypeID", NULLs last.
            rows.sort_by_key(|m| (m.int("activity").is_none(), m.int("activity"), m.int("requiredTypeID").is_none(), m.int("requiredTypeID")));
            rowset(&header, rows.iter().map(|m| Value::List(m.line(&header))).collect())
        }
        // What an item is built from: the consumed manufacturing inputs of the
        // blueprint that produces it (no skills, no reusable tools).
        "GetMaterialCompositionOfItemType" => {
            let sde = crate::sde::get();
            let materials = sde.table("TL2MaterialsForTypeWithActivity");
            let mut rows: Vec<_> = sde
                .table("invBlueprintTypes")
                .find("productTypeID", call.arg_id(0) as i32 as i64)
                .filter_map(|b| b.int("blueprintTypeID"))
                .flat_map(|blueprint| materials.find("typeID", blueprint))
                .filter(|m| {
                    m.int("activity") == Some(ACTIVITY_MANUFACTURING)
                        && m.float("damagePerJob").is_some_and(|d| d >= 1.0)
                        && m.int("quantity").is_some_and(|q| q > 0)
                        && m.int("requiredTypeID").and_then(|t| sde.group_and_category(t)).is_some_and(|(_, c)| c != CATEGORY_SKILL)
                })
                .collect();
            rows.sort_by_key(|m| m.int("requiredTypeID"));
            rowset(&["typeID", "quantity"], rows.iter().map(|m| Value::List(m.line(&["requiredTypeID", "quantity"]))).collect())
        }
        // (locationID, flag, isCorp): the blueprints in one hangar.
        "GetBlueprintInformationAtLocationWithFlag" => {
            let header = [
                "itemID", "typeID", "quantity", "singleton", "copy", "materialLevel", "productivityLevel",
                "licensedProductionRunsRemaining",
            ];
            let (location, flag) = (call.arg_id(0), call.arg_id(1));
            let blueprints = crate::sde::table("invBlueprintTypes");
            let lines = ctx.store.read(|db| {
                let mut rows: Vec<_> = db
                    .table("entity")
                    .find(&["locationID", "flag"], &[store::Value::Int(location), store::Value::Int(flag)])
                    .filter(|e| e.int("typeID").is_some_and(|t| blueprints.get("blueprintTypeID", t).is_some()))
                    .collect();
                rows.sort_by_key(|e| e.int("itemID"));
                rows.iter()
                    .map(|e| {
                        let (copy, material, productivity, runs) = research(db, e.int("itemID").unwrap_or(0));
                        let mut line = e.line(&header[..4]);
                        line.extend([copy, material, productivity, runs].map(Value::Int));
                        Value::List(line)
                    })
                    .collect()
            });
            rowset(&header, lines)
        }
        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// infosvc reads these keys from the dict (has_key) for a blueprint item.
async fn blueprint_attributes(ctx: &Ctx<'_>, item_id: i64) -> Value {
    let found = ctx.store.read(|db| {
        let type_id = db.table("entity").get_id(item_id)?.int("typeID");
        Some((type_id, research(db, item_id)))
    });
    let Some((type_id, (copy, material, productivity, runs))) = found else { return Value::None };
    // Only a blueprint has these: an item of any other type has no answer.
    let Some(b) = type_id.and_then(|t| crate::sde::table("invBlueprintTypes").get("blueprintTypeID", t)) else { return Value::None };
    let num = Value::Int;
    let static_num = |c: &str| Value::Int(b.int(c).unwrap_or(0));
    let pairs = [
        ("manufacturingTime", static_num("productionTime")),
        ("researchMaterialTime", static_num("researchMaterialTime")),
        ("researchCopyTime", static_num("researchCopyTime")),
        ("researchProductivityTime", static_num("researchProductivityTime")),
        ("wastageFactor", Value::Double(b.float("wasteFactor").unwrap_or(0.0) / 100.0)),
        ("productTypeID", static_num("productTypeID")),
        ("materialLevel", num(material)),
        ("productivityLevel", num(productivity)),
        ("copy", num(copy)),
        ("licensedProductionRunsRemaining", num(runs)),
    ];
    Value::Dict(pairs.into_iter().map(|(k, v)| (Value::bytes(k), v)).collect())
}

/// A blueprint item's copy flag, material and productivity levels and runs
/// left: its invBlueprints row, else an original's (0, 0, 0, unlimited).
fn research(db: &Db, item_id: i64) -> (i64, i64, i64, i64) {
    let row = db.table("invBlueprints").get_id(item_id);
    let col = |c: &str, default: i64| row.and_then(|r| r.int(c)).unwrap_or(default);
    (col("copy", 0), col("materialLevel", 0), col("productivityLevel", 0), col("licensedProductionRunsRemaining", -1))
}
