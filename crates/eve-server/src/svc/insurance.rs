//! `insuranceSvc`, unbound and bound per station (base_insurance.py). Quotes are
//! computed by the client; the server keeps the contracts.

use eve_marshal::Value;

use crate::items::ItemTxn;
use crate::services::{Call, Ctx, Outcome};
use crate::session::{CORP_ROLE_ACCOUNTANT, CORP_ROLE_DIRECTOR};
use crate::store::{Db, StoreError, Value as V};
use crate::svc::rows::{row, rowset};
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
            let now = crate::bulkdata::filetime_now();
            let lines = ctx.store.read(|db| {
                let mut current: Vec<_> = db
                    .table("insuranceContracts")
                    .find(&["ownerID"], &[V::Int(owner)])
                    .filter(|r| r.int("endDate").is_some_and(|end| end > now))
                    .collect();
                current.sort_by_key(|r| r.int("contractID"));
                current.iter().map(|r| Value::List(r.line(HEADER))).collect()
            });
            rowset(HEADER, lines)
        }
        // infosvc reads contract.ownerID/.fraction/.startDate/.endDate, or None.
        "GetContractForShip" => {
            let now = crate::bulkdata::filetime_now();
            let line = ctx.store.read(|db| {
                db.table("insuranceContracts")
                    .find(&["shipID"], &[V::Int(call.arg_id(0))])
                    .find(|r| r.int("endDate").is_some_and(|end| end > now))
                    .map(|r| r.line(HEADER))
            });
            match line {
                Some(line) => row(HEADER, line),
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

/// Why a purchase or a cancellation stopped, as the client is told it; a
/// store error is logged and told as the insurance not being updated.
struct Refusal(Outcome);

impl From<StoreError> for Refusal {
    fn from(error: StoreError) -> Self {
        Refusal(db_failed(error))
    }
}

fn refused(reason: &str) -> Refusal {
    Refusal(insure_failed(reason))
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
    let (payout, fraction, premium) = ctx
        .store
        .write(|tx| {
            let mut tx = ItemTxn::new(tx);
            let ship = tx
                .item(ship_id)?
                .ok_or_else(|| refused("That ship no longer exists."))?;
            if ship.category_id != 6 || !ship.singleton {
                return Err(refused("That is not an assembled ship."));
            }
            if ship.owner_id != owner
                || !at_station(tx.db(), ship.location_id, ship.flag, owner, station, is_corp)
            {
                return Err(refused(
                    "The ship must be yours and in this station's hangar or corporation office.",
                ));
            }
            // The ship is of category 6, so its type is known.
            let base_price = crate::sde::table("invTypes")
                .get("typeID", ship.type_id)
                .and_then(|t| t.float("basePrice"))
                .unwrap_or(0.0);
            let (premium, fraction) = quote(amount, base_price)
                .ok_or_else(|| refused("That is not a premium on offer."))?;
            let now = crate::bulkdata::filetime_now();
            let old = tx
                .db()
                .table("insuranceContracts")
                .find_one(&["shipID"], &[V::Int(ship_id)])
                .map(|r| (r.id, r.int("ownerID").unwrap_or(0), r.int("endDate").unwrap_or(0)));
            if let Some((old, previous, end)) = old {
                if end > now {
                    if previous == owner {
                        return Err(Refusal(Outcome::UserError {
                            msg: "InsureShipFailedMultipleContracts".into(),
                            args: vec![],
                        }));
                    }
                    if !void_old {
                        return Err(Refusal(Outcome::UserError {
                            msg: "InsureShipFailedSingleContract".into(),
                            args: vec![(
                                "ownerName".into(),
                                Value::Tuple(vec![Value::Int(2), Value::Int(previous)]),
                            )],
                        }));
                    }
                }
                tx.tx().delete("insuranceContracts", old)?;
            }
            let arg = ship_id.to_string();
            let entry = Entry {
                payer: owner,
                payee: 0,
                ref_type: REF_INSURANCE,
                arg_id1: &arg,
                reason: "",
            };
            wallet::post(tx.tx(), owner, -premium, &entry).map_err(|e| {
                Refusal(match e {
                    Transfer::NotEnough { need, have } => Outcome::not_enough_money(need, have),
                    Transfer::NoSuchOwner => insure_failed("There is no wallet to pay from."),
                    Transfer::Db(e) => db_failed(e),
                })
            })?;
            let end = now + CONTRACT_WEEKS * 7 * 24 * 3_600 * 10_000_000;
            let payout = fraction * base_price;
            let contract = tx.tx().nextval("dyn_id_seq")?;
            tx.tx().insert(
                "insuranceContracts",
                &[
                    ("contractID", V::Int(contract)),
                    ("shipID", V::Int(ship_id)),
                    ("ownerID", V::Int(owner)),
                    ("startDate", V::Int(now)),
                    ("endDate", V::Int(end)),
                    ("fraction", V::Float(fraction)),
                    ("amount", V::Float(premium)),
                    ("payout", V::Float(payout)),
                    ("isCorp", V::Int(i64::from(is_corp))),
                ],
            )?;
            Ok((payout, fraction, premium))
        })
        .map_err(|r: Refusal| r.0)?;
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

fn at_station(
    db: &Db,
    location: i64,
    flag: i64,
    owner: i64,
    station: i64,
    corp: bool,
) -> bool {
    if location == station {
        return if corp { flag == 62 } else { flag == 4 };
    }
    if !corp || super::corphangar::division(flag).is_none() {
        return false;
    }
    db.table("crpOffices")
        .find(&["itemID"], &[V::Int(location)])
        .any(|r| r.int("corporationID") == Some(owner) && r.int("stationID") == Some(station))
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
    ctx.store
        .write(|tx| {
            let mut tx = ItemTxn::new(tx);
            let ship = tx
                .item(ship_id)?
                .ok_or_else(|| refused("That ship no longer exists."))?;
            let Some((policy, owner, corp)) = tx
                .db()
                .table("insuranceContracts")
                .find_one(&["shipID"], &[V::Int(ship_id)])
                .map(|r| (r.id, r.int("ownerID").unwrap_or(0), r.int("isCorp").unwrap_or(0) != 0))
            else {
                return Ok(());
            };
            let permitted = if corp {
                ctx.session.corp_id == Some(owner) && may_manage_corp(ctx)
            } else {
                owner == me
            };
            if !permitted
                || ship.owner_id != owner
                || !at_station(tx.db(), ship.location_id, ship.flag, owner, station, corp)
            {
                return Err(refused("You cannot cancel insurance for that ship."));
            }
            tx.tx().delete("insuranceContracts", policy)?;
            Ok(())
        })
        .map_err(|r: Refusal| r.0)
}

fn db_failed(error: StoreError) -> Outcome {
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
        let id = db.id().await as i64;
        db.store
            .write(|tx| {
                tx.insert(
                    "character_",
                    &[
                        ("characterID", V::Int(id)),
                        ("characterName", V::Text(format!("Insurance fixture {id}").into())),
                        ("balance", V::Float(100_000_000.0)),
                    ],
                )
            })
            .unwrap();
        crate::session::Session {
            char_id: Some(id),
            station_id: Some(60000004),
            ..Default::default()
        }
    }
    async fn ship(db: &crate::test_support::Database, owner: i64, location: i64) -> i64 {
        db.store
            .write(|tx| {
                let row = tx.insert(
                    "entity",
                    &[
                        ("typeID", V::Int(587)),
                        ("ownerID", V::Int(owner)),
                        ("locationID", V::Int(location)),
                        ("flag", V::Int(4)),
                        ("quantity", V::Int(1)),
                        ("singleton", V::Int(1)),
                    ],
                )?;
                Ok::<_, StoreError>(tx.table("entity").row(row).and_then(|r| r.int("itemID")).unwrap())
            })
            .unwrap()
    }
    async fn balance(db: &crate::test_support::Database, owner: i64, corp: bool) -> f64 {
        let table = if corp { "corporation" } else { "character_" };
        db.store.read(|db| db.table(table).get_id(owner).and_then(|r| r.float("balance")).unwrap())
    }
    fn contracts(db: &crate::test_support::Database, ship: i64) -> Vec<(i64, i64, i64)> {
        db.store.read(|db| {
            db.table("insuranceContracts")
                .find(&["shipID"], &[V::Int(ship)])
                .map(|r| (r.int("contractID").unwrap(), r.int("ownerID").unwrap(), r.int("endDate").unwrap()))
                .collect()
        })
    }
    fn premium() -> f64 {
        crate::sde::table("invTypes").get("typeID", 587).unwrap().float("basePrice").unwrap() * 0.05
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
    async fn insurance_purchase_is_atomic_concurrent_and_renews_expired_coverage() {
        let db = crate::test_support::Database::load().await;
        let session = pilot(&db).await;
        let me = session.char_id.unwrap();
        let ctx = db.ctx(&session);
        let hull = ship(&db, me, 60000004).await;
        let premium = premium();
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
        assert_eq!(contracts(&db, hull).len(), 1);
        db.store
            .write(|tx| {
                let row = tx.table("insuranceContracts").find_one(&["shipID"], &[V::Int(hull)]).unwrap().id;
                tx.update("insuranceContracts", row, &[("endDate", V::Int(0))])
            })
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

        // Make the final insert fail after the debit and journal write: a
        // contract already holds the ID it will draw.
        let failing = ship(&db, me, 60000004).await;
        let elsewhere = db.id().await as i64;
        let blocker = db
            .store
            .write(|tx| {
                let next = tx.db().sequence("dyn_id_seq").unwrap() + 1;
                tx.insert(
                    "insuranceContracts",
                    &[
                        ("contractID", V::Int(next)),
                        ("shipID", V::Int(elsewhere)),
                        ("ownerID", V::Int(me)),
                        ("startDate", V::Int(0)),
                        ("endDate", V::Int(0)),
                        ("fraction", V::Float(0.5)),
                    ],
                )
            })
            .unwrap();
        let result = insure(&ctx, failing, premium, false, false).await;
        db.store.write(|tx| tx.delete("insuranceContracts", blocker)).unwrap();
        assert!(!matches!(result, Outcome::Value(_)));
        assert_eq!(balance(&db, me, false).await, before);
        let journals = db.store.read(|db| {
            db.table("market_journal")
                .rows()
                .filter(|r| r.int("characterID") == Some(me) && r.text("argID1") == Some(failing.to_string().as_str()))
                .count()
        });
        assert_eq!(journals, 0);
        let other = ship(&db, me, 60000004).await;
        let (one, two) = tokio::join!(
            insure(&ctx, failing, premium, false, false),
            insure(&ctx, other, premium, false, false)
        );
        assert!(matches!(one, Outcome::Value(_)) && matches!(two, Outcome::Value(_)));
        let mut ids: Vec<i64> = [failing, other].iter().flat_map(|s| contracts(&db, *s)).map(|c| c.0).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), 2);
        assert_eq!(balance(&db, me, false).await, before - 2.0 * premium);
    }

    #[tokio::test]
    async fn insurance_corporation_office_permissions_replacement_and_cancellation() {
        let db = crate::test_support::Database::load().await;
        let mut session = pilot(&db).await;
        let me = session.char_id.unwrap();
        let premium = premium();
        let hull = ship(&db, me, 60000004).await;
        assert!(matches!(
            insure(&db.ctx(&session), hull, premium, false, false).await,
            Outcome::Value(_)
        ));
        let corp = db.id().await as i64;
        let office = db.id().await as i64;
        let set_corp_balance = |balance: f64| {
            db.store
                .write(|tx| {
                    let row = tx.table("corporation").get_id(corp).unwrap().id;
                    tx.update("corporation", row, &[("balance", V::Float(balance))])
                })
                .unwrap()
        };
        db.store
            .write(|tx| {
                tx.insert(
                    "corporation",
                    &[
                        ("corporationID", V::Int(corp)),
                        ("corporationName", V::Text(format!("Insurance corporation {corp}").into())),
                        ("balance", V::Float(100_000_000.0)),
                    ],
                )?;
                tx.insert(
                    "crpOffices",
                    &[
                        ("corporationID", V::Int(corp)),
                        ("stationID", V::Int(60000004)),
                        ("itemID", V::Int(office)),
                        ("officeFolderID", V::Int(office)),
                    ],
                )?;
                let row = tx.table("entity").get_id(hull).unwrap().id;
                tx.update("entity", row, &[("ownerID", V::Int(corp)), ("locationID", V::Int(office))])
            })
            .unwrap();
        session.corp_id = Some(corp);
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
        set_corp_balance(0.0);
        assert!(!matches!(
            insure(&db.ctx(&session), hull, premium, true, true).await,
            Outcome::Value(_)
        ));
        assert_eq!(contracts(&db, hull)[0].1, me);
        set_corp_balance(100_000_000.0);
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
            balance(&db, corp, true).await,
            100_000_000.0 - premium
        );
        session.roles_at_all = CORP_ROLE_JUNIOR_ACCOUNTANT;
        assert!(uninsure(&db.ctx(&session), hull).await.is_err());
        session.roles_at_all = CORP_ROLE_ACCOUNTANT;
        assert!(uninsure(&db.ctx(&session), hull).await.is_ok());
        assert!(contracts(&db, hull).is_empty());
        assert_eq!(
            balance(&db, corp, true).await,
            100_000_000.0 - premium
        );
    }
}
