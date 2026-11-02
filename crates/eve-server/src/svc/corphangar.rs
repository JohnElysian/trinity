//! Corporation hangars: an office's seven divisions (flags 4 and 116 to
//! 121), named by the directors (corpRegistry.UpdateDivisionNames).
//!
//! Members of the corporation that rents an office reach it when docked in
//! its station. What goes in becomes the corporation's; what comes out
//! becomes the taker's. Looking into a division needs its Hangar Query role,
//! taking from it its Hangar Take role; directors may do anything.

use eve_marshal::Value;
use tokio_postgres::GenericClient;

use crate::services::Ctx;
use crate::session::CORP_ROLE_DIRECTOR;

pub const FLAG_HANGAR: i64 = 4;
/// Divisions 2 to 7.
const FLAG_CORP_SAG2: i64 = 116;
const FLAG_CORP_SAG7: i64 = 121;
/// corpRoleHangarCanTake1 and corpRoleHangarCanQuery1; the rest follow.
const ROLE_HANGAR_TAKE_1: i64 = 8_192;
const ROLE_HANGAR_QUERY_1: i64 = 1_048_576;

/// Which division (1 to 7) an office flag is.
pub fn division(flag: i64) -> Option<u32> {
    match flag {
        FLAG_HANGAR => Some(1),
        FLAG_CORP_SAG2..=FLAG_CORP_SAG7 => Some((flag - FLAG_CORP_SAG2 + 2) as u32),
        _ => None,
    }
}

/// The flags of the divisions.
pub fn flags() -> impl Iterator<Item = i64> {
    std::iter::once(FLAG_HANGAR).chain(FLAG_CORP_SAG2..=FLAG_CORP_SAG7)
}

/// The corporation and station of an office, if `id` is one.
pub async fn office(db: &impl GenericClient, id: i64) -> Option<(i64, i64)> {
    let r = db.query_opt(r#"SELECT "corporationID", "stationID" FROM "crpOffices" WHERE "itemID" = $1"#, &[&(id as i32)]).await.ok().flatten()?;
    Some((r.get::<_, i32>(0) as i64, r.get::<_, i32>(1) as i64))
}

/// Whether the caller is a member of the office's corporation, docked there.
pub fn mine(ctx: &Ctx<'_>, office: (i64, i64)) -> bool {
    ctx.session.corp_id == Some(office.0) && ctx.session.station_id == Some(office.1)
}

fn roles(ctx: &Ctx<'_>) -> i64 {
    ctx.session.roles_at_all | ctx.session.roles_at_hq | ctx.session.roles_at_base | ctx.session.roles_at_other
}

pub fn may_take(ctx: &Ctx<'_>, flag: i64) -> bool {
    let r = roles(ctx);
    r & CORP_ROLE_DIRECTOR != 0 || division(flag).is_some_and(|d| r & (ROLE_HANGAR_TAKE_1 << (d - 1)) != 0)
}

pub fn may_query(ctx: &Ctx<'_>, flag: i64) -> bool {
    let r = roles(ctx);
    r & CORP_ROLE_DIRECTOR != 0 || division(flag).is_some_and(|d| r & ((ROLE_HANGAR_QUERY_1 << (d - 1)) | (ROLE_HANGAR_TAKE_1 << (d - 1))) != 0)
}

/// The flags of an office the caller may look into.
pub fn visible_flags(ctx: &Ctx<'_>) -> Vec<i32> {
    flags().filter(|f| may_query(ctx, *f)).map(|f| f as i32).collect()
}

/// corpRegistry.UpdateDivisionNames(d1, ..., d7): directors name the divisions.
pub async fn update_division_names(ctx: &Ctx<'_>, args: &[Value]) -> Result<Value, String> {
    let corp = ctx.session.corp_id.ok_or("No corporation.")?;
    if !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err("Only directors may rename the divisions.".into());
    }
    let names: Vec<Option<String>> = (0..7).map(|i| args.get(i).and_then(|v| v.as_string()).map(|s| s.chars().take(50).collect())).collect();
    let old = ctx
        .query_one(r#"SELECT "division1","division2","division3","division4","division5","division6","division7" FROM "corporation" WHERE "corporationID" = $1"#, &[&(corp as i32)])
        .await;
    let ok = ctx
        .execute(
            r#"UPDATE "corporation" SET "division1" = COALESCE($2, "division1"), "division2" = COALESCE($3, "division2"), "division3" = COALESCE($4, "division3"),
                      "division4" = COALESCE($5, "division4"), "division5" = COALESCE($6, "division5"), "division6" = COALESCE($7, "division6"),
                      "division7" = COALESCE($8, "division7") WHERE "corporationID" = $1"#,
            &[&(corp as i32), &names[0], &names[1], &names[2], &names[3], &names[4], &names[5], &names[6]],
        )
        .await;
    if !ok {
        return Err("The division names could not be saved.".into());
    }
    let header = ["division1", "division2", "division3", "division4", "division5", "division6", "division7"];
    let before: Vec<Value> = (0..7).map(|i| Value::text(old.as_ref().and_then(|r| crate::svc::rows::text(r, i)).unwrap_or_default())).collect();
    let after: Vec<Value> = (0..7).map(|i| names[i].clone().map(Value::text).unwrap_or_else(|| before[i].clone())).collect();
    let change = crate::svc::rows::change(&header, Some(&before), Some(&after));
    let w = ctx.world.lock();
    w.notify(&w.corp_members(corp), "OnCorporationChanged", Value::Tuple(vec![Value::Int(corp), change]));
    tracing::info!("corporation {corp}'s divisions renamed");
    Ok(Value::None)
}

/// corpRegistry.UpdateCorporation(description, url, taxRate): directors set
/// the corporation's description, web page and the tax it takes.
pub async fn update_corporation(ctx: &Ctx<'_>, args: &[Value]) -> Result<Value, String> {
    let corp = ctx.session.corp_id.ok_or("No corporation.")?;
    if !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Err("Only directors may change the corporation's details.".into());
    }
    let description = args.first().and_then(|v| v.as_string()).unwrap_or_default();
    let url = args.get(1).and_then(|v| v.as_string()).unwrap_or_default();
    let tax = args.get(2).and_then(|v| v.as_f64()).unwrap_or(0.0).clamp(0.0, 1.0);
    let old = ctx.query_one(r#"SELECT "description", "url", "taxRate" FROM "corporation" WHERE "corporationID" = $1"#, &[&(corp as i32)]).await;
    if !ctx
        .execute(r#"UPDATE "corporation" SET "description" = $2, "url" = $3, "taxRate" = $4 WHERE "corporationID" = $1"#, &[&(corp as i32), &description, &url, &tax])
        .await
    {
        return Err("The corporation's details could not be saved.".into());
    }
    let header = ["description", "url", "taxRate"];
    let before = vec![
        Value::text(old.as_ref().and_then(|r| crate::svc::rows::text(r, 0)).unwrap_or_default()),
        Value::text(old.as_ref().and_then(|r| crate::svc::rows::text(r, 1)).unwrap_or_default()),
        Value::Double(old.as_ref().and_then(|r| crate::svc::rows::float(r, 2)).unwrap_or(0.0)),
    ];
    let after = vec![Value::text(description), Value::text(url), Value::Double(tax)];
    let change = crate::svc::rows::change(&header, Some(&before), Some(&after));
    let w = ctx.world.lock();
    w.notify(&w.corp_members(corp), "OnCorporationChanged", Value::Tuple(vec![Value::Int(corp), change]));
    Ok(Value::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn office_flags_are_divisions_one_to_seven() {
        assert_eq!(division(4), Some(1));
        assert_eq!(division(116), Some(2));
        assert_eq!(division(121), Some(7));
        assert_eq!(division(5), None);
        assert_eq!(flags().count(), 7);
    }
}
