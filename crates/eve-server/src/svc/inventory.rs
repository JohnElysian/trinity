use eve_marshal::Value;
use tokio_postgres::types::ToSql;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::bound;
use crate::svc::rows::{row, rowset};

const CONTAINER_WALLET: i64 = 10001;
const CONTAINER_GLOBAL: i64 = 10002;
const CONTAINER_SOLAR_SYSTEM: i64 = 10003;
pub const CONTAINER_HANGAR: i64 = 10004;
const CONTAINER_SCRAP_HEAP: i64 = 10005;
const CONTAINER_FACTORY: i64 = 10006;
const CONTAINER_BANK: i64 = 10007;
const CONTAINER_RECYCLER: i64 = 10008;
const CONTAINER_OFFICES: i64 = 10009;
const CONTAINER_STATION_CHARACTERS: i64 = 10010;
const CONTAINER_CHARACTER: i64 = 10011;
const CONTAINER_CORP_MARKET: i64 = 10012;

pub const FLAG_HANGAR: i32 = 4;
pub const FLAG_CORP_MARKET: i32 = 62;

const CATEGORY_BLUEPRINT: i32 = 9;

const STATION_ID_MIN: i32 = 60_000_000;
const STATION_ID_MAX: i32 = 64_000_000;

const UNLIMITED_CAPACITY: f64 = 9_000_000_000_000_000.0;

/// flagFactoryBlueprint: a blueprint installed in a job, out of every
/// listing until the job gives it back.
pub const FLAG_FACTORY_BLUEPRINT: i32 = 36;

/// What a blueprint item carries beyond the item row (GetSubContent reads
/// them off ListStationBlueprintItems); None for a blueprint never researched.
const BLUEPRINT_COLUMNS: &[&str] = &["copy", "materialLevel", "productivityLevel", "licensedProductionRunsRemaining"];

pub const INVITEM_HEADER: &[&str] = &[
    "itemID", "typeID", "ownerID", "locationID", "flag", "contraband", "singleton",
    "quantity", "groupID", "categoryID", "customInfo",
];

pub const INVITEM_SELECT: &str = r#"SELECT e."itemID",e."typeID",e."ownerID",e."locationID",e."flag",
                                       e."contraband",e."singleton",e."quantity",
                                       t."groupID",g."categoryID",e."customInfo"
                                  FROM "entity" e
                                  JOIN "invTypes" t ON t."typeID" = e."typeID"
                                  JOIN "invGroups" g ON g."groupID" = t."groupID"
                                 WHERE "#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Special { container_id: i64, owner_id: Option<i64> },
    Item(i64),
}

const BROKER: &str = "invbroker";

const CONTAINER: &str = "invcontainer";

pub async fn invbroker(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if matches!(call.method.as_str(), "AssembleConstructionPlatform" | "AssembleCargoContainer") {
        return Some(match crate::svc::outposts::assemble(ctx, call.arg_id(0)).await {
            Ok(()) => Outcome::Value(Value::None),
            Err(why) => Outcome::custom(why),
        });
    }
    Some(broker_method(ctx, &call.method, &call.args))
}

pub async fn invcontainer(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let container = container_from_params(&bound::params_of(call));
    Some(container_method(ctx, container, call).await)
}

fn broker_method(ctx: &Ctx<'_>, method: &str, args: &[Value]) -> Outcome {
    let arg = |i: usize| args.get(i).and_then(|v| v.as_i64());
    match method {
        "GetInventory" => Outcome::Value(bound::object_ref(
            ctx.node_id,
            CONTAINER,
            &[KIND_SPECIAL, arg(0).unwrap_or(0), arg(1).unwrap_or(NO_OWNER)],
        )),
        "GetInventoryFromId" | "GetInventoryFromIdEx" => Outcome::Value(bound::object_ref(
            ctx.node_id,
            CONTAINER,
            &[KIND_ITEM, arg(0).unwrap_or(0), NO_OWNER],
        )),
        _ => Outcome::Unknown(format!("{BROKER}::{method}")),
    }
}

async fn container_method(ctx: &Ctx<'_>, container: Container, call: &Call) -> Outcome {
    if let Some(outcome) = crate::svc::moves::container_write(ctx, container, call).await {
        return outcome;
    }
    let (method, args) = (call.method.as_str(), &call.args);
    let arg = |i: usize| args.get(i).and_then(|v| v.as_i64());
    // A starbase structure shows its contents only to those its settings let look.
    if let Container::Item(id) = container {
        use crate::svc::starbase::{may, Access};
        if method == "List" && may(ctx, id, Access::ViewPut).await == Some(false) && super::maintenance::view(ctx,id).await.is_none() {
            return Outcome::custom("You are not allowed to look inside that structure from here.");
        }
    }
    if let (Container::Item(id), "List") = (container, method) {
        if let Some(office) = crate::svc::corphangar::office(ctx.db, id).await {
            if !crate::svc::corphangar::mine(ctx, office) {
                return Outcome::custom("That is not your corporation's office.");
            }
            return Outcome::Value(list(ctx,container).await);
        }
    }
    let out = match method {
        "List" => list(ctx, container).await,

        "ListStations" => {
            list_stations(ctx, arg(0).unwrap_or(0) != 0, arg(1).unwrap_or(0) != 0).await
        }
        "ListStationItems" => list_station_items(ctx, arg(0).unwrap_or(0), false, false).await,
        "ListStationBlueprintItems" => {
            list_station_items(ctx, arg(0).unwrap_or(0), true, arg(1).unwrap_or(0) != 0).await
        }

        "GetItem" => get_item(ctx, container).await,
        "GetCapacity" => get_capacity(ctx, container, arg(0)).await,
        // An outpost platform in space: construction starts.
        "Build" => {
            let Container::Item(id) = container else { return Outcome::custom("Only a platform can be built.") };
            return match crate::svc::outposts::build(ctx, id).await {
                Ok(()) => Outcome::Value(Value::None),
                Err(why) => Outcome::custom(why),
            };
        }

        _ => return Outcome::Unknown(format!("{CONTAINER}::{method} on {container:?}")),
    };
    Outcome::Value(out)
}

const KIND_SPECIAL: i64 = 0;
const KIND_ITEM: i64 = 1;
const NO_OWNER: i64 = -1;

fn container_from_params(params: &[i64]) -> Container {
    match (params.first().copied(), params.get(1).copied(), params.get(2).copied()) {
        (Some(KIND_ITEM), Some(item_id), _) => Container::Item(item_id),
        (Some(KIND_SPECIAL), Some(container_id), owner) => Container::Special {
            container_id,
            owner_id: owner.filter(|o| *o != NO_OWNER),
        },
        _ => Container::Special { container_id: CONTAINER_HANGAR, owner_id: None },
    }
}

fn contents_scope(ctx: &Ctx<'_>, container: Container) -> Option<(String, Vec<i32>)> {
    match container {
        Container::Item(item_id) => {
            Some((r#"e."locationID" = $1"#.to_string(), vec![item_id as i32]))
        }
        Container::Special { container_id, owner_id } => match container_id {
            CONTAINER_HANGAR => {
                let station = ctx.session.station_id?;
                let owner = owner_id.or(ctx.session.char_id)?;
                Some((
                    r#"e."locationID" = $1 AND e."ownerID" = $2 AND e."flag" <> $3"#.to_string(),
                    vec![station as i32, owner as i32, FLAG_FACTORY_BLUEPRINT],
                ))
            }
            CONTAINER_CORP_MARKET => {
                let station = ctx.session.station_id?;
                let owner = owner_id.or(ctx.session.corp_id)?;
                Some((
                    r#"e."locationID" = $1 AND e."ownerID" = $2 AND e."flag" = $3"#.to_string(),
                    vec![station as i32, owner as i32, FLAG_CORP_MARKET],
                ))
            }
            CONTAINER_GLOBAL => {
                let owner = ctx.session.char_id?;
                Some((r#"e."ownerID" = $1 AND e."flag" <> $2"#.to_string(), vec![owner as i32, FLAG_FACTORY_BLUEPRINT]))
            }
            CONTAINER_CHARACTER => {
                let char_id = ctx.session.char_id?;
                Some((r#"e."locationID" = $1"#.to_string(), vec![char_id as i32]))
            }
            CONTAINER_WALLET
            | CONTAINER_SOLAR_SYSTEM
            | CONTAINER_SCRAP_HEAP
            | CONTAINER_FACTORY
            | CONTAINER_BANK
            | CONTAINER_RECYCLER
            | CONTAINER_OFFICES
            | CONTAINER_STATION_CHARACTERS => None,
            _ => None,
        },
    }
}

pub async fn list(ctx: &Ctx<'_>, container: Container) -> Value {
    if let Container::Item(id)=container {if super::maintenance::is_array(ctx.db,id).await.unwrap_or(false){
        if super::maintenance::view(ctx,id).await.is_none(){return rowset(INVITEM_HEADER,vec![]);}
        let sql=format!(r#"{INVITEM_SELECT} e."locationID"=$1 AND e.flag=0"#);
        return ctx.rowset(INVITEM_HEADER,&sql,&[&(id as i32)]).await;
    }}
    let ordinary=may_read(ctx,container).await;
    if !ordinary { if let Container::Item(id)=container {
        if Some(id)!=ctx.session.ship_id {
            if let Some((_,flag))=super::maintenance::view(ctx,id).await {
                let sql=format!(r#"{INVITEM_SELECT} e."locationID"=$1 AND e.flag=$2"#);
                return ctx.rowset(INVITEM_HEADER,&sql,&[&(id as i32),&(flag as i32)]).await;
            }
        }
    }}
    if !ordinary {
        return rowset(INVITEM_HEADER, Vec::new());
    }
    if let Container::Item(id) = container {
        if let Some((corp, _)) = crate::svc::corphangar::office(ctx.db, id).await {
            let flags = crate::svc::corphangar::visible_flags(ctx);
            let sql = format!(r#"{INVITEM_SELECT} e."locationID"=$1 AND e."ownerID"=$2 AND e.flag=ANY($3)"#);
            return ctx.rowset(INVITEM_HEADER, &sql, &[&(id as i32),&(corp as i32),&flags]).await;
        }
    }
    let Some((where_sql, vals)) = contents_scope(ctx, container) else {
        return rowset(INVITEM_HEADER, Vec::new());
    };
    let sql = format!("{}{} AND e.flag<>{FLAG_FACTORY_BLUEPRINT}", INVITEM_SELECT, where_sql);
    query_items(ctx, &sql, &vals).await
}

/// A bound inventory reference is an address, not an access grant. In particular,
/// guessing another pilot's ship ID must not reveal its cargo or fitted modules.
async fn may_read(ctx: &Ctx<'_>, container: Container) -> bool {
    let Some(me) = ctx.session.char_id else { return false };
    let Container::Item(mut id) = container else {
        return match container {
            Container::Special { container_id:CONTAINER_HANGAR,owner_id } => ctx.session.station_id.is_some() && owner_id.unwrap_or(me)==me,
            Container::Special { container_id:CONTAINER_CORP_MARKET,owner_id } => ctx.session.station_id.is_some() && ctx.session.corp_id.is_some_and(|corp|owner_id.unwrap_or(corp)==corp),
            Container::Special { container_id:CONTAINER_GLOBAL|CONTAINER_CHARACTER,owner_id } => owner_id.unwrap_or(me)==me,
            _ => false,
        };
    };
    let mut own_chain = true;
    let mut corp_chain = true;
    let mut visited = std::collections::HashSet::new();
    for _ in 0..8 {
        if !visited.insert(id) { return false; }
        if let Some(office) = crate::svc::corphangar::office(ctx.db,id).await {
            return crate::svc::corphangar::mine(ctx,office) && corp_chain;
        }
        if let Some(allowed) = crate::svc::starbase::may(ctx,id,crate::svc::starbase::Access::ViewPut).await {
            return allowed;
        }
        {
            let world = ctx.world.lock();
            if let Some(trade) = world.trades.get(&id) {
                return trade.traders.contains(&me) && ctx.session.station_id==Some(trade.station);
            }
        }
        if let (Some(system),Some(ship)) = (ctx.session.solar_system_id,ctx.session.ship_id) {
            if crate::loot::in_reach(ctx.world,system,ship,id) { return true; }
        }
        let row = ctx.db.query_opt(r#"SELECT "ownerID","locationID",flag FROM entity WHERE "itemID"=$1"#, &[&(id as i32)]).await;
        let Ok(Some(row)) = row else { return false };
        let owner = row.get::<_,i32>(0) as i64;
        let parent = row.get::<_,i32>(1) as i64;
        let flag = row.get::<_,i32>(2) as i64;
        if flag==i64::from(FLAG_FACTORY_BLUEPRINT) { return false; }
        own_chain &= owner==me;
        corp_chain &= Some(owner)==ctx.session.corp_id;
        if own_chain && (id==me || Some(id)==ctx.session.ship_id) { return true; }
        if let Some(office) = crate::svc::corphangar::office(ctx.db,parent).await {
            return corp_chain && crate::svc::corphangar::mine(ctx,office) && crate::svc::corphangar::may_query(ctx,flag);
        }
        if ctx.session.station_id==Some(parent) { return own_chain; }
        id=parent;
    }
    false
}

pub async fn list_station_items(
    ctx: &Ctx<'_>,
    station_id: i64,
    blueprints_only: bool,
    for_corp: bool,
) -> Value {
    let owner = match if for_corp { ctx.session.corp_id } else { ctx.session.char_id } {
        Some(o) => o,
        None => return rowset(INVITEM_HEADER, Vec::new()),
    };
    let mut where_sql = String::from(
        r#"e."ownerID" = $1 AND e."flag" <> $3
             AND (e."locationID" = $2
                  OR e."locationID" IN (SELECT c."itemID" FROM "entity" c
                                         WHERE c."locationID" = $2 AND c."ownerID" = $1))"#,
    );
    let mut vals = vec![owner as i32, station_id as i32, FLAG_FACTORY_BLUEPRINT];
    if for_corp {
        where_sql.push_str(" AND ");
        where_sql.push_str(&queryable_divisions(ctx,Some(station_id)));
    }
    if !blueprints_only {
        let sql = format!("{}{}", INVITEM_SELECT, where_sql);
        return query_items(ctx, &sql, &vals).await;
    }
    where_sql.push_str(r#" AND g."categoryID" = $4"#);
    vals.push(CATEGORY_BLUEPRINT);
    let select = INVITEM_SELECT.replace(
        r#"FROM "entity" e"#,
        r#", b."copy", b."materialLevel", b."productivityLevel", b."licensedProductionRunsRemaining"
           FROM "entity" e LEFT JOIN "invBlueprints" b ON b."itemID" = e."itemID""#,
    );
    let sql = format!("{select}{where_sql}");
    let params: Vec<&(dyn ToSql + Sync)> = vals.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
    let header: Vec<&str> = INVITEM_HEADER.iter().chain(BLUEPRINT_COLUMNS).copied().collect();
    match ctx.db.query(&sql, &params).await {
        Ok(rows) => {
            let lines = rows
                .iter()
                .map(|r| {
                    let mut cells = invitem_cells(r);
                    cells.extend((0..BLUEPRINT_COLUMNS.len()).map(|i| crate::svc::rows::cell(r, INVITEM_HEADER.len() + i)));
                    Value::List(cells)
                })
                .collect();
            rowset(&header, lines)
        }
        Err(e) => {
            error!("invbroker blueprint listing: {err}", err = crate::logging::chain(&e));
            rowset(&header, Vec::new())
        }
    }
}

pub async fn list_stations(ctx: &Ctx<'_>, blueprints_only: bool, for_corp: bool) -> Value {
    const HEADER: &[&str] = &["stationID", "itemCount"];
    let owner = match if for_corp { ctx.session.corp_id } else { ctx.session.char_id } {
        Some(o) => o,
        None => return rowset(HEADER, Vec::new()),
    };
    let mut vals: Vec<i32> = vec![owner as i32, STATION_ID_MIN, STATION_ID_MAX];
    let sql = if blueprints_only {
        vals.push(CATEGORY_BLUEPRINT);
        r#"SELECT e."locationID", count(*)::int AS "itemCount"
             FROM "entity" e
             JOIN "invTypes" t ON t."typeID" = e."typeID"
             JOIN "invGroups" g ON g."groupID" = t."groupID"
            WHERE e."ownerID" = $1 AND e."locationID" >= $2 AND e."locationID" < $3
              AND g."categoryID" = $4 AND e."flag" <> 36
            GROUP BY e."locationID""#
    } else {
        r#"SELECT e."locationID", count(*)::int AS "itemCount"
             FROM "entity" e
            WHERE e."ownerID" = $1 AND e."locationID" >= $2 AND e."locationID" < $3
            GROUP BY e."locationID""#
    };
    let sql = if for_corp {
        let hq=ctx.session.hq_id.unwrap_or(0);
        let base=ctx.session.base_id.unwrap_or(0);
        let flags=format!(r#" AND CASE WHEN e."locationID"={hq} THEN {} WHEN e."locationID"={base} THEN {} ELSE {} END GROUP BY e."locationID""#,
            queryable_divisions(ctx,Some(hq)),queryable_divisions(ctx,Some(base)),queryable_divisions(ctx,None));
        sql.replace(r#"GROUP BY e."locationID""#,&flags)
    } else {sql.to_string()};
    let params: Vec<&(dyn ToSql + Sync)> = vals.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
    match ctx.db.query(&sql, &params).await {
        Ok(rows) => {
            let lines = rows
                .iter()
                .map(|r| Value::List(vec![int_at(r, 0), int_at(r, 1)]))
                .collect();
            rowset(HEADER, lines)
        }
        Err(e) => {
            error!("invbroker ListStations: {err}", err = crate::logging::chain(&e));
            rowset(HEADER, Vec::new())
        }
    }
}

fn queryable_divisions(ctx:&Ctx<'_>,station:Option<i64>)->String {
    let flags=crate::svc::corphangar::flags().filter(|flag|crate::svc::corphangar::may_query_at(ctx,*flag,station)).map(|flag|flag.to_string()).collect::<Vec<_>>().join(",");
    if flags.is_empty() {"false".into()} else {format!("e.flag IN ({flags})")}
}

pub async fn get_item(ctx: &Ctx<'_>, container: Container) -> Value {
    let bay=match container{Container::Item(id)=>super::maintenance::view(ctx,id).await.is_some(),_=>false};
    if let Container::Item(id)=container {if !bay && super::maintenance::is_array(ctx.db,id).await.unwrap_or(false){return row(INVITEM_HEADER,vec![Value::None;INVITEM_HEADER.len()]);}}
    if !bay && !may_read(ctx,container).await { return row(INVITEM_HEADER, vec![Value::None; INVITEM_HEADER.len()]); }
    let item_id = match container {
        Container::Item(id) => id,
        Container::Special { .. } => match ctx.session.station_id {
            Some(s) => s,
            None => return row(INVITEM_HEADER, vec![Value::None; INVITEM_HEADER.len()]),
        },
    };
    let sql = format!("{}{}", INVITEM_SELECT, r#"e."itemID" = $1"#);
    match ctx.db.query(&sql, &[&(item_id as i32)]).await {
        Ok(rows) => match rows.first() {
            Some(r) => row(INVITEM_HEADER, invitem_cells(r)),
            None => row(INVITEM_HEADER, vec![Value::None; INVITEM_HEADER.len()]),
        },
        Err(e) => {
            error!("invbroker GetItem: {err}", err = crate::logging::chain(&e));
            row(INVITEM_HEADER, vec![Value::None; INVITEM_HEADER.len()])
        }
    }
}

pub async fn get_capacity(ctx: &Ctx<'_>, container: Container, flag: Option<i64>) -> Value {
    const HEADER: &[&str] = &["capacity", "used"];
    if let Container::Item(id)=container {
        if let Some((capacity,bay_flag))=super::maintenance::view(ctx,id).await {
            if flag==Some(bay_flag) || (bay_flag==0 && flag.is_none()) {
                let used=super::maintenance::used(ctx.db,id,bay_flag,None).await.unwrap_or(0.0);
                return row(HEADER,vec![Value::Double(capacity),Value::Double(used)]);
            }
        }
        if super::maintenance::is_array(ctx.db,id).await.unwrap_or(false){return row(HEADER,vec![Value::Double(0.0),Value::Double(0.0)]);}
    }
    if !may_read(ctx,container).await { return row(HEADER,vec![Value::Double(0.0),Value::Double(0.0)]); }

    let capacity = match container {
        Container::Item(item_id) => {
            // A silo's tower may raise its capacity.
            let silo = match crate::svc::moonmining::silo_capacity(ctx.db, item_id).await {
                Ok(silo) => silo,
                Err(e) => {
                    error!("invbroker GetCapacity silo: {err}", err = crate::logging::chain(&e));
                    None
                }
            };
            let sql = r#"SELECT t."capacity" FROM "entity" e
                           JOIN "invTypes" t ON t."typeID" = e."typeID"
                          WHERE e."itemID" = $1"#;
            match silo {
                Some(capacity) => capacity,
                None => match ctx.db.query(sql, &[&(item_id as i32)]).await {
                    Ok(rows) => rows
                        .first()
                        .and_then(|r| r.try_get::<_, f64>(0).ok())
                        .unwrap_or(0.0),
                    Err(e) => {
                        error!("invbroker GetCapacity capacity: {err}", err = crate::logging::chain(&e));
                        0.0
                    }
                },
            }
        }
        Container::Special { .. } => UNLIMITED_CAPACITY,
    };

    let used = match contents_scope(ctx, container) {
        Some((mut where_sql, mut vals)) => {
            if let Container::Item(id) = container {
                if crate::svc::corphangar::office(ctx.db,id).await.is_some() {
                    let visible = crate::svc::corphangar::visible_flags(ctx).into_iter().map(|flag|flag.to_string()).collect::<Vec<_>>().join(",");
                    where_sql.push_str(&if visible.is_empty() {" AND false".into()} else {format!(" AND e.flag IN ({visible})")});
                }
            }
            if let Some(f) = flag {
                where_sql.push_str(&format!(r#" AND e."flag" = ${}"#, vals.len() + 1));
                vals.push(f as i32);
            }
            let sql = format!(
                r#"SELECT e."typeID",t."groupID",e.singleton,e.quantity,COALESCE(t.volume,0)::float8
                     FROM "entity" e
                     JOIN "invTypes" t ON t."typeID" = e."typeID"
                    WHERE {where_sql}"#
            );
            let params: Vec<&(dyn ToSql + Sync)> =
                vals.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
            match ctx.db.query(&sql, &params).await {
                Ok(rows) => rows.iter().map(|r|super::maintenance::unit_volume(i64::from(r.get::<_,i32>(0)),r.get(1),r.get::<_,i32>(2)!=0,r.get(4))*f64::from(r.get::<_,i32>(3))).sum(),
                Err(e) => {
                    error!("invbroker GetCapacity used: {err}", err = crate::logging::chain(&e));
                    0.0
                }
            }
        }
        None => 0.0,
    };

    row(HEADER, vec![Value::Double(capacity), Value::Double(used)])
}

async fn query_items(ctx: &Ctx<'_>, sql: &str, vals: &[i32]) -> Value {
    let params: Vec<&(dyn ToSql + Sync)> = vals.iter().map(|v| v as &(dyn ToSql + Sync)).collect();
    match ctx.db.query(sql, &params).await {
        Ok(rows) => {
            let lines = rows.iter().map(|r| Value::List(invitem_cells(r))).collect();
            rowset(INVITEM_HEADER, lines)
        }
        Err(e) => {
            error!("invbroker item listing: {err}", err = crate::logging::chain(&e));
            rowset(INVITEM_HEADER, Vec::new())
        }
    }
}

#[cfg(test)]
mod custody_tests {
    use super::*;

    fn item_ids(value:Value)->Vec<i64> {
        match value { Value::Instance {state,..} => state.get_str("lines").and_then(Value::seq).unwrap_or(&[]).iter().filter_map(|r|r.seq()?.first()?.as_i64()).collect(),_=>vec![] }
    }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn inventory_custody_hides_private_and_unpermitted_hangars_and_keeps_job_blueprints() {
        let db=crate::test_support::Database::load().await;
        let me=db.id().await;let other=db.id().await;let corp=db.id().await;
        let office=db.id().await;let ship=db.id().await;let foreign_ship=db.id().await;
        let nested=db.id().await;let secret=db.id().await;let visible=db.id().await;let hidden=db.id().await;let blueprint=db.id().await;let corp_blueprint=db.id().await;
        let station:i32=db.db.query_one(r#"SELECT "stationID" FROM "staStations" ORDER BY "stationID" LIMIT 1"#,&[]).await.unwrap().get(0);
        db.db.execute(r#"INSERT INTO corporation ("corporationID","corporationName") VALUES($1,$2)"#,&[&corp,&format!("Custody {corp}")]).await.unwrap();
        db.db.execute(r#"INSERT INTO "crpOffices" ("corporationID","stationID","officeFolderID","itemID") VALUES($1,$2,$3,$3)"#,&[&corp,&station,&office]).await.unwrap();
        for (id,ty,owner,location,flag) in [(ship,587,me,station,4),(foreign_ship,587,other,station,4),(nested,3296,corp,office,4),(secret,34,other,foreign_ship,5),(visible,34,corp,office,4),(hidden,35,corp,office,116),(blueprint,691,me,station,36),(corp_blueprint,691,corp,station,4)] {
            db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton) VALUES($1,$2,$3,$4,$5,1,1)"#,&[&id,&ty,&owner,&location,&flag]).await.unwrap();
        }
        let mut session=crate::session::Session {char_id:Some(me as i64),ship_id:Some(ship as i64),corp_id:Some(corp as i64),station_id:Some(station as i64),hq_id:Some(station as i64+1),roles_at_hq:8_192|1_048_576,..Default::default()};
        let ctx=db.ctx(&session);
        assert!(item_ids(list(&ctx,Container::Item(foreign_ship as i64)).await).is_empty());
        assert!(item_ids(list(&ctx,Container::Special {container_id:CONTAINER_HANGAR,owner_id:Some(other as i64)}).await).is_empty());
        assert!(item_ids(list(&ctx,Container::Item(office as i64)).await).is_empty(),"HQ roles do not grant access at another office");
        assert!(!may_read(&ctx,Container::Item(nested as i64)).await);
        assert!(item_ids(list_station_items(&ctx,station as i64,true,true).await).is_empty(),"remote blueprint listing also checks the target station's roles");
        assert!(item_ids(list_stations(&ctx,true,true).await).is_empty());
        assert!(!item_ids(list(&ctx,Container::Special {container_id:CONTAINER_HANGAR,owner_id:None}).await).contains(&(blueprint as i64)));
        let call=Call {service:"invcontainer".into(),method:"Add".into(),args:vec![Value::Int(blueprint as i64)],kwargs:vec![]};
        let result=crate::svc::moves::container_write(&ctx,Container::Special {container_id:CONTAINER_HANGAR,owner_id:None},&call).await.unwrap();
        assert!(!matches!(result,Outcome::Value(_)),"ordinary moves cannot retrieve an installed blueprint");
        session.roles_at_other=1_048_576;
        let ctx=db.ctx(&session);
        assert_eq!(item_ids(list(&ctx,Container::Item(office as i64)).await).into_iter().collect::<std::collections::BTreeSet<_>>(),[nested as i64,visible as i64].into_iter().collect());
        assert!(may_read(&ctx,Container::Item(nested as i64)).await);
        assert_eq!(item_ids(list_station_items(&ctx,station as i64,true,true).await),vec![corp_blueprint as i64]);
        assert!(item_ids(list_stations(&ctx,true,true).await).contains(&(station as i64)));
        assert!(!crate::svc::corphangar::may_take(&ctx,4));
        session.roles_at_other=8_192;
        let ctx=db.ctx(&session);
        assert!(crate::svc::corphangar::may_take(&ctx,4));
        assert!(!crate::svc::corphangar::may_query(&ctx,4),"take and query are distinct original roles");
        session.roles_at_all=crate::session::CORP_ROLE_DIRECTOR;
        let ctx=db.ctx(&session);
        assert!(item_ids(list(&ctx,Container::Item(office as i64)).await).contains(&(hidden as i64)));
    }
}

/// Column positions in an item row (the client's const.ix*).
pub const IX_LOCATION_ID: i64 = 3;
pub const IX_QUANTITY: i64 = 7;

/// `OnItemChange(item, change)` arguments: the item as it is now and, keyed by
/// column position, the values it had before (invCache.py:111). An item whose
/// quantity is now 0 is gone; one whose old location is 0 is new.
pub fn item_change(cells: Vec<Value>, before: Vec<(i64, Value)>) -> Value {
    Value::Tuple(vec![
        row(INVITEM_HEADER, cells),
        Value::Dict(before.into_iter().map(|(ix, v)| (Value::Int(ix), v)).collect()),
    ])
}

pub fn invitem_cells(r: &tokio_postgres::Row) -> Vec<Value> {
    let mut cells: Vec<Value> = (0..10).map(|i| int_at(r, i)).collect();
    cells.push(match r.try_get::<_, Option<String>>(10) {
        Ok(Some(s)) => Value::Bytes(s.into_bytes()),
        _ => Value::None,
    });
    cells
}

fn int_at(r: &tokio_postgres::Row, i: usize) -> Value {
    match r.try_get::<_, i32>(i) {
        Ok(v) => Value::Int(v as i64),
        Err(_) => Value::None,
    }
}


