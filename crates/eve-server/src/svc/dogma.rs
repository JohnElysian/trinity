use std::collections::{BTreeMap, HashSet};

use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::rows::{index_rowset, line, row};

const SERVICE: &str = "dogmaIM";

const EFFECT_ONLINE: i64 = 16;
const ATTR_IS_ONLINE: i32 = 2;
const ATTR_QUANTITY: i32 = 805;

const CHARGED: [(i32, i32, i32); 2] = [(18, 55, 482), (264, 479, 263)];

const ALLOWED_CATEGORIES: [i32; 7] = [11, 23, 6, 7, 20, 16, 8];
const GROUP_CHARACTER: i32 = 1;

pub async fn dogma_im(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(method_call(ctx, &call.method, &call.args).await)
}

async fn method_call(ctx: &Ctx<'_>, method: &str, args: &[Value]) -> Outcome {
    match method {
        "GetAttributeTypes" => Outcome::Value(attribute_types(ctx).await),

        "GetSkillPrerequisites" => Outcome::Value(Value::List(Vec::new())),
        "GetRequiredSkillLevels" => match crate::svc::fitting::required_skill_levels(ctx.db,
            args.first().and_then(Value::as_i64).unwrap_or(0)).await {
            Ok(levels) => Outcome::Value(Value::Dict(levels.into_iter()
                .map(|(skill, level)| (Value::Int(i64::from(skill)), Value::Int(level))).collect())),
            Err(error) => {
                error!("reading required skills: {}", crate::logging::chain(&error));
                Outcome::custom("The required skills could not be read.")
            }
        },

        "ShipGetInfo" => Outcome::Value(ship_info(ctx).await),
        "CharGetInfo" => Outcome::Value(char_info(ctx).await),
        "ItemGetInfo" => Outcome::Value(item_info(ctx, args.first()).await),
        "ChangeDroneSettings" => crate::drones::change_settings(ctx, args.first()).await,

        "CheckSendLocationInfo" => Outcome::Value(Value::None),

        // Targeting and module activation are `inflight`'s.
        "LogAttribute" => Outcome::Value(Value::List(Vec::new())),

        _ => Outcome::Unknown(format!("{SERVICE}::{method}")),
    }
}

async fn attribute_types(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &[
        "attributeID", "attributeName", "attributeCategory", "description", "maxAttributeID",
        "attributeIdx", "graphicID", "chargeRechargeTimeID", "defaultValue", "published",
        "displayName", "unitID", "stackable", "highIsGood",
    ];
    const SQL: &str = r#"
        SELECT "attributeID","attributeName","attributeCategory","description","maxAttributeID",
               "attributeIdx","graphicID","chargeRechargeTimeID","defaultValue","published",
               "displayName","unitID","stackable","highIsGood"
          FROM "dgmAttributeTypes" ORDER BY "attributeID""#;
    match ctx.db.query(SQL, &[]).await {
        Ok(rows) => index_rowset(
            HEADER,
            "attributeName",
            rows.iter()
                .map(|r| {
                    let l = line(r);
                    (l.get(1).cloned().unwrap_or(Value::None), Value::List(l))
                })
                .collect(),
        ),
        Err(e) => {
            error!("dogmaIM::GetAttributeTypes: {err}", err = crate::logging::chain(&e));
            index_rowset(HEADER, "attributeName", Vec::new())
        }
    }
}

const INVITEM_HEADER: &[&str] = &[
    "itemID", "typeID", "ownerID", "locationID", "flag", "contraband", "singleton", "quantity",
    "groupID", "categoryID", "customInfo",
];

const INFO_HEADER: &[&str] = &["itemID", "invItem", "attributes", "activeEffects", "time"];

const ITEM_SELECT: &str = r#"
    SELECT e."itemID", e."typeID", e."ownerID", e."locationID", e."flag", e."contraband",
           e."singleton", e."quantity", t."groupID", g."categoryID", e."customInfo",
           (SELECT b.expires FROM character_boosters b WHERE b.item=e."itemID") AS booster_expiry
      FROM entity e
      JOIN "invTypes"  t ON t."typeID"  = e."typeID"
      JOIN "invGroups" g ON g."groupID" = t."groupID"
     WHERE (e.flag<>88 OR EXISTS(SELECT 1 FROM character_boosters b
                WHERE b.item=e."itemID" AND b.character=e."ownerID" AND b.character=e."locationID"
                  AND b.type_id=e."typeID" AND e.quantity=1 AND e.singleton=1
                  AND b.expires>((extract(epoch FROM clock_timestamp())+11644473600)*10000000)::bigint)) AND "#;

struct Item {
    item_id: i64,
    type_id: i64,
    owner_id: i64,
    location_id: i64,
    group_id: i32,
    category_id: i32,
    booster_expiry: Option<i64>,
    line: Vec<Value>,
}

/// Stacked ammunition can be addressed by its ship, slot and type instead of
/// the inventory entity ID (godma.py::IsSubLocation/UpdateItem).
#[derive(Debug, PartialEq, Eq)]
enum ItemKey {
    Entity(i32),
    Sublocation { location: i32, flag: i32, type_id: i32 },
}

impl ItemKey {
    fn parse(value: &Value) -> Option<Self> {
        let positive = |v: &Value| i32::try_from(v.as_i64()?).ok().filter(|id| *id > 0);
        if let Some(id) = positive(value) {
            return Some(Self::Entity(id));
        }
        let [location, flag, type_id] = value.seq()? else { return None };
        let flag = positive(flag)?;
        if !crate::svc::fitting::is_slot(i64::from(flag)) {
            return None;
        }
        Some(Self::Sublocation { location: positive(location)?, flag, type_id: positive(type_id)? })
    }

    fn value(&self) -> Value {
        match *self {
            Self::Entity(id) => Value::Int(i64::from(id)),
            Self::Sublocation { location, flag, type_id } => Value::Tuple(
                [location, flag, type_id].into_iter().map(|v| Value::Int(i64::from(v))).collect(),
            ),
        }
    }
}

async fn ship_info(ctx: &Ctx<'_>) -> Value {
    let (Some(ship_id), Some(char_id)) = (ctx.session.ship_id, ctx.session.char_id) else {
        return Value::Dict(Vec::new());
    };
    let sql = format!(
        r#"{ITEM_SELECT} (e."itemID" = $1 OR e."itemID" = $2
            OR (e."locationID" = $1 AND (e."flag" BETWEEN 11 AND 35 OR e."flag" BETWEEN 92 AND 99)))"#
    );
    info_dict(ctx, "ShipGetInfo", &sql, &[ship_id, char_id]).await
}

fn params(ids: &[i64]) -> Vec<i32> {
    ids.iter().map(|i| i32::try_from(*i).unwrap_or(0)).collect()
}

async fn char_info(ctx: &Ctx<'_>) -> Value {
    let Some(char_id) = ctx.session.char_id else { return Value::Dict(Vec::new()) };
    let sql = format!(
        r#"{ITEM_SELECT} (e."itemID" = $1
            OR (e."locationID" = $1 AND e."flag" IN (7, 61, 88, 89)))"#
    );
    info_dict(ctx, "CharGetInfo", &sql, &[char_id]).await
}

async fn item_info(ctx: &Ctx<'_>, key: Option<&Value>) -> Value {
    let Some(key) = key.and_then(ItemKey::parse) else { return Value::None };
    let (sql, ids) = match key {
        ItemKey::Entity(id) => (format!(r#"{ITEM_SELECT} e."itemID" = $1"#), vec![i64::from(id)]),
        ItemKey::Sublocation { location, flag, type_id } => (
            format!(r#"{ITEM_SELECT} e."locationID" = $1 AND e."flag" = $2 AND e."typeID" = $3
                AND g."categoryID" = 8 AND e."singleton" = 0 AND e."quantity" > 0
                ORDER BY e."itemID" LIMIT 1"#),
            vec![i64::from(location), i64::from(flag), i64::from(type_id)],
        ),
    };
    let items = match load_items(ctx, "ItemGetInfo", &sql, &ids).await {
        Some(v) => v,
        None => return Value::None,
    };
    let Some(item) = items.into_iter().next() else { return Value::None };
    let attrs = computed_attributes(ctx, &[item.item_id]).await;
    let now = crate::bulkdata::filetime_now();
    let running = running_modules(ctx);
    info_row_for_key(&item, &key, &attrs, now, running.iter().find(|module| module.module_id == item.item_id))
}

async fn info_dict(ctx: &Ctx<'_>, what: &str, sql: &str, ids: &[i64]) -> Value {
    let Some(items) = load_items(ctx, what, sql, ids).await else {
        return Value::Dict(Vec::new());
    };
    let ids: Vec<i64> = items.iter().map(|i| i.item_id).collect();
    let attrs = computed_attributes(ctx, &ids).await;
    let now = crate::bulkdata::filetime_now();
    let running = running_modules(ctx);
    Value::Dict(
        items
            .iter()
            .map(|item| (Value::Int(item.item_id), info_row_for_key(item, &ItemKey::Entity(item.item_id as i32),
                &attrs, now, running.iter().find(|module| module.module_id == item.item_id))))
            .collect(),
    )
}

fn info_row_for_key(item: &Item, key: &ItemKey, attrs: &BTreeMap<i64, BTreeMap<i32, f64>>, now: i64, running: Option<&crate::combat::Active>) -> Value {
    let own = attrs.get(&item.item_id);
    let mut inv_item = item.line.clone();
    let mut attributes = attribute_dict(item, own);
    if let (Some(expiry),Value::Dict(pairs))=(item.booster_expiry,&mut attributes) {
        pairs.retain(|(id,_)|*id!=Value::Int(i64::from(crate::svc::boosters::EXPIRY)));
        pairs.push((Value::Int(i64::from(crate::svc::boosters::EXPIRY)),Value::Long(expiry)));
    }
    if matches!(key, ItemKey::Sublocation { .. }) {
        if let Some(id) = inv_item.first_mut() {
            *id = key.value();
        }
        if let Value::Dict(pairs) = &mut attributes {
            pairs.retain(|(id, _)| *id != Value::Int(i64::from(ATTR_QUANTITY)));
            pairs.push((Value::Int(i64::from(ATTR_QUANTITY)), item.line.get(7).cloned().unwrap_or(Value::Int(0))));
        }
    }
    row(
        INFO_HEADER,
        vec![
            key.value(),
            row(INVITEM_HEADER, inv_item),
            attributes,
            active_effects(item, own, running),
            Value::Long(now),
        ],
    )
}

async fn load_items(ctx: &Ctx<'_>, what: &str, sql: &str, ids: &[i64]) -> Option<Vec<Item>> {
    let vals = params(ids);
    let bound: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> =
        vals.iter().map(|v| v as &(dyn tokio_postgres::types::ToSql + Sync)).collect();
    let rows = match ctx.db.query(sql, &bound).await {
        Ok(r) => r,
        Err(e) => {
            error!("dogmaIM::{what}: {err}", err = crate::logging::chain(&e));
            return None;
        }
    };
    Some(
        rows.iter()
            .filter_map(|r| {
                let get = |i: usize| r.try_get::<_, i32>(i).ok().map(|v| v as i64);
                let item = Item {
                    item_id: get(0)?,
                    type_id: get(1)?,
                    owner_id: get(2).unwrap_or(0),
                    location_id: get(3).unwrap_or(0),
                    group_id: r.try_get::<_, i32>(8).unwrap_or(0),
                    category_id: r.try_get::<_, i32>(9).unwrap_or(0),
                    booster_expiry: r.get(11),
                    line: line(r).into_iter().take(INVITEM_HEADER.len()).collect(),
                };
                let flag = r.try_get::<_, i32>(4).unwrap_or(0);
                if flag == 5 || flag == 87 {
                    return None;
                }
                if !ALLOWED_CATEGORIES.contains(&item.category_id)
                    && item.group_id != GROUP_CHARACTER
                {
                    return None;
                }
                let _ = item.type_id;
                Some(item)
            })
            .collect(),
    )
}

/// Attributes as the dogma engine computes them for the character's loadout
/// (character, skills, implants, ship and what is fitted); anything outside it
/// gets its stored values.
async fn computed_attributes(ctx: &Ctx<'_>, item_ids: &[i64]) -> BTreeMap<i64, BTreeMap<i32, f64>> {
    let mut out = BTreeMap::new();
    let mut rest = Vec::new();
    match ctx.session.char_id {
        Some(char_id) => {
            let active = active_modules(ctx);
            match crate::dogma::Loadout::load(ctx.db, ctx.dogma, char_id, ctx.session.ship_id, &active).await {
                Ok(loadout) => {
                    for id in item_ids {
                        match loadout.find(*id) {
                            Some(i) => {
                                out.insert(*id, loadout.attributes(i).into_iter().collect());
                            }
                            None => rest.push(*id),
                        }
                    }
                }
                Err(e) => {
                    error!("dogma loadout: {}", crate::logging::chain(&e));
                    rest.extend_from_slice(item_ids);
                }
            }
        }
        None => rest.extend_from_slice(item_ids),
    }
    if !rest.is_empty() {
        out.extend(attributes_for(ctx, &rest).await);
    }
    live_vitals(ctx, &mut out);
    out
}

fn active_modules(ctx: &Ctx<'_>) -> HashSet<i64> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return HashSet::new();
    };
    ctx.world.lock().parks.get(&system)
        .and_then(|park| park.ships.get(&ship_id))
        .and_then(|ship| ship.fight.as_ref())
        .map(|fight| fight.applied.iter().copied().collect())
        .unwrap_or_default()
}

fn running_modules(ctx: &Ctx<'_>) -> Vec<crate::combat::Active> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Vec::new();
    };
    ctx.world.lock().parks.get(&system)
        .and_then(|park| park.ships.get(&ship_id))
        .and_then(|ship| ship.fight.as_ref())
        .map(|fight| fight.modules.clone()).unwrap_or_default()
}

/// In space, the ship's hit points and capacitor are the fight's, not the
/// database's.
fn live_vitals(ctx: &Ctx<'_>, attrs: &mut BTreeMap<i64, BTreeMap<i32, f64>>) {
    use crate::combat::attr;
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else { return };
    let Some(own) = attrs.get_mut(&ship_id) else { return };
    let world = ctx.world.lock();
    let Some(fight) = world.parks.get(&system).and_then(|p| p.ships.get(&ship_id)).and_then(|s| s.fight.as_ref()) else {
        return;
    };
    let now = crate::combat::now_ms();
    own.insert(attr::SHIELD_CHARGE, fight.vitals.shield.at(now));
    own.insert(attr::CHARGE, fight.vitals.capacitor.at(now));
    own.insert(attr::ARMOR_DAMAGE, fight.vitals.armor_damage);
    own.insert(attr::DAMAGE, fight.vitals.hull_damage);
}

async fn attributes_for(
    ctx: &Ctx<'_>,
    item_ids: &[i64],
) -> BTreeMap<i64, BTreeMap<i32, f64>> {
    const NUMERIC: &str = r#"'^\s*-?[0-9]+(\.[0-9]+)?([eE][-+]?[0-9]+)?\s*$'"#;
    let sql = format!(
        r#"
        WITH scope AS (
            SELECT e."itemID", e."typeID" FROM entity e WHERE e."itemID" = ANY($1)
        ), merged AS (
            SELECT s."itemID", ta."attributeID", 1 AS arm,
                   COALESCE(ea."valueFloat", ea."valueInt"::float8,
                            CASE WHEN ta.value ~ {NUMERIC} THEN ta.value::float8 END) AS v
              FROM scope s
              JOIN "dgmTypeAttribs"     ta  ON ta."typeID"      = s."typeID"
              JOIN "dgmAttributeTypes"  dat ON dat."attributeID" = ta."attributeID"
              LEFT JOIN entity_attributes ea
                     ON ea."itemID" = s."itemID" AND ea."attributeID" = ta."attributeID"
            UNION ALL
            SELECT ea."itemID", ea."attributeID", 2 AS arm,
                   COALESCE(ea."valueFloat", ea."valueInt"::float8) AS v
              FROM entity_attributes ea
              JOIN scope s ON s."itemID" = ea."itemID"
              JOIN "dgmAttributeTypes" dat ON dat."attributeID" = ea."attributeID"
        )
        SELECT DISTINCT ON ("itemID", "attributeID") "itemID", "attributeID", v
          FROM merged WHERE v IS NOT NULL ORDER BY "itemID", "attributeID", arm"#
    );
    let ids: Vec<i32> = item_ids.iter().filter_map(|i| i32::try_from(*i).ok()).collect();
    let mut out: BTreeMap<i64, BTreeMap<i32, f64>> = BTreeMap::new();
    match ctx.db.query(sql.as_str(), &[&ids]).await {
        Ok(rows) => {
            for r in &rows {
                let (Ok(item), Ok(attr), Ok(v)) = (
                    r.try_get::<_, i32>(0),
                    r.try_get::<_, i32>(1),
                    r.try_get::<_, f64>(2),
                ) else {
                    continue;
                };
                out.entry(item as i64).or_default().insert(attr, v);
            }
        }
        Err(e) => error!("dogmaIM attributes: {err}", err = crate::logging::chain(&e)),
    }
    out
}

fn attribute_dict(item: &Item, own: Option<&BTreeMap<i32, f64>>) -> Value {
    let mut attrs: BTreeMap<i32, f64> = own.cloned().unwrap_or_default();
    for (value_attr, tau_attr, cap_attr) in CHARGED {
        if attrs.contains_key(&value_attr) {
            continue;
        }
        let (Some(&tau), Some(&cap)) = (attrs.get(&tau_attr), attrs.get(&cap_attr)) else {
            continue;
        };
        if tau != 0.0 && cap != 0.0 {
            attrs.insert(value_attr, cap);
        }
    }
    let _ = item;
    Value::Dict(
        attrs.into_iter().map(|(id, v)| (Value::Int(id as i64),
            if id==crate::svc::boosters::EXPIRY {Value::Long(v as i64)} else {number(v)})).collect(),
    )
}

fn number(v: f64) -> Value {
    if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
        Value::Int(v as i64)
    } else {
        Value::Double(v)
    }
}

fn active_effects(item: &Item, own: Option<&BTreeMap<i32, f64>>, running: Option<&crate::combat::Active>) -> Value {
    let online = own.and_then(|a| a.get(&ATTR_IS_ONLINE)).copied().unwrap_or(0.0);
    let mut effects = Vec::new();
    let env = vec![
        Value::Int(item.item_id),
        Value::Int(item.owner_id),
        Value::Int(item.location_id),
        Value::None,
        Value::None,
        Value::List(Vec::new()),
        Value::Int(EFFECT_ONLINE),
    ];
    let mut d = env;
    d.push(Value::Long(crate::bulkdata::filetime_now()));
    d.push(Value::Int(0));
    d.push(Value::Int(0));
    d.push(Value::None);
    if online != 0.0 {
        effects.push((Value::Int(EFFECT_ONLINE), Value::List(d)));
    }
    if let Some(module) = running {
        effects.push((Value::Int(module.effect_id), Value::List(vec![
            Value::Int(item.item_id), Value::Int(item.owner_id), Value::Int(item.location_id),
            Value::int_or_none(module.target), Value::int_or_none(module.ammo.map(|ammo| ammo.item_id)),
            Value::List(Vec::new()), Value::Int(module.effect_id),
            Value::Long(crate::combat::filetime(module.started_ms)), Value::Int(module.duration_ms),
            Value::Int(module.repeat), Value::None,
        ])));
    }
    Value::Dict(effects)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ship(attrs: &[(i32, f64)]) -> (Item, BTreeMap<i32, f64>) {
        let item = Item {
            item_id: 140_000_016,
            type_id: 606,
            owner_id: 140_000_006,
            location_id: 60_014_689,
            group_id: 237,
            category_id: 6,
            booster_expiry: None,
            line: Vec::new(),
        };
        (item, attrs.iter().copied().collect())
    }

    fn get(v: &Value, key: i64) -> Option<&Value> {
        match v {
            Value::Dict(pairs) => {
                pairs.iter().find(|(k, _)| *k == Value::Int(key)).map(|(_, v)| v)
            }
            _ => None,
        }
    }

    fn row_values(value: &Value) -> &[Value] {
        let Value::Instance { state, .. } = value else { panic!("expected a row") };
        let Value::Dict(fields) = state.as_ref() else { panic!("expected row state") };
        fields.iter().find(|(key, _)| *key == Value::bytes("line"))
            .and_then(|(_, value)| value.seq()).expect("row line")
    }

    #[test]
    fn item_keys_accept_entities_and_fitted_charge_tuples() {
        assert_eq!(ItemKey::parse(&Value::Int(140_000_016)), Some(ItemKey::Entity(140_000_016)));
        let key = Value::Tuple(vec![Value::Long(140_000_016), Value::Int(27), Value::Int(230)]);
        let parsed = ItemKey::parse(&key).unwrap();
        assert_eq!(parsed, ItemKey::Sublocation { location: 140_000_016, flag: 27, type_id: 230 });
        assert_eq!(parsed.value(), Value::Tuple(vec![Value::Int(140_000_016), Value::Int(27), Value::Int(230)]));
        for invalid in [
            Value::None,
            Value::Int(0),
            Value::Int(-1),
            Value::Long(i64::MAX),
            Value::Tuple(vec![Value::Int(140_000_016), Value::Int(27)]),
            Value::Tuple(vec![Value::Int(140_000_016), Value::Int(5), Value::Int(230)]),
            Value::Tuple(vec![Value::Int(140_000_016), Value::Int(87), Value::Int(230)]),
            Value::Tuple(vec![Value::Int(140_000_016), Value::Int(27), Value::bytes("230")]),
        ] {
            assert_eq!(ItemKey::parse(&invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn a_sublocation_reply_keeps_its_tuple_and_current_stack_quantity() {
        let item = Item {
            item_id: 140_000_018, type_id: 230, owner_id: 140_000_006,
            location_id: 140_000_016, group_id: 85, category_id: 8,
            booster_expiry: None,
            line: vec![
                Value::Int(140_000_018), Value::Int(230), Value::Int(140_000_006),
                Value::Int(140_000_016), Value::Int(27), Value::Int(0), Value::Int(0),
                Value::Int(87), Value::Int(85), Value::Int(8), Value::None,
            ],
        };
        let key = ItemKey::Sublocation { location: 140_000_016, flag: 27, type_id: 230 };
        let attrs = BTreeMap::from([(item.item_id, BTreeMap::from([(ATTR_QUANTITY, 100.0), (114, 6.0)]))]);
        let value = info_row_for_key(&item, &key, &attrs, 123, None);
        let fields = row_values(&value);
        assert_eq!(fields[0], key.value());
        assert_eq!(row_values(&fields[1])[0], key.value());
        assert_eq!(get(&fields[2], 805), Some(&Value::Int(87)));
        assert_eq!(get(&fields[2], 114), Some(&Value::Int(6)));
        assert_eq!(fields[4], Value::Long(123));

        // The ordinary inventory ID remains usable by inventory change events.
        let numeric = info_row_for_key(&item, &ItemKey::Entity(item.item_id as i32), &attrs, 123, None);
        assert_eq!(row_values(&numeric)[0], Value::Int(item.item_id));
        assert_eq!(row_values(&row_values(&numeric)[1])[0], Value::Int(item.item_id));
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE with imported static data"]
    async fn fitted_ammo_is_resolved_by_tuple_and_rigs_are_in_ship_info() {
        let database = crate::test_support::Database::load().await;
        let owner = database.id().await;
        let ship_id = database.id().await;
        let charge = database.id().await;
        let rig = database.id().await;
        for (id, type_id, location, flag, quantity, singleton) in [
            (owner, 1373, ship_id, 57, 1, 1),
            (ship_id, 606, 60014689, 4, 1, 1),
            (charge, 230, ship_id, 27, 87, 0),
            (rig, 26286, ship_id, 92, 1, 1),
        ] {
            database.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton)
                VALUES ($1,$2,$3,$4,$5,$6,$7)"#, &[&id, &type_id, &owner, &location, &flag, &quantity, &singleton]).await.unwrap();
        }
        let session = crate::session::Session {
            char_id: Some(i64::from(owner)), ship_id: Some(i64::from(ship_id)),
            station_id: Some(60014689), ..Default::default()
        };
        let ctx = database.ctx(&session);
        let key = Value::Tuple(vec![Value::Int(i64::from(ship_id)), Value::Int(27), Value::Int(230)]);
        let info = item_info(&ctx, Some(&key)).await;
        let fields = row_values(&info);
        assert_eq!(fields[0], key);
        assert_eq!(get(&fields[2], 805), Some(&Value::Int(87)));
        let Value::Dict(ship) = ship_info(&ctx).await else { panic!("ship dictionary") };
        assert!(ship.iter().any(|(key, _)| *key == Value::Int(i64::from(rig))), "rig missing from ship snapshot");
        let wrong_type = Value::Tuple(vec![Value::Int(i64::from(ship_id)), Value::Int(27), Value::Int(231)]);
        assert_eq!(item_info(&ctx, Some(&wrong_type)).await, Value::None);
        database.db.execute(r#"UPDATE entity SET quantity = 0 WHERE "itemID" = $1"#, &[&charge]).await.unwrap();
        assert_eq!(item_info(&ctx, Some(&key)).await, Value::None);
    }

    #[test]
    fn synthesises_the_two_charged_attributes_at_full() {
        let (item, own) = ship(&[(55, 75000.0), (482, 100.0), (479, 500000.0), (263, 63.0)]);
        let d = attribute_dict(&item, Some(&own));
        assert_eq!(get(&d, 18), Some(&Value::Int(100)), "charge = capacitorCapacity");
        assert_eq!(get(&d, 264), Some(&Value::Int(63)), "shieldCharge = shieldCapacity");
    }

    #[test]
    fn an_override_beats_the_synthesised_full_value() {
        let (item, own) = ship(&[(55, 75000.0), (482, 100.0), (18, 42.5)]);
        let d = attribute_dict(&item, Some(&own));
        assert_eq!(get(&d, 18), Some(&Value::Double(42.5)));
    }

    #[test]
    fn no_charge_without_both_tau_and_cap() {
        let (item, own) = ship(&[(482, 100.0)]);
        assert_eq!(get(&attribute_dict(&item, Some(&own)), 18), None, "no tau");

        let (item, own) = ship(&[(55, 0.0), (482, 100.0)]);
        assert_eq!(get(&attribute_dict(&item, Some(&own)), 18), None, "zero tau");

        let (item, own) = ship(&[(55, 75000.0), (482, 0.0)]);
        assert_eq!(get(&attribute_dict(&item, Some(&own)), 18), None, "zero cap");
    }

    #[test]
    fn integral_values_travel_as_int() {
        assert_eq!(number(2.0), Value::Int(2));
        assert_eq!(number(-1.0), Value::Int(-1));
        assert_eq!(number(0.0), Value::Int(0));
        assert_eq!(number(2.5), Value::Double(2.5));
    }

    #[test]
    fn active_effects_follow_the_is_online_attribute() {
        let (item, own) = ship(&[(ATTR_IS_ONLINE, 0.0)]);
        assert_eq!(active_effects(&item, Some(&own), None), Value::Dict(Vec::new()));
        assert_eq!(active_effects(&item, None, None), Value::Dict(Vec::new()));

        let (item, own) = ship(&[(ATTR_IS_ONLINE, 1.0)]);
        let d = active_effects(&item, Some(&own), None);
        let Some(Value::List(env)) = get(&d, EFFECT_ONLINE) else {
            panic!("online effect missing: {d:?}");
        };
        assert!(env.len() >= 11, "activeEffects value is {} long", env.len());
        assert_eq!(env[0], Value::Int(140_000_016), "self");
        assert_eq!(env[1], Value::Int(140_000_006), "char/owner");
        assert_eq!(env[2], Value::Int(60_014_689), "ship/location");
        assert_eq!(env[5], Value::List(Vec::new()), "area is a list");
        assert_eq!(env[6], Value::Int(EFFECT_ONLINE));
    }

    #[test]
    fn an_item_refresh_preserves_the_running_effect_target_and_cycle() {
        let (item, own) = ship(&[(ATTR_IS_ONLINE, 1.0)]);
        let ty = crate::npc::NpcType::with(3634, 53, 1.0, 1.0, &[]);
        let mut active = crate::npc::entity_module(&ty, item.item_id, 34, "effects.ProjectileFired", crate::combat::Action::Passive, 1500.0);
        active.target = Some(140_000_099);
        active.started_ms = 12345;
        active.repeat = 1000;
        let effects = active_effects(&item, Some(&own), Some(&active));
        assert!(get(&effects, EFFECT_ONLINE).is_some());
        let env = get(&effects, 34).unwrap().seq().unwrap();
        assert_eq!(env[3], Value::Int(140_000_099));
        assert_eq!(env[6], Value::Int(34));
        assert_eq!(env[7], Value::Long(crate::combat::filetime(12345)));
        assert_eq!(env[8], Value::Int(1500));
        assert_eq!(env[9], Value::Int(1000));
        assert_eq!(env[10], Value::None);
    }
}
