//! `standing2`: standings between characters, corporations and NPCs (standingsvc.py).

use eve_marshal::Value;
use tracing::info;

use crate::services::{Call, Ctx, Outcome};
use crate::session::CORP_ROLE_DIRECTOR;
use crate::store::{self, Db, StoreError};
use crate::svc::rows::rowset;

const OWNER_HEADER: &[&str] = &["ownerID", "ownerName", "typeID"];

pub async fn standing(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "GetMyKillRights" => my_kill_rights(ctx),
        "GetNPCNPCStandings" => {
            const HEADER: &[&str] = &["fromID", "toID", "standing"];
            rowset(HEADER, crate::sde::table("npcStandings").rows().map(|r| Value::List(r.line(HEADER))).collect())
        }
        "GetMyStandings" => my_standings(ctx),
        "GetStandingTransactions" => standing_transactions(),
        "GetSecurityRating" => {
            let rating = ctx.store.read(|db| db.table("character_").get_id(call.arg_id(0)).and_then(|r| r.float("securityRating")));
            Value::Double(rating.unwrap_or(0.0))
        }
        "GetNPCStandingsTo" => {
            const HEADER: &[&str] = &["fromID", "standing"];
            ctx.store.read(|db| rowset(HEADER, lines(db.table("chrNPCStandings").find(&["characterID"], &[store::Value::Int(call.arg_id(0))]), HEADER)))
        }
        "GetStandingCompositions" => {
            ctx.store.read(|db| standing_compositions(db, call.arg_id(0), call.arg_id(1)))
        }
        // (toID, standing, reason); a standing of 0 clears it, as the client's cache does.
        "SetPlayerStanding" => {
            let Some(me) = ctx.session.char_id else { return Some(Outcome::Value(Value::None)) };
            set_standing(ctx, Standing::Character(me), call).await
        }
        "SetCorpStanding" => {
            let Some(corp) = ctx.session.corp_id else { return Some(Outcome::Value(Value::None)) };
            if !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
                return Some(Outcome::UserError {
                    msg: "CrpAccessDenied".into(),
                    args: vec![("reason".into(), Value::bytes("you are not a director"))],
                });
            }
            set_standing(ctx, Standing::Corporation(corp), call).await
        }
        _ => return None,
    };
    Some(Outcome::Value(value))
}

enum Standing {
    Character(i64),
    Corporation(i64),
}

async fn set_standing(ctx: &Ctx<'_>, from: Standing, call: &Call) -> Value {
    let to_id = call.arg_id(0);
    let standing = call.args.get(1).and_then(|v| v.as_f64()).unwrap_or(0.0).clamp(-10.0, 10.0);
    let reason = call.arg_text(2).unwrap_or_default();
    let (from_id, table, from_col, to_col) = match from {
        Standing::Character(id) => (id, "chrStandings", "characterID", "toID"),
        Standing::Corporation(id) => (id, "ownerStandings", "fromID", "toID"),
    };
    let key = [store::Value::Int(from_id), store::Value::Int(to_id)];
    let saved = ctx.store.write(|tx| -> Result<(), StoreError> {
        if standing == 0.0 {
            tx.delete_where(table, &[from_col, to_col], &key)?;
        } else if let Standing::Corporation(_) = from {
            let now = store::Value::Int(crate::bulkdata::filetime_now());
            let reason = store::Value::Text(reason.as_str().into());
            let update = [("standing", store::Value::Float(standing)), ("reason", reason), ("setAt", now)];
            let insert = [(from_col, key[0].clone()), (to_col, key[1].clone()), update[0].clone(), update[1].clone(), update[2].clone()];
            tx.upsert(table, &[from_col, to_col], &insert, &update)?;
        } else {
            let update = [("standing", store::Value::Float(standing))];
            let insert = [(from_col, key[0].clone()), (to_col, key[1].clone()), update[0].clone()];
            tx.upsert(table, &[from_col, to_col], &insert, &update)?;
        }
        Ok(())
    });
    if let Err(e) = saved {
        tracing::error!("statement failed: {e}");
    }
    info!("standing {from_id} -> {to_id} set to {standing}");
    Value::None
}

/// The rows' `columns` as rowset lines.
fn lines<'a>(rows: impl Iterator<Item = store::Row<'a>>, columns: &[&str]) -> Vec<Value> {
    rows.map(|r| Value::List(r.line(columns))).collect()
}

/// What fromID's standing towards toID is made of. Standings here are set
/// directly, never derived, so the only component is the standing itself
/// (the window labels the fromID entry "Current Value").
fn standing_compositions(db: &Db, from_id: i64, to_id: i64) -> Value {
    let (from, to) = (store::Value::Int(from_id), store::Value::Int(to_id));
    let found = [
        db.table("chrNPCStandings").find(&["fromID", "characterID"], &[from.clone(), to.clone()]),
        db.table("chrStandings").find(&["characterID", "toID"], &[from.clone(), to.clone()]),
        db.table("ownerStandings").find(&["fromID", "toID"], &[from, to]),
    ];
    let lines = found.into_iter().flatten().map(|r| Value::List(vec![Value::Int(from_id), r.cell("standing")])).collect();
    rowset(&["ownerID", "standing"], lines)
}

fn standing_transactions() -> Value {
    const HEADER: &[&str] = &[
        "eventID", "eventTypeID", "eventDateTime", "fromID", "toID", "originalFromID",
        "originalToID", "modification", "int_1", "int_2", "int_3", "msg",
    ];
    rowset(HEADER, Vec::new())
}

/// standingsvc.__Init unpacks (charstandings, charprime, npccharstandings,
/// corpstandings, corpprime, npccorpstandings). The primes go to
/// cfg.eveowners.Hint(None, prime), which needs owner rows, not bare IDs.
fn my_standings(ctx: &Ctx<'_>) -> Value {
    const TO_HEADER: &[&str] = &["toID", "standing"];
    const FROM_HEADER: &[&str] = &["fromID", "standing"];
    let char_id = store::Value::Int(ctx.session.char_id.unwrap_or(0));
    let corp_id = store::Value::Int(ctx.session.corp_id.unwrap_or(0));

    let (char_standings, char_prime, npc_standings, corp_standings, corp_prime) = ctx.store.read(|db| {
        let mine = || db.table("chrStandings").find(&["characterID"], std::slice::from_ref(&char_id));
        let corp = || db.table("ownerStandings").find(&["fromID"], std::slice::from_ref(&corp_id));
        (
            rowset(TO_HEADER, lines(mine(), TO_HEADER)),
            owners_of(db, mine()),
            rowset(FROM_HEADER, lines(db.table("chrNPCStandings").find(&["characterID"], std::slice::from_ref(&char_id)), FROM_HEADER)),
            rowset(TO_HEADER, lines(corp(), TO_HEADER)),
            owners_of(db, corp()),
        )
    });

    Value::Tuple(vec![
        char_standings,
        char_prime,
        npc_standings,
        corp_standings,
        corp_prime,
        Value::Dict(Vec::new()),
    ])
}

/// Owner rows for the standings' `toID`s, for cfg.eveowners.Hint.
fn owners_of<'a>(db: &'a Db, standings: impl Iterator<Item = store::Row<'a>>) -> Value {
    let owners = db.table("eveStaticOwners");
    let mut seen = std::collections::HashSet::new();
    let lines = standings
        .filter_map(|r| r.int("toID"))
        .filter(|id| seen.insert(*id))
        .filter_map(|id| owners.get_id(id))
        .map(|r| Value::List(r.line(OWNER_HEADER)))
        .collect();
    rowset(OWNER_HEADER, lines)
}

/// (rights I hold over others, rights others hold over me): {charID: expiry}.
fn my_kill_rights(ctx: &Ctx<'_>) -> Value {
    let char_id = store::Value::Int(ctx.session.char_id.unwrap_or(0));
    ctx.store.read(|db| {
        let rights = db.table("chrKillRights");
        let dict = |by: &str, other: &str| {
            Value::Dict(rights.find(&[by], std::slice::from_ref(&char_id)).map(|r| (r.cell(other), r.cell("whenTimestamp"))).collect())
        };
        Value::Tuple(vec![dict("fromID", "toID"), dict("toID", "fromID")])
    })
}
