mod offices;
pub(crate) use offices::{ensure_schema as ensure_office_schema,ensure_slots_locked as ensure_office_slots_locked};

use eve_marshal::Value;
use tracing::error;

use crate::services::{Call, Ctx, Outcome};
use crate::svc::bound;
use crate::svc::inventory::{FLAG_CORP_MARKET, INVITEM_HEADER, INVITEM_SELECT};
use crate::svc::rows::{cell, index_rowset, int, line, lines, row, rowset};

pub async fn registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    Some(corp_registry(ctx, &call.method, &call.args).await)
}

pub async fn station_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let station = bound::parse(&call.service)
        .and_then(|b| b.params.first().copied())
        .or(ctx.session.station_id)
        .unwrap_or(0);
    if let Some(done) = crate::svc::outposts::station_mgr(ctx, &call.method, &call.args, station).await {
        return Some(done.map_or_else(Outcome::custom, Outcome::Value));
    }
    Some(corp_station_mgr(ctx, &call.method, &call.args, Some(station)).await)
}

pub async fn corpmgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg0 = call.arg_id(0);
    let value = match call.method.as_str() {
        "GetCorporations" | "GetPublicInfo" => corporation_row(ctx, arg0).await,
        "GetCorporationIDForCharacter" => corporation_id_for_character(ctx, arg0).await,
        "GetCorporationWallet" => {
            return Some(Outcome::Unknown(
                "corpmgr::GetCorporationWallet needs a bound corp wallet object".into(),
            ))
        }
        "GetCorporationStations" => {
            let sql = r#"SELECT "stationID" AS "itemID", "solarSystemID" AS "locationID",
                                "stationTypeID" AS "typeID"
                           FROM "staStations" WHERE "corporationID" = $1 ORDER BY "stationID""#;
            let corp = ctx.session.corp_id.unwrap_or(0) as i32;
            ctx.rowset(&["itemID", "locationID", "typeID"], sql, &[&corp]).await
        }
        "GetItemsRented" => return Some(match items_rented(ctx).await {
            Ok(value)=>Outcome::Value(value),Err(error)=>{error!("corporation rentals: {}",crate::logging::chain(&error));Outcome::custom("The corporation's rentals could not be read.")}
        }),
        "SearchAssets" => search_assets(ctx,call).await,
        // (corpID, which): the stations holding the corporation's assets of a kind.
        "GetAssetInventory" => {
            let which = call.arg_text(1).unwrap_or_default();
            match own_corp(ctx, arg0) {
                Some(corp) => asset_locations(ctx, corp, &which).await,
                None => rowset(&["locationID"], Vec::new()),
            }
        }
        // (corpID, locationID, which): those assets at one station.
        "GetAssetInventoryForLocation" => {
            let which = call.arg_text(2).unwrap_or_default();
            match own_corp(ctx, arg0) {
                Some(corp) => asset_items(ctx, corp, call.arg_id(1), &which).await,
                None => rowset(INVITEM_HEADER, Vec::new()),
            }
        }
        // (memberID, fromDate, toDate, rowsPerPage): item and role audit trails.
        // Neither is recorded yet.
        "AuditMember" => Value::Tuple(vec![
            rowset(&["eventDateTime", "eventTypeID", "actorID", "itemID", "typeID", "quantity", "locationID"], Vec::new()),
            rowset(&["changeTime", "characterID", "issuerID", "oldRoles", "newRoles", "grantable"], Vec::new()),
        ]),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

/// The corporation ID if it is the caller's own; assets are not public.
fn own_corp(ctx: &Ctx<'_>, corp_id: i64) -> Option<i32> {
    (ctx.session.corp_id == Some(corp_id)).then_some(corp_id as i32)
}

/// The corporation asset and wallet views share these rental records.
async fn items_rented(ctx: &Ctx<'_>) -> Result<Value,tokio_postgres::Error> {
    let sql = r#"SELECT "typeID", "stationID" AS "rentedFromID", "startDate",
                  "rentPeriodInDays", "slotNumber" AS "flag", "periodCost", "balanceDueDate"
             FROM "rentalInfo" WHERE "renterID" = $1 AND "renterID">0 ORDER BY "stationID", "slotNumber""#;
    let corp = ctx.session.corp_id.unwrap_or(0) as i32;
    let header = ["typeID", "rentedFromID", "startDate", "rentPeriodInDays", "flag", "periodCost", "balanceDueDate"];
    // Earlier builds created offices without rentalInfo. Preserve existing
    // history while linking those offices under the station rental lock.
    let stations=ctx.db.query(r#"SELECT DISTINCT "stationID" FROM "crpOffices" WHERE "corporationID"=$1 AND "rentalSlot" IS NULL ORDER BY 1"#,&[&corp]).await?;
    if !stations.is_empty() {
        let mut db=crate::db::connect(ctx.db_url).await?;let tx=db.transaction().await?;
        for row in stations {
            let station:i32=row.get(0);
            if tx.query_opt(r#"SELECT 1 FROM "staStations" WHERE "stationID"=$1 FOR UPDATE"#,&[&station]).await?.is_some() {
                ensure_office_slots_locked(&tx,station).await?;
            }
        }
        tx.commit().await?;
    }
    Ok(rowset(&header,lines(&ctx.db.query(sql,&[&corp]).await?)))
}

/// SQL restricting entity `e` to one kind of corporation asset; $1 is the corp.
fn asset_scope(which: &str) -> Option<String> {
    Some(match which {
        "offices" => r#"e."ownerID"=$1 AND e."locationID" IN (SELECT "itemID" FROM "crpOffices" WHERE "corporationID" = $1)"#.into(),
        "property" => format!(
            r#"e."ownerID" = $1 AND e."flag" <> {FLAG_CORP_MARKET}
               AND e."locationID" IN (SELECT "solarSystemID" FROM "mapSolarSystems")"#
        ),
        "deliveries" => format!(r#"e."ownerID" = $1 AND e."flag" = {FLAG_CORP_MARKET}"#),
        _ => return None, // "junk", "jobs": nothing tracks these yet
    })
}

async fn search_assets(ctx:&Ctx<'_>,call:&Call)->Value {
    let empty=||rowset(&["locationID"],vec![]);
    let Some(corp)=ctx.session.corp_id else {return empty()};
    let Some(scope)=asset_scope(&call.arg_text(0).unwrap_or_default()) else {return empty()};
    let criterion=|i|call.arg_i64(i).and_then(|n|i32::try_from(n).ok()).filter(|n|*n>0);
    let category=criterion(1);let group=criterion(2);let ty=criterion(3);let quantity=criterion(4).unwrap_or(0);
    if category.is_none() && group.is_none() && ty.is_none() {return empty()}
    let sql=format!(r#"SELECT DISTINCT COALESCE(o."stationID",e."locationID") AS "locationID" FROM entity e
      JOIN "invTypes" t ON t."typeID"=e."typeID" JOIN "invGroups" g ON g."groupID"=t."groupID"
      LEFT JOIN "crpOffices" o ON o."itemID"=e."locationID" AND o."corporationID"=$1
      WHERE {scope} AND ($2::integer IS NULL OR g."categoryID"=$2) AND ($3::integer IS NULL OR t."groupID"=$3)
        AND ($4::integer IS NULL OR e."typeID"=$4) AND e.quantity>=$5 ORDER BY 1"#);
    ctx.rowset(&["locationID"],&sql,&[&(corp as i32),&category,&group,&ty,&quantity]).await
}

#[cfg(test)]
mod asset_tests {
    use super::*;
    fn locations(value:Value)->Vec<i64> {
        match value {Value::Instance {state,..}=>state.get_str("lines").and_then(Value::seq).unwrap_or(&[]).iter().filter_map(|line|line.seq()?.first()?.as_i64()).collect(),_=>vec![]}
    }
    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn corporation_asset_search_filters_owned_items_and_reports_office_stations() {
        let fixture=crate::test_support::Database::load().await;let corp=fixture.id().await;let office=fixture.id().await;
        fixture.db.execute(r#"INSERT INTO corporation ("corporationID","corporationName") VALUES ($1,$2)"#,&[&corp,&format!("Asset corporation {corp}")]).await.unwrap();
        fixture.db.execute(r#"INSERT INTO "crpOffices" ("corporationID","stationID","officeFolderID","itemID") VALUES ($1,60000004,$2,$2)"#,&[&corp,&office]).await.unwrap();
        fixture.db.execute(r#"INSERT INTO entity ("typeID","ownerID","locationID",quantity,flag) VALUES (34,$1,$2,5,4),(35,1,$2,100,4),(34,$1,30000142,9,0)"#,&[&corp,&office]).await.unwrap();
        let session=crate::session::Session {corp_id:Some(corp as i64),roles_at_all:1,..Default::default()};let ctx=fixture.ctx(&session);
        let mut call=Call {service:"corpmgr".into(),method:"SearchAssets".into(),args:vec![Value::bytes("offices"),Value::None,Value::None,Value::Int(34),Value::Int(5)],kwargs:vec![]};
        assert_eq!(locations(search_assets(&ctx,&call).await),vec![60000004]);
        call.args[4]=Value::Int(6);assert!(locations(search_assets(&ctx,&call).await).is_empty());
        call.args[3]=Value::Int(35);assert!(locations(search_assets(&ctx,&call).await).is_empty());
        call.args[0]=Value::bytes("property");call.args[3]=Value::Int(34);assert_eq!(locations(search_assets(&ctx,&call).await),vec![30000142]);
    }
}

async fn asset_locations(ctx: &Ctx<'_>, corp: i32, which: &str) -> Value {
    let Some(scope) = asset_scope(which) else { return rowset(&["locationID"], Vec::new()) };
    // Office contents sit in the office item; report the station it is in.
    let sql = format!(
        r#"SELECT DISTINCT COALESCE(o."stationID", e."locationID") AS "locationID"
             FROM entity e
             LEFT JOIN "crpOffices" o ON o."itemID" = e."locationID" AND o."corporationID" = $1
            WHERE {scope} ORDER BY 1"#
    );
    ctx.rowset(&["locationID"], &sql, &[&corp]).await
}

async fn asset_items(ctx: &Ctx<'_>, corp: i32, station_id: i64, which: &str) -> Value {
    let Some(scope) = asset_scope(which) else { return rowset(INVITEM_HEADER, Vec::new()) };
    let sql = format!(
        r#"{INVITEM_SELECT} {scope}
             AND (e."locationID" = $2 OR e."locationID" IN
                  (SELECT "itemID" FROM "crpOffices" WHERE "corporationID" = $1 AND "stationID" = $2))
           ORDER BY e."itemID""#
    );
    ctx.rowset(INVITEM_HEADER, &sql, &[&corp, &(station_id as i32)]).await
}

pub async fn corporation_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let arg0 = call.arg_id(0);
    let value = match call.method.as_str() {
        "GetNPCDivisions" => npc_divisions(ctx).await,
        "GetEmploymentRecord" => employment_record(ctx, arg0).await,
        "GetCorpInfo" => corp_market_info(),
        "GetFactionInfo" => faction_info(ctx).await,
        _ => return None,
    };
    Some(Outcome::Value(value))
}

async fn corp_registry(ctx: &Ctx<'_>, method: &str, args: &[Value]) -> Outcome {
    let arg0 = args.first().and_then(|v| v.as_i64());
    let corp = ctx.session.corp_id.unwrap_or(0);

    let out = match method {
        "GetCorporation" => corporation_row(ctx, corp).await,
        "GetEveOwners" => eve_owners(ctx, corp).await,
        "GetMember" => member(ctx, arg0.unwrap_or(0)).await,
        "GetRoles" => {
            let roles = roles(ctx).await;
            rowset(ROLE_HEADER, roles.iter().map(role_line).collect())
        }
        "GetRoleGroups" => role_groups(ctx).await,
        "GetSharesByShareholder" => shares_by_shareholder(ctx, arg0.unwrap_or(0) != 0).await,
        "GetInfoWindowDataForChar" => info_window_data_for_char(ctx, arg0.unwrap_or(0)).await,
        "GetTitles" => index_rowset(TITLE_HEADER, "titleID", Vec::new()),
        // The stations holding items locked down by a corporation vote.
        "GetLockedItemLocations" => crate::svc::votes::locked_locations(ctx).await,
        "GetMemberIDsWithMoreThanAvgShares" => Value::List(Vec::new()),

        "GetMembers" => members_rowset(ctx, corp).await,

        _ => return Outcome::Unknown(format!("corpRegistry::{method}")),
    };
    Outcome::Value(out)
}

const CORP_HEADER: &[&str] = &[
    "corporationID", "corporationName", "description", "tickerName", "url", "taxRate",
    "minimumJoinStanding", "corporationType", "hasPlayerPersonnelManager",
    "sendCharTerminationMessage", "creatorID", "ceoID", "stationID", "raceID", "allianceID",
    "shares", "memberCount", "memberLimit", "allowedMemberRaceIDs", "graphicID", "shape1",
    "shape2", "shape3", "color1", "color2", "color3", "typeface", "division1", "division2",
    "division3", "division4", "division5", "division6", "division7", "balance", "deleted",
];

// NPC corporations have no founder (creatorID 0, which no owner record
// matches); their CEO stands in, so the info window shows "CEO and Founder".
const CORP_SQL: &str = r#"SELECT "corporationID","corporationName","description","tickerName",
                                 "url","taxRate","minimumJoinStanding","corporationType",
                                 "hasPlayerPersonnelManager","sendCharTerminationMessage",
                                 COALESCE(NULLIF("creatorID", 0), "ceoID"),"ceoID","stationID","raceID","allianceID","shares",
                                 "memberCount","memberLimit","allowedMemberRaceIDs","graphicID",
                                 "shape1","shape2","shape3","color1","color2","color3","typeface",
                                 "division1","division2","division3","division4","division5",
                                 "division6","division7","balance","deleted"
                            FROM "corporation" WHERE "corporationID" = $1"#;

async fn corporation_row(ctx: &Ctx<'_>, corp_id: i64) -> Value {
    match ctx.db.query(CORP_SQL, &[&(corp_id as i32)]).await {
        Ok(rows) if !rows.is_empty() => row(CORP_HEADER, line(&rows[0])),
        Ok(_) => Value::None,
        Err(e) => {
            error!("GetCorporation({corp_id}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

async fn eve_owners(ctx: &Ctx<'_>, corp_id: i64) -> Value {
    const HEADER: &[&str] = &["ownerID", "ownerName", "typeID"];
    let sql = r#"SELECT "characterID","characterName","typeID"
                   FROM "character_" WHERE "corporationID" = $1"#;
    ctx.rowset(HEADER, sql, &[&(corp_id as i32)]).await
}

const MEMBER_HEADER: &[&str] = &[
    "characterID", "corporationID", "title", "startDateTime", "roles", "grantableRoles",
    "rolesAtHQ", "grantableRolesAtHQ", "rolesAtBase", "grantableRolesAtBase", "rolesAtOther",
    "grantableRolesAtOther", "baseID", "titleMask", "divisionID", "squadronID",
];

async fn member(ctx: &Ctx<'_>, char_id: i64) -> Value {
    match member_lines(ctx, MemberScope::One(char_id)).await.pop() {
        Some((_, line)) => row(MEMBER_HEADER, line),
        None => Value::None,
    }
}

enum MemberScope {
    One(i64),
    Corporation(i64),
}

/// Member rows (character ID, line), in character ID order.
async fn member_lines(ctx: &Ctx<'_>, scope: MemberScope) -> Vec<(i64, Vec<Value>)> {
    let (filter, id) = match scope {
        MemberScope::One(id) => (r#"c."characterID""#, id),
        MemberScope::Corporation(id) => (r#"c."corporationID""#, id),
    };
    let sql = format!(
        r#"SELECT c."characterID", c."corporationID", c."title", c."corporationDateTime",
                  COALESCE(r."rolesAtAll", 0), COALESCE(r."grantableRoles", 0),
                  COALESCE(r."rolesAtHQ", 0), COALESCE(r."grantableRolesAtHQ", 0),
                  COALESCE(r."rolesAtBase", 0), COALESCE(r."grantableRolesAtBase", 0),
                  COALESCE(r."rolesAtOther", 0), COALESCE(r."grantableRolesAtOther", 0),
                  co."stationID"
             FROM "character_" c
             JOIN "corporation" co ON co."corporationID" = c."corporationID"
             LEFT JOIN "chrCorporationRoles" r ON r."characterID" = c."characterID"
            WHERE {filter} = $1
            ORDER BY c."characterID""#
    );
    ctx.query(&sql, &[&(id as i32)])
        .await
        .iter()
        .map(|r| {
            let mask = |i: usize| Value::Long(r.get::<_, Option<i64>>(i).unwrap_or(0));
            let mut line = vec![cell(r, 0), cell(r, 1), cell(r, 2), cell(r, 3)];
            line.extend((4..12).map(mask));
            // baseID, titleMask, divisionID, squadronID.
            line.extend([cell(r, 12), Value::Long(0), Value::Int(1), Value::Int(1)]);
            (int(r, 0).unwrap_or(0), line)
        })
        .collect()
}

/// Bound object name of a corporation's member list.
const MEMBERS_PROVIDER: &str = "corpMembers";

/// GetMembers: a util.SparseRowset over a server-side provider, which the
/// client pages through with Fetch and FetchByKey (rowset.py).
async fn members_rowset(ctx: &Ctx<'_>, corp_id: i64) -> Value {
    let count = member_lines(ctx, MemberScope::Corporation(corp_id)).await.len() as i64;
    let provider = bound::object_ref_with(
        ctx.node_id,
        MEMBERS_PROVIDER,
        &[corp_id],
        vec![("realRowCount", Value::Int(count)), ("header", crate::svc::rows::header_value(MEMBER_HEADER))],
    );
    Value::Instance {
        class: Box::new(Value::bytes("util.SparseRowset")),
        state: Box::new(Value::Tuple(vec![crate::svc::rows::header_value(MEMBER_HEADER), provider, Value::Int(count)])),
    }
}

/// The member list provider: Fetch(start, count) gives [(pk, line)],
/// FetchByKey(pks) gives [(pk, position, line)].
pub async fn members_provider(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let corp_id = bound::parse(&call.service).and_then(|b| b.params.first().copied()).unwrap_or(0);
    if !matches!(call.method.as_str(), "Fetch" | "FetchByKey") {
        return None;
    }
    if ctx.session.corp_id != Some(corp_id) {
        return Some(Outcome::custom("That is not your corporation's member list."));
    }
    let lines = member_lines(ctx, MemberScope::Corporation(corp_id)).await;
    let value = if call.method == "Fetch" {
        let start = call.arg_i64(0).unwrap_or(0).max(0) as usize;
        let size = call.arg_i64(1).unwrap_or(0).max(0) as usize;
        lines
            .into_iter()
            .skip(start)
            .take(size)
            .map(|(pk, line)| Value::Tuple(vec![Value::Int(pk), Value::List(line)]))
            .collect()
    } else {
        let wanted: Vec<i64> = call.args.first().and_then(|v| v.seq()).unwrap_or(&[]).iter().filter_map(|v| v.as_i64()).collect();
        lines
            .into_iter()
            .enumerate()
            .filter(|(_, (pk, _))| wanted.contains(pk))
            .map(|(i, (pk, line))| Value::Tuple(vec![Value::Int(pk), Value::Int(i as i64), Value::List(line)]))
            .collect()
    };
    Some(Outcome::Value(Value::List(value)))
}

const ROLE_HEADER: &[&str] = &["roleID", "roleName", "shortDescription", "description"];

struct CorpRole {
    id: i64,
    name: String,
    short: String,
    long: String,
}

fn role_line(r: &CorpRole) -> Value {
    Value::List(vec![
        Value::Long(r.id),
        Value::Bytes(r.name.clone().into_bytes()),
        Value::Bytes(r.short.clone().into_bytes()),
        Value::Bytes(r.long.clone().into_bytes()),
    ])
}

fn role_text(name: &str) -> Option<(String, String)> {
    let divisional = |prefix: &str| -> Option<u32> {
        let d = name.strip_prefix(prefix)?;
        match d.parse::<u32>() {
            Ok(n) if (1..=7).contains(&n) && d.len() == 1 => Some(n),
            _ => None,
        }
    };
    if let Some(n) = divisional("corpRoleHangarCanTake") {
        return Some((
            format!("Hangar Take Division {n}"),
            format!("May take items out of corporation hangar division {n}"),
        ));
    }
    if let Some(n) = divisional("corpRoleHangarCanQuery") {
        return Some((
            format!("Hangar Query Division {n}"),
            format!("May look at the contents of corporation hangar division {n}"),
        ));
    }
    if let Some(n) = divisional("corpRoleAccountCanTake") {
        return Some((
            format!("Account Take Division {n}"),
            format!("May withdraw funds from corporation account division {n}"),
        ));
    }
    let general = match name {
        "corpRoleDirector" => ("Director", "Has every role in the corporation"),
        "corpRolePersonnelManager" => {
            ("Personnel Manager", "May accept and reject applications to the corporation")
        }
        "corpRoleAccountant" => ("Accountant", "May view the corporation wallet and journal"),
        "corpRoleSecurityOfficer" => {
            ("Security Officer", "May set corporation standings and declare war")
        }
        "corpRoleFactoryManager" => {
            ("Factory Manager", "May rent and operate the corporation's factory slots")
        }
        "corpRoleStationManager" => {
            ("Station Manager", "May manage the corporation's stations")
        }
        "corpRoleCanRentOffice" => ("Rent Office", "May rent offices for the corporation"),
        "corpRoleAuditor" => ("Auditor", "May read the corporation's member audit log"),
        _ => return None,
    };
    Some((general.0.to_string(), general.1.to_string()))
}

async fn roles(ctx: &Ctx<'_>) -> Vec<CorpRole> {
    let sql = r#"SELECT "constantID","constantValue" FROM "eveConstants"
                  WHERE "constantID" LIKE 'corpRole%' AND "constantValue" <> 4294967295
                  ORDER BY "constantValue""#;
    let rows = match ctx.db.query(sql, &[]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("GetRoles: {err}", err = crate::logging::chain(&e));
            Vec::new()
        }
    };
    let mut out = Vec::new();
    for r in &rows {
        let name: String = r.get(0);
        let id: i64 = r.get(1);
        if let Some((short, long)) = role_text(&name) {
            out.push(CorpRole { id, name, short, long });
        }
    }
    out
}

const ROLE_GROUP_HEADER: &[&str] =
    &["roleGroupID", "roleGroupName", "roleMask", "isDivisional", "appliesTo", "appliesToGrantable"];

async fn role_groups(ctx: &Ctx<'_>) -> Value {
    let roles = roles(ctx).await;
    let mask_of = |pred: &dyn Fn(&str) -> bool| -> i64 {
        roles.iter().filter(|r| pred(&r.name)).fold(0i64, |acc, r| acc | r.id)
    };
    let general = mask_of(&|n: &str| {
        !n.starts_with("corpRoleHangarCan") && !n.starts_with("corpRoleAccountCan")
    });
    let hangar = mask_of(&|n: &str| n.starts_with("corpRoleHangarCan"));
    let account = mask_of(&|n: &str| n.starts_with("corpRoleAccountCan"));

    let groups: &[(i64, &str, i64, i64, &str, &str)] = &[
        (0, "General Roles", general, 0, "roles", "grantableRoles"),
        (1, "Accounting Roles", account, 0, "roles", "grantableRoles"),
        (2, "Hangar Roles at HQ", hangar, 1, "rolesAtHQ", "grantableRolesAtHQ"),
        (4, "Hangar Roles at Base", hangar, 1, "rolesAtBase", "grantableRolesAtBase"),
        (6, "Hangar Roles at Other", hangar, 1, "rolesAtOther", "grantableRolesAtOther"),
    ];
    let lines = groups
        .iter()
        .map(|(id, name, mask, divisional, applies, grantable)| {
            Value::List(vec![
                Value::Int(*id),
                Value::Bytes(name.as_bytes().to_vec()),
                Value::Long(*mask),
                Value::Int(*divisional),
                Value::Bytes(applies.as_bytes().to_vec()),
                Value::Bytes(grantable.as_bytes().to_vec()),
            ])
        })
        .collect();
    rowset(ROLE_GROUP_HEADER, lines)
}

async fn shares_by_shareholder(ctx: &Ctx<'_>, corp_shares: bool) -> Value {
    const HEADER: &[&str] = &["corporationID", "shares"];
    if corp_shares {
        return rowset(HEADER, Vec::new());
    }
    let Some(char_id) = ctx.session.char_id else { return rowset(HEADER, Vec::new()) };
    let sql = r#"SELECT "corporationID","shares" FROM "crpCharShares" WHERE "characterID" = $1"#;
    ctx.rowset(HEADER, sql, &[&(char_id as i32)]).await
}

async fn info_window_data_for_char(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "corpID", "allianceID", "title", "title1", "title2", "title3", "title4", "title5",
        "title6", "title7", "title8",
    ];
    let sql = r#"SELECT "character_"."corporationID", "corporation"."allianceID",
                        "character_"."title"
                   FROM "character_"
                   JOIN "corporation"
                     ON "corporation"."corporationID" = "character_"."corporationID"
                  WHERE "character_"."characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) if !rows.is_empty() => {
            let r = &rows[0];
            let mut line = line(r);
            line.resize(HEADER.len(), Value::None);
            row(HEADER, line)
        }
        Ok(_) => Value::None,
        Err(e) => {
            error!("GetInfoWindowDataForChar({char_id}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

async fn corporation_id_for_character(ctx: &Ctx<'_>, char_id: i64) -> Value {
    let sql = r#"SELECT "corporationID" FROM "character_" WHERE "characterID" = $1"#;
    match ctx.db.query(sql, &[&(char_id as i32)]).await {
        Ok(rows) if !rows.is_empty() => cell(&rows[0], 0),
        Ok(_) => Value::None,
        Err(e) => {
            error!("GetCorporationIDForCharacter({char_id}): {err}", err = crate::logging::chain(&e));
            Value::None
        }
    }
}

async fn npc_divisions(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &["divisionID", "divisionName", "description", "leaderType"];
    let sql = r#"SELECT "divisionID","divisionName","description","leaderType"
                   FROM "crpNPCDivisions" ORDER BY "divisionID""#;
    ctx.rowset(HEADER, sql, &[]).await
}

async fn employment_record(ctx: &Ctx<'_>, char_id: i64) -> Value {
    // `deleted` is whether the corporation has closed: the info window marks
    // it so (infosvc.py GetEmploymentHistorySubContent).
    const HEADER: &[&str] = &["corporationID", "startDate", "deleted"];
    let sql = r#"SELECT e."corporationID", e."startDate", COALESCE(c."deleted", 0)::int
                   FROM "chrEmployment" e
                   LEFT JOIN "corporation" c ON c."corporationID" = e."corporationID"
                  WHERE e."characterID" = $1 AND e."deleted" = 0
                  ORDER BY e."startDate" DESC"#;
    ctx.rowset(HEADER, sql, &[&(char_id as i32)]).await
}

fn corp_market_info() -> Value {
    const HEADER: &[&str] = &[
        "typeID", "sellPrice", "sellQuantity", "sellDateTime", "sellStationID", "buyPrice",
        "buyQuantity", "buyDateTime", "buyStationID",
    ];
    rowset(HEADER, Vec::new())
}

async fn faction_info(ctx: &Ctx<'_>) -> Value {
    let by_corp = int_pairs(
        ctx,
        r#"SELECT "corporationID","factionID" FROM "crpNPCCorporations"
            WHERE "factionID" IS NOT NULL"#,
    )
    .await;
    let regions = int_lists(
        ctx,
        r#"SELECT "factionID","regionID" FROM "mapRegions" WHERE "factionID" IS NOT NULL"#,
    )
    .await;
    let constellations = int_lists(
        ctx,
        r#"SELECT "factionID","constellationID" FROM "mapConstellations"
            WHERE "factionID" IS NOT NULL"#,
    )
    .await;
    let solar_systems = int_lists(
        ctx,
        r#"SELECT "factionID","solarSystemID" FROM "mapSolarSystems"
            WHERE "factionID" IS NOT NULL"#,
    )
    .await;
    let races = int_lists(ctx, r#"SELECT "factionID","raceID" FROM "factionRaces""#).await;
    let station_count = int_pairs(
        ctx,
        r#"SELECT "factionID","stationCount" FROM "chrFactions"
            WHERE "stationCount" IS NOT NULL"#,
    )
    .await;
    let system_count = int_pairs(
        ctx,
        r#"SELECT "factionID","stationSystemCount" FROM "chrFactions"
            WHERE "stationSystemCount" IS NOT NULL"#,
    )
    .await;

    const NPC_HEADER: &[&str] = &["corporationID", "size", "extent"];
    let sql = r#"SELECT "corporationID","size","extent" FROM "crpNPCCorporations""#;
    let npc_info = match ctx.db.query(sql, &[]).await {
        Ok(rows) => rows
            .iter()
            .map(|r| (cell(r, 0), row(NPC_HEADER, line(r))))
            .collect(),
        Err(e) => {
            error!("GetFactionInfo(npcCorpInfo): {err}", err = crate::logging::chain(&e));
            Vec::new()
        }
    };

    Value::Tuple(vec![
        Value::Dict(by_corp),
        Value::Dict(regions),
        Value::Dict(constellations),
        Value::Dict(solar_systems),
        Value::Dict(races),
        Value::Dict(station_count),
        Value::Dict(system_count),
        Value::Dict(npc_info),
    ])
}

async fn int_pairs(ctx: &Ctx<'_>, sql: &str) -> Vec<(Value, Value)> {
    match ctx.db.query(sql, &[]).await {
        Ok(rows) => rows
            .iter()
            .map(|r| {
                (
                    cell(r, 0),
                    cell(r, 1),
                )
            })
            .collect(),
        Err(e) => {
            error!("{sql}: {err}", err = crate::logging::chain(&e));
            Vec::new()
        }
    }
}

async fn int_lists(ctx: &Ctx<'_>, sql: &str) -> Vec<(Value, Value)> {
    let rows = match ctx.db.query(sql, &[]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("{sql}: {err}", err = crate::logging::chain(&e));
            return Vec::new();
        }
    };
    let mut keys: Vec<i64> = Vec::new();
    let mut vals: Vec<Vec<Value>> = Vec::new();
    for r in &rows {
        let k = r.get::<_, i32>(0) as i64;
        let v = cell(r, 1);
        match keys.iter().position(|x| *x == k) {
            Some(i) => vals[i].push(v),
            None => {
                keys.push(k);
                vals.push(vec![v]);
            }
        }
    }
    keys.into_iter().zip(vals).map(|(k, v)| (Value::Int(k), Value::List(v))).collect()
}

const TITLE_HEADER: &[&str] = &[
    "titleID", "titleName", "roles", "grantableRoles", "rolesAtHQ", "grantableRolesAtHQ",
    "rolesAtBase", "grantableRolesAtBase", "rolesAtOther", "grantableRolesAtOther",
];

async fn corp_station_mgr(
    ctx: &Ctx<'_>,
    method: &str,
    args: &[Value],
    station: Option<i64>,
) -> Outcome {
    let station = station.or(ctx.session.station_id).unwrap_or(0);
    let arg0 = args.first().and_then(|v| v.as_i64());

    let out = match method {
        "GetCorporateStationInfo" => corporate_station_info(ctx, station).await,

        "GetQuoteForRentingAnOffice" => return offices::quote(ctx,station,args).await,

        "DoesPlayersCorpHaveJunkAtStation" => Value::Int(0),
        "GetQuoteForGettingCorpJunkBack" => Value::Double(0.0),

        "DoStandingCheckForStationService" => Value::None,

        "GetPotentialHomeStations" => potential_home_stations(ctx, station).await,

        "AreAnyFactorySlotsAvailableForRenting"
        | "AreAnyResearchFacilitySlotsAvailableForRenting" => Value::Int(0),
        "GetQuoteForRentingAFactorySlot" | "GetQuoteForRentingAResearchFacilitySlot" => {
            Value::Double(0.0)
        }

        "SetCloneTypeID" => return crate::svc::medical::set_clone_type(ctx, arg0.unwrap_or(0)).await,
        "SetHomeStation" => return crate::svc::medical::set_home_station(ctx, arg0.unwrap_or(0)).await,
        "RentOffice" => return offices::rent(ctx,station,args).await,

        _ => return Outcome::Unknown(format!("corpStationMgr::{method}")),
    };
    Outcome::Value(out)
}

async fn corporate_station_info(ctx: &Ctx<'_>, station: i64) -> Value {
    const OFFICE_HEADER: &[&str] = &["corporationID", "itemID", "officeFolderID"];

    let station = station as i32;

    let office_sql = r#"SELECT "corporationID","itemID","officeFolderID"
                          FROM "crpOffices" WHERE "stationID" = $1"#;
    let offices = ctx.rowset(OFFICE_HEADER, office_sql, &[&station]).await;

    let corp_sql = CORP_SQL.replace(
        r#"WHERE "corporationID" = $1"#,
        r#"WHERE "stationID" = $1
              OR "corporationID" IN (SELECT "corporationID" FROM "crpOffices"
                                      WHERE "stationID" = $1)"#,
    );
    let corp_rows = match ctx.db.query(corp_sql.as_str(), &[&station]).await {
        Ok(rows) => rows,
        Err(e) => {
            error!("GetCorporateStationInfo({station}) corporations: {err}", err = crate::logging::chain(&e));
            Vec::new()
        }
    };
    let corporations =
        rowset(CORP_HEADER, lines(&corp_rows));

    let mut ids: Vec<i32> = Vec::new();
    for r in &corp_rows {
        for i in [
            0usize,
            10,
            11,
        ] {
            if let Ok(Some(id)) = r.try_get::<_, Option<i32>>(i) {
                if id != 0 && !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    let eveowners = eve_owners_by_id(ctx, &ids).await;

    Value::Tuple(vec![eveowners, corporations, offices])
}

async fn eve_owners_by_id(ctx: &Ctx<'_>, ids: &[i32]) -> Value {
    const HEADER: &[&str] = &["ownerID", "ownerName", "typeID"];
    if ids.is_empty() {
        return rowset(HEADER, Vec::new());
    }
    const SQL: &str = r#"
        SELECT k.id AS "ownerID",
               COALESCE(o."ownerName", c."characterName", co."corporationName") AS "ownerName",
               COALESCE(NULLIF(o."typeID", 0), NULLIF(c."typeID", 0), bt."typeID",
                        CASE WHEN co."corporationID" IS NOT NULL THEN 2 END) AS "typeID"
          FROM unnest($1::int[]) AS k(id)
          LEFT JOIN "eveStaticOwners" o  ON o."ownerID"        = k.id
          LEFT JOIN "character_"      c  ON c."characterID"    = k.id
          LEFT JOIN "bloodlineTypes" bt  ON bt."bloodlineID"   = c."bloodlineID"
          LEFT JOIN "corporation"    co  ON co."corporationID" = k.id
         WHERE COALESCE(o."ownerName", c."characterName", co."corporationName") IS NOT NULL
           AND COALESCE(NULLIF(o."typeID", 0), NULLIF(c."typeID", 0), bt."typeID",
                        CASE WHEN co."corporationID" IS NOT NULL THEN 2 END) IS NOT NULL"#;
    ctx.rowset(HEADER, SQL, &[&ids]).await
}

async fn potential_home_stations(ctx: &Ctx<'_>, station: i64) -> Value {
    const HEADER: &[&str] = &["stationID", "typeID"];
    let sql = r#"SELECT "stationID","stationTypeID" AS "typeID"
                   FROM "staStations" WHERE "stationID" = $1"#;
    ctx.rowset(HEADER, sql, &[&(station as i32)]).await
}
