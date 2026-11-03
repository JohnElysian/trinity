//! `insuranceSvc`, unbound and bound per station (base_insurance.py). Quotes are
//! computed by the client; the server keeps the contracts.

use eve_marshal::Value;

use crate::items::ItemTxn;
use crate::services::{Call, Ctx, Outcome};
use crate::session::{CORP_ROLE_ACCOUNTANT, CORP_ROLE_DIRECTOR};
use crate::svc::rows::{line, row, rowset};
use crate::svc::wallet::{self, Entry, Transfer};

const CORP_ROLE_JUNIOR_ACCOUNTANT: i64 = 4_503_599_627_370_496;

const HEADER: &[&str] = &[
    "contractID",
    "shipID",
    "ownerID",
    "startDate",
    "endDate",
    "fraction",
    "amount",
    "payout",
    "isCorp",
];
const SELECT: &str = r#"SELECT "contractID","shipID","ownerID","startDate","endDate","fraction",
                               "amount","payout","isCorp" FROM "insuranceContracts""#;

pub async fn insurance_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        // GetContracts(isCorp=0): the caller's contracts, or its corporation's.
        "GetContracts" => {
            let corp = call.arg_i64(0).unwrap_or(0) != 0;
            if corp
                && ctx.session.corp_role()
                    & (CORP_ROLE_ACCOUNTANT | CORP_ROLE_JUNIOR_ACCOUNTANT | CORP_ROLE_DIRECTOR)
                    == 0
            {
                return Some(insure_failed(
                    "You do not have access to corporation insurance.",
                ));
            }
            let owner = if corp {
                ctx.session.corp_id
            } else {
                ctx.session.char_id
            };
            let Some(owner) = owner else {
                return Some(Outcome::Value(rowset(HEADER, vec![])));
            };
            let sql = format!(
                r#"{SELECT} WHERE "ownerID" = $1 AND "endDate" > $2 ORDER BY "contractID""#
            );
            ctx.rowset(
                HEADER,
                &sql,
                &[&(owner as i32), &crate::bulkdata::filetime_now()],
            )
            .await
        }
        // infosvc reads contract.ownerID/.fraction/.startDate/.endDate, or None.
        "GetContractForShip" => {
            let sql = format!(r#"{SELECT} WHERE "shipID" = $1 AND "endDate" > $2"#);
            match ctx
                .query_one(
                    &sql,
                    &[&(call.arg_id(0) as i32), &crate::bulkdata::filetime_now()],
                )
                .await
            {
                Some(r) => row(HEADER, line(&r)),
                None => Value::None,
            }
        }
        // InsureShip(shipID, amount, isCorp): the quote picked (base_insurance.py
        // GetQuoteForShip: 5% of the hull's base price buys 50% cover, each
        // further 5% another 10%).
        "InsureShip" => {
            let is_corp = call.arg_i64(2).unwrap_or(0) != 0;
            let amount = call.args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let void_old = call
                .kwargs
                .iter()
                .find(|(k, _)| k.as_bytes() == Some(b"voidOld"))
                .and_then(|(_, v)| v.as_i64())
                .or_else(|| call.arg_i64(3))
                .unwrap_or(0)
                != 0;
            return Some(insure(ctx, call.arg_id(0), amount, is_corp, void_old).await);
        }
        "UnInsureShip" => {
            return Some(
                uninsure(ctx, call.arg_id(0))
                    .await
                    .map_or_else(|e| e, |_| Outcome::Value(Value::None)),
            );
        }
        _ => return None,
    };
    Some(Outcome::Value(value))
}

const REF_INSURANCE: i32 = 19;
const CONTRACT_WEEKS: i64 = 12;

/// Match the client's repeated addition of five percent, allowing only floating
/// point rounding error. Always charge the canonical price for that level.
fn quote(amount: f64, base_price: f64) -> Option<(f64, f64)> {
    let step = base_price * 0.05;
    if !amount.is_finite() || !base_price.is_finite() || amount <= 0.0 || step <= 0.0 {
        return None;
    }
    (1..=6).find_map(|n| {
        let premium = step * n as f64;
        ((amount - premium).abs() <= premium.abs() * f64::EPSILON * 8.0)
            .then_some((premium, (4 + n) as f64 / 10.0))
    })
}

async fn insure(
    ctx: &Ctx<'_>,
    ship_id: i64,
    amount: f64,
    is_corp: bool,
    void_old: bool,
) -> Outcome {
    match purchase(ctx, ship_id, amount, is_corp, void_old).await {
        Ok(owner) => {
            wallet::notify_balance(ctx, owner).await;
            Outcome::Value(Value::None)
        }
        Err(error) => error,
    }
}

async fn purchase(
    ctx: &Ctx<'_>,
    ship_id: i64,
    amount: f64,
    is_corp: bool,
    void_old: bool,
) -> Result<i64, Outcome> {
    let (Some(char_id), Some(station)) = (ctx.session.char_id, ctx.session.station_id) else {
        return Err(insure_failed("Ships can only be insured in a station."));
    };
    if ship_id <= 0 || ship_id > i32::MAX as i64 {
        return Err(insure_failed("That is not a ship."));
    }
    if is_corp && !may_manage_corp(ctx) {
        return Err(insure_failed(
            "Only a corporation Accountant may insure its ships.",
        ));
    }
    let owner = if is_corp {
        ctx.session.corp_id.unwrap_or(0)
    } else {
        char_id
    };
    let mut client = crate::db::connect(ctx.db_url).await.map_err(db_failed)?;
    let mut tx = ItemTxn::begin(&mut client).await.map_err(db_failed)?;
    let ship = tx
        .item(ship_id)
        .await
        .map_err(db_failed)?
        .ok_or_else(|| insure_failed("That ship no longer exists."))?;
    if ship.category_id != 6 || !ship.singleton {
        return Err(insure_failed("That is not an assembled ship."));
    }
    if ship.owner_id != owner
        || !at_station(&tx, ship.location_id, ship.flag, owner, station, is_corp).await?
    {
        return Err(insure_failed(
            "The ship must be yours and in this station's hangar or corporation office.",
        ));
    }
    let base_price: f64 = tx
        .tx()
        .query_one(
            r#"SELECT COALESCE("basePrice",0.0) FROM "invTypes" WHERE "typeID"=$1"#,
            &[&(ship.type_id as i32)],
        )
        .await
        .map_err(db_failed)?
        .get(0);
    let (premium, fraction) = quote(amount, base_price)
        .ok_or_else(|| insure_failed("That is not a premium on offer."))?;
    let now = crate::bulkdata::filetime_now();
    if let Some(old) = tx
        .tx()
        .query_opt(
            r#"SELECT "ownerID","endDate" FROM "insuranceContracts" WHERE "shipID"=$1 FOR UPDATE"#,
            &[&(ship_id as i32)],
        )
        .await
        .map_err(db_failed)?
    {
        let previous = old.get::<_, i32>(0) as i64;
        if old.get::<_, i64>(1) > now {
            if previous == owner {
                return Err(Outcome::UserError {
                    msg: "InsureShipFailedMultipleContracts".into(),
                    args: vec![],
                });
            }
            if !void_old {
                return Err(Outcome::UserError {
                    msg: "InsureShipFailedSingleContract".into(),
                    args: vec![(
                        "ownerName".into(),
                        Value::Tuple(vec![Value::Int(2), Value::Int(previous)]),
                    )],
                });
            }
        }
        tx.tx()
            .execute(
                r#"DELETE FROM "insuranceContracts" WHERE "shipID"=$1"#,
                &[&(ship_id as i32)],
            )
            .await
            .map_err(db_failed)?;
    }
    let arg = ship_id.to_string();
    let entry = Entry {
        payer: owner,
        payee: 0,
        ref_type: REF_INSURANCE,
        arg_id1: &arg,
        reason: "",
    };
    wallet::post(tx.tx(), owner, -premium, &entry)
        .await
        .map_err(|e| match e {
            Transfer::NotEnough { need, have } => Outcome::not_enough_money(need, have),
            Transfer::NoSuchOwner => insure_failed("There is no wallet to pay from."),
            Transfer::Db(e) => db_failed(e),
        })?;
    let end = now + CONTRACT_WEEKS * 7 * 24 * 3_600 * 10_000_000;
    let sql = r#"INSERT INTO "insuranceContracts"
                    ("contractID","shipID","ownerID","startDate","endDate","fraction","amount","payout","isCorp")
                 VALUES (nextval('dyn_id_seq'), $1, $2, $3, $4, $5, $6, $7, $8)"#;
    let payout = fraction * base_price;
    tx.tx()
        .execute(
            sql,
            &[
                &(ship_id as i32),
                &(owner as i32),
                &now,
                &end,
                &fraction,
                &premium,
                &payout,
                &i16::from(is_corp),
            ],
        )
        .await
        .map_err(db_failed)?;
    tx.commit().await.map_err(db_failed)?;
    tracing::info!(
        "ship {ship_id} insured for {payout} ISK ({:.0}%) for {premium} ISK",
        fraction * 100.0
    );
    Ok(owner)
}

fn may_manage_corp(ctx: &Ctx<'_>) -> bool {
    ctx.session.corp_id.is_some()
        && ctx.session.corp_role() & (CORP_ROLE_ACCOUNTANT | CORP_ROLE_DIRECTOR) != 0
}

async fn at_station(
    tx: &ItemTxn<'_>,
    location: i64,
    flag: i64,
    owner: i64,
    station: i64,
    corp: bool,
) -> Result<bool, Outcome> {
    if location == station {
        return Ok(if corp { flag == 62 } else { flag == 4 });
    }
    if !corp || super::corphangar::division(flag).is_none() {
        return Ok(false);
    }
    Ok(tx.tx().query_opt(r#"SELECT 1 FROM "crpOffices" WHERE "itemID"=$1 AND "corporationID"=$2 AND "stationID"=$3"#,&[&(location as i32),&(owner as i32),&(station as i32)]).await.map_err(db_failed)?.is_some())
}

async fn uninsure(ctx: &Ctx<'_>, ship_id: i64) -> Result<(), Outcome> {
    let (Some(me), Some(station)) = (ctx.session.char_id, ctx.session.station_id) else {
        return Err(insure_failed(
            "Insurance can only be cancelled in a station.",
        ));
    };
    if ship_id <= 0 || ship_id > i32::MAX as i64 {
        return Err(insure_failed("That is not a ship."));
    }
    let mut client = crate::db::connect(ctx.db_url).await.map_err(db_failed)?;
    let mut tx = ItemTxn::begin(&mut client).await.map_err(db_failed)?;
    let ship = tx
        .item(ship_id)
        .await
        .map_err(db_failed)?
        .ok_or_else(|| insure_failed("That ship no longer exists."))?;
    let Some(policy) = tx
        .tx()
        .query_opt(
            r#"SELECT "ownerID","isCorp" FROM "insuranceContracts" WHERE "shipID"=$1 FOR UPDATE"#,
            &[&(ship_id as i32)],
        )
        .await
        .map_err(db_failed)?
    else {
        return Ok(());
    };
    let owner = policy.get::<_, i32>(0) as i64;
    let corp = policy.get::<_, i16>(1) != 0;
    let permitted = if corp {
        ctx.session.corp_id == Some(owner) && may_manage_corp(ctx)
    } else {
        owner == me
    };
    if !permitted
        || ship.owner_id != owner
        || !at_station(&tx, ship.location_id, ship.flag, owner, station, corp).await?
    {
        return Err(insure_failed("You cannot cancel insurance for that ship."));
    }
    tx.tx()
        .execute(
            r#"DELETE FROM "insuranceContracts" WHERE "shipID"=$1"#,
            &[&(ship_id as i32)],
        )
        .await
        .map_err(db_failed)?;
    tx.commit().await.map_err(db_failed)?;
    Ok(())
}

fn db_failed(error: tokio_postgres::Error) -> Outcome {
    tracing::error!("insurance: {}", crate::logging::chain(&error));
    insure_failed("The insurance could not be updated.")
}

fn insure_failed(reason: &str) -> Outcome {
    Outcome::UserError {
        msg: "InsureShipFailed".into(),
        args: vec![("reason".into(), Value::text(reason))],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn premiums_buy_cover_in_tenths() {
        assert_eq!(quote(500.0, 10_000.0), Some((500.0, 0.5)));
        assert_eq!(quote(3_000.0, 10_000.0), Some((3000.0, 1.0)));
        for invalid in [100.0, 250.0, 499.99, 501.0, 2750.0, f64::NAN, f64::INFINITY] {
            assert_eq!(quote(invalid, 10_000.0), None);
        }
        assert_eq!(quote(500.0, 0.0), None);
        let price = 123_456.789;
        let mut premium = 0.0;
        for level in 1..=6 {
            premium += price * 0.05;
            assert_eq!(
                quote(premium, price),
                Some((price * 0.05 * level as f64, (4 + level) as f64 / 10.0))
            );
        }
    }

    async fn pilot(db: &crate::test_support::Database) -> crate::session::Session {
        let id = db.id().await;
        db.db.execute(r#"INSERT INTO "character_" ("characterID","characterName",balance) VALUES ($1,$2,100000000)"#,&[&id,&format!("Insurance fixture {id}")]).await.unwrap();
        crate::session::Session {
            char_id: Some(id as i64),
            station_id: Some(60000004),
            ..Default::default()
        }
    }
    async fn ship(db: &crate::test_support::Database, owner: i64, location: i64) -> i64 {
        db.db.query_one(r#"INSERT INTO entity ("typeID","ownerID","locationID",flag,quantity,singleton) VALUES (587,$1,$2,4,1,1) RETURNING "itemID""#,&[&(owner as i32),&(location as i32)]).await.unwrap().get::<_,i32>(0) as i64
    }
    async fn balance(db: &crate::test_support::Database, owner: i64, corp: bool) -> f64 {
        let sql = if corp {
            r#"SELECT balance FROM corporation WHERE "corporationID"=$1"#
        } else {
            r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#
        };
        db.db
            .query_one(sql, &[&(owner as i32)])
            .await
            .unwrap()
            .get(0)
    }
    async fn premium(db: &crate::test_support::Database) -> f64 {
        db.db
            .query_one(
                r#"SELECT "basePrice"*0.05 FROM "invTypes" WHERE "typeID"=587"#,
                &[],
            )
            .await
            .unwrap()
            .get(0)
    }
    fn call(method: &str, args: Vec<Value>) -> Call {
        Call {
            service: "insuranceSvc".into(),
            method: method.into(),
            args,
            kwargs: vec![],
        }
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn insurance_purchase_is_atomic_concurrent_and_renews_expired_coverage() {
        let db = crate::test_support::Database::load().await;
        let session = pilot(&db).await;
        let me = session.char_id.unwrap();
        let ctx = db.ctx(&session);
        let hull = ship(&db, me, 60000004).await;
        let premium = premium(&db).await;
        let (one, two) = tokio::join!(
            insure(&ctx, hull, premium, false, false),
            insure(&ctx, hull, premium, false, false)
        );
        assert_eq!(
            [one, two]
                .iter()
                .filter(|r| matches!(r, Outcome::Value(_)))
                .count(),
            1
        );
        assert_eq!(balance(&db, me, false).await, 100_000_000.0 - premium);
        let policies: i64 = db
            .db
            .query_one(
                r#"SELECT count(*) FROM "insuranceContracts" WHERE "shipID"=$1"#,
                &[&(hull as i32)],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(policies, 1);
        db.db
            .execute(
                r#"UPDATE "insuranceContracts" SET "endDate"=0 WHERE "shipID"=$1"#,
                &[&(hull as i32)],
            )
            .await
            .unwrap();
        assert!(matches!(
            insurance_svc(&call("GetContractForShip", vec![Value::Int(hull)]), &ctx).await,
            Some(Outcome::Value(Value::None))
        ));
        assert!(matches!(
            insure(&ctx, hull, premium, false, false).await,
            Outcome::Value(_)
        ));
        let before = balance(&db, me, false).await;
        assert_eq!(before, 100_000_000.0 - 2.0 * premium);

        // Force the final insert to fail after the debit and journal write.
        let failing = ship(&db, me, 60000004).await;
        let constraint = format!("insurance_failure_fixture_{failing}");
        db.db.batch_execute(&format!(r#"ALTER TABLE "insuranceContracts" ADD CONSTRAINT {constraint} CHECK ("shipID" <> {failing})"#)).await.unwrap();
        let result = insure(&ctx, failing, premium, false, false).await;
        db.db
            .batch_execute(&format!(
                r#"ALTER TABLE "insuranceContracts" DROP CONSTRAINT {constraint}"#
            ))
            .await
            .unwrap();
        assert!(!matches!(result, Outcome::Value(_)));
        assert_eq!(balance(&db, me, false).await, before);
        let journals: i64 = db
            .db
            .query_one(
                r#"SELECT count(*) FROM market_journal WHERE "characterID"=$1 AND "argID1"=$2"#,
                &[&(me as i32), &failing.to_string()],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(journals, 0);
        let other = ship(&db, me, 60000004).await;
        let (one, two) = tokio::join!(
            insure(&ctx, failing, premium, false, false),
            insure(&ctx, other, premium, false, false)
        );
        assert!(matches!(one, Outcome::Value(_)) && matches!(two, Outcome::Value(_)));
        let ids:i64=db.db.query_one(r#"SELECT count(DISTINCT "contractID") FROM "insuranceContracts" WHERE "shipID"=ANY($1)"#,&[&vec![failing as i32,other as i32]]).await.unwrap().get(0);
        assert_eq!(ids, 2);
        assert_eq!(balance(&db, me, false).await, before - 2.0 * premium);
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn insurance_corporation_office_permissions_replacement_and_cancellation() {
        let db = crate::test_support::Database::load().await;
        let mut session = pilot(&db).await;
        let me = session.char_id.unwrap();
        let premium = premium(&db).await;
        let hull = ship(&db, me, 60000004).await;
        assert!(matches!(
            insure(&db.ctx(&session), hull, premium, false, false).await,
            Outcome::Value(_)
        ));
        let corp = db.id().await;
        let office = db.id().await;
        db.db.execute(r#"INSERT INTO corporation ("corporationID","corporationName",balance) VALUES ($1,$2,100000000)"#,&[&corp,&format!("Insurance corporation {corp}")]).await.unwrap();
        db.db.execute(r#"INSERT INTO "crpOffices" ("corporationID","stationID","itemID","officeFolderID") VALUES ($1,60000004,$2,$2)"#,&[&corp,&office]).await.unwrap();
        db.db
            .execute(
                r#"UPDATE entity SET "ownerID"=$2,"locationID"=$3 WHERE "itemID"=$1"#,
                &[&(hull as i32), &corp, &office],
            )
            .await
            .unwrap();
        session.corp_id = Some(corp as i64);
        assert!(!matches!(
            insure(&db.ctx(&session), hull, premium, true, true).await,
            Outcome::Value(_)
        ));
        session.roles_at_all = CORP_ROLE_JUNIOR_ACCOUNTANT;
        assert!(!matches!(
            insure(&db.ctx(&session), hull, premium, true, true).await,
            Outcome::Value(_)
        ));
        session.roles_at_all = CORP_ROLE_ACCOUNTANT;
        assert!(
            matches!(insure(&db.ctx(&session),hull,premium,true,false).await,Outcome::UserError {msg,..} if msg=="InsureShipFailedSingleContract")
        );
        db.db
            .execute(
                r#"UPDATE corporation SET balance=0 WHERE "corporationID"=$1"#,
                &[&corp],
            )
            .await
            .unwrap();
        assert!(!matches!(
            insure(&db.ctx(&session), hull, premium, true, true).await,
            Outcome::Value(_)
        ));
        let owner: i32 = db
            .db
            .query_one(
                r#"SELECT "ownerID" FROM "insuranceContracts" WHERE "shipID"=$1"#,
                &[&(hull as i32)],
            )
            .await
            .unwrap()
            .get(0);
        assert_eq!(owner, me as i32);
        db.db
            .execute(
                r#"UPDATE corporation SET balance=100000000 WHERE "corporationID"=$1"#,
                &[&corp],
            )
            .await
            .unwrap();
        let mut request = call(
            "InsureShip",
            vec![Value::Int(hull), Value::Double(premium), Value::Bool(true)],
        );
        request
            .kwargs
            .push((Value::bytes("voidOld"), Value::Bool(true)));
        assert!(matches!(
            insurance_svc(&request, &db.ctx(&session)).await,
            Some(Outcome::Value(_))
        ));
        assert_eq!(
            balance(&db, corp as i64, true).await,
            100_000_000.0 - premium
        );
        session.roles_at_all = CORP_ROLE_JUNIOR_ACCOUNTANT;
        assert!(uninsure(&db.ctx(&session), hull).await.is_err());
        session.roles_at_all = CORP_ROLE_ACCOUNTANT;
        assert!(uninsure(&db.ctx(&session), hull).await.is_ok());
        assert!(db
            .db
            .query_opt(
                r#"SELECT 1 FROM "insuranceContracts" WHERE "shipID"=$1"#,
                &[&(hull as i32)]
            )
            .await
            .unwrap()
            .is_none());
        assert_eq!(
            balance(&db, corp as i64, true).await,
            100_000_000.0 - premium
        );
    }
}
