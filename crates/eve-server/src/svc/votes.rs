//! Corporation votes (corp_ui_votes.py) and the actions they sanction
//! (corp_ui_sanctionableactions.py).
//!
//! A director or the CEO puts a question to the shareholders, with options;
//! each shareholder votes once, weighted by the shares they hold. When the
//! vote closes ([`close_votes`], from the heartbeat) the option with the most
//! shares behind it wins (the first on a tie) and is recorded as a sanctioned
//! action. For a war, an expulsion, new shares or a blueprint lockdown the
//! CEO then has a week to put it into effect (UpdateSanctionedAction); a
//! general vote's decision is simply recorded.
//!
//! CEO elections are votes too: a member with Corporation Management runs
//! (ProposeCEOVote), the shareholders choose between the candidate and the
//! CEO there is, and a winning candidate takes over as the vote closes.
//! While one is open no other vote may be put. A CEO may also resign
//! (ResignFromCEO), handing over to a member of their choice, or closing
//! the corporation if nobody else is left.

use eve_marshal::Value;
use tracing::{error, info};

use crate::services::{Call, Ctx, Outcome};
use crate::session::CORP_ROLE_DIRECTOR;
use crate::store::{self, Db, Row, Store, StoreError, Tx};
use crate::svc::membership::{access_denied, db_failed, message, Refused};
use crate::svc::rows::{change, index_rowset};
use crate::world::World;

/// eveConstants vote*.
const VOTE_CEO: i64 = 0;
const VOTE_WAR: i64 = 1;
const VOTE_SHARES: i64 = 2;
const VOTE_KICK_MEMBER: i64 = 3;
const VOTE_ITEM_LOCKDOWN: i64 = 5;
const VOTE_ITEM_UNLOCK: i64 = 6;
/// How long the CEO has to act on a sanctioned action.
const ACT_WITHIN: i64 = 7 * 24 * 3_600 * 10_000_000;

const CASE_HEADER: &[&str] =
    &["voteCaseID", "corporationID", "voteType", "voteCaseText", "description", "startDateTime", "endDateTime"];
const OPTION_HEADER: &[&str] = &["voteCaseID", "optionID", "optionText", "parameter", "parameter1", "parameter2", "votesFor"];
const VOTE_HEADER: &[&str] = &["voteCaseID", "characterID", "optionID"];
const SANCTION_HEADER: &[&str] = &[
    "voteCaseID", "corporationID", "voteType", "optionID", "parameter", "parameter1", "parameter2",
    "expires", "actedUpon", "inEffect", "timeActedUpon", "timeRescended", "reason",
];

/// A refusal from inside a store write, which undoes the write.
struct Failed(Outcome);

impl From<Outcome> for Failed {
    fn from(o: Outcome) -> Self {
        Failed(o)
    }
}

impl From<StoreError> for Failed {
    fn from(e: StoreError) -> Self {
        Failed(db_failed("writing", e))
    }
}

fn int(v: i64) -> store::Value {
    store::Value::Int(v)
}

/// A sanctioned action as the client is sent it: with its vote's type and
/// the chosen option's parameters (None where the option is gone). None if
/// its vote is gone.
fn sanction_line(db: &Db, s: &Row) -> Option<Vec<Value>> {
    let case = s.value("voteCaseID").clone();
    let c = db.table("crpVoteCases").get(std::slice::from_ref(&case))?;
    let o = s.int("optionID").and_then(|option| db.table("crpVoteOptions").get(&[case, int(option)]));
    let option = |col: &str| o.map_or(Value::None, |o| o.cell(col));
    Some(vec![
        s.cell("voteCaseID"), s.cell("corporationID"), c.cell("voteType"), s.cell("optionID"),
        option("parameter"), option("parameter1"), option("parameter2"),
        s.cell("expires"), s.cell("actedUpon"), s.cell("inEffect"), s.cell("timeActedUpon"), s.cell("timeRescended"), s.cell("reason"),
    ])
}

/// The shares behind one option of a vote: what each who chose it holds in
/// the vote's corporation.
fn votes_for(db: &Db, case: i64, corp: i64, option: i64) -> i64 {
    let shares = db.table("crpCharShares");
    db.table("crpVotes")
        .find(&["voteCaseID"], &[int(case)])
        .filter(|v| v.int("optionID") == Some(option))
        .filter_map(|v| shares.get(&[v.value("characterID").clone(), int(corp)]))
        .filter_map(|sh| sh.int("shares"))
        .sum()
}

pub async fn corp_registry(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let corp = call.arg_id(0);
    let reads = matches!(
        call.method.as_str(),
        "GetVoteCasesByCorporation" | "GetVoteCaseOptions" | "GetVotes" | "GetSanctionedActionsByCorporation"
    );
    if reads {
        if let Err(refusal) = viewer(ctx, corp).await {
            return Some(refusal);
        }
    }
    let result = match call.method.as_str() {
        "GetVoteCasesByCorporation" => Ok(cases(ctx, corp).await),
        "GetVoteCaseOptions" => Ok(options(ctx, call.arg_id(1)).await),
        "GetVotes" => Ok(votes(ctx, call.arg_id(1)).await),
        "GetSanctionedActionsByCorporation" => Ok(sanctions(ctx, corp).await),
        "CanViewVotes" => Ok(Value::Bool(viewer(ctx, corp).await.is_ok())),
        "InsertVoteCase" => propose(ctx, call).await,
        "InsertVote" => vote(ctx, corp, call.arg_id(1), call.arg_id(2)).await,
        "UpdateSanctionedAction" => {
            let in_effect = call.arg_i64(1).unwrap_or(0) != 0;
            act(ctx, call.arg_id(0), in_effect, &call.arg_text(2).unwrap_or_default()).await
        }
        "MemberCanRunForCEO" => Ok(Value::Int(can_run(ctx).await.is_ok().into())),
        "ResignFromCEO" => resign(ctx, call.arg_i64(0)).await,
        _ => return None,
    };
    Some(result.map_or_else(|refusal| refusal, Outcome::Value))
}

/// Who may see a corporation's votes: its directors, and its shareholders.
async fn viewer(ctx: &Ctx<'_>, corp: i64) -> Refused<()> {
    if ctx.session.corp_id == Some(corp) && ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
        return Ok(());
    }
    if ctx.store.read(|db| shares_of(db, corp, ctx.session.char_id.unwrap_or(0))) > 0 {
        return Ok(());
    }
    Err(access_denied("you must be the CEO, a director or a shareholder"))
}

fn shares_of(db: &Db, corp: i64, char_id: i64) -> i64 {
    db.table("crpCharShares").get(&[int(char_id), int(corp)]).and_then(|r| r.int("shares")).unwrap_or(0)
}

async fn cases(ctx: &Ctx<'_>, corp: i64) -> Value {
    let lines = ctx.store.read(|db| {
        let mut rows: Vec<Row> = db.table("crpVoteCases").find(&["corporationID"], &[int(corp)]).collect();
        rows.sort_by_key(|r| r.int("voteCaseID"));
        rows.iter().map(|r| r.line(CASE_HEADER)).collect()
    });
    keyed(CASE_HEADER, lines, 0)
}

/// The options, with the shares behind each so far.
async fn options(ctx: &Ctx<'_>, case: i64) -> Value {
    let lines = ctx.store.read(|db| {
        let corp = db.table("crpVoteCases").get_id(case).and_then(|c| c.int("corporationID"));
        let mut rows: Vec<Row> = db.table("crpVoteOptions").find(&["voteCaseID"], &[int(case)]).collect();
        rows.sort_by_key(|r| r.int("optionID"));
        rows.iter()
            .map(|r| {
                let mut l = r.line(&OPTION_HEADER[..6]);
                let shares = corp.zip(r.int("optionID")).map_or(0, |(corp, option)| votes_for(db, case, corp, option));
                l.push(Value::Int(shares));
                l
            })
            .collect()
    });
    keyed(OPTION_HEADER, lines, 1)
}

async fn votes(ctx: &Ctx<'_>, case: i64) -> Value {
    let lines = ctx.store.read(|db| {
        let mut rows: Vec<Row> = db.table("crpVotes").find(&["voteCaseID"], &[int(case)]).collect();
        rows.sort_by_key(|r| r.int("characterID"));
        rows.iter().map(|r| r.line(VOTE_HEADER)).collect()
    });
    keyed(VOTE_HEADER, lines, 1)
}

async fn sanctions(ctx: &Ctx<'_>, corp: i64) -> Value {
    let lines = ctx.store.read(|db| {
        let mut rows: Vec<Row> = db.table("crpSanctionedActions").find(&["corporationID"], &[int(corp)]).collect();
        rows.sort_by_key(|r| r.int("voteCaseID"));
        rows.iter().filter_map(|r| sanction_line(db, r)).collect()
    });
    keyed(SANCTION_HEADER, lines, 0)
}

fn keyed(header: &[&str], lines: Vec<Vec<Value>>, key: usize) -> Value {
    let items = lines.into_iter().map(|l| (l[key].clone(), Value::List(l))).collect();
    index_rowset(header, header[key], items)
}

/// InsertVoteCase(text, description, corporationID, voteType, options,
/// startDateTime, endDateTime); options a util.Rowset of [optionText,
/// parameter, parameter1, parameter2].
async fn propose(ctx: &Ctx<'_>, call: &Call) -> Refused<Value> {
    let corp = call.arg_id(2);
    let (text, description) = (call.arg_text(0).unwrap_or_default(), call.arg_text(1).unwrap_or_default());
    let vote_type = call.arg_i64(3).unwrap_or(-1);
    if vote_type == VOTE_CEO {
        can_run(ctx).await?;
    } else {
        if ctx.session.corp_id != Some(corp) || !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) {
            return Err(message("CrpOnlyDirectorsCanProposeVotes"));
        }
        if ctx.store.read(|db| ceo_vote_open(db, corp)) {
            return Err(message("CEOCantAddVoteInCEOVotePeriod"));
        }
    }
    let now = crate::bulkdata::filetime_now();
    let start = call.arg_i64(5).unwrap_or(now).max(now);
    let end = call.arg_i64(6).unwrap_or(start).max(start);
    let lines = rowset_lines(call.args.get(4));
    if lines.len() < 2 {
        return Err(Outcome::custom("A vote needs at least two options."));
    }
    // A candidate runs for themselves only.
    let me = ctx.session.char_id.unwrap_or(0);
    if vote_type == VOTE_CEO && !lines.iter().all(|l| matches!(l.get(1).and_then(|v| v.as_i64()), Some(0) | None) || l.get(1).and_then(|v| v.as_i64()) == Some(me)) {
        return Err(message("CantRunForCEOAtTheMoment"));
    }
    let case = ctx
        .store
        .write(|tx| -> Result<i64, Failed> {
            let case = tx.nextval("dyn_id_seq")?;
            tx.insert(
                "crpVoteCases",
                &[
                    ("voteCaseID", int(case)),
                    ("corporationID", int(corp)),
                    ("voteType", int(vote_type)),
                    ("voteCaseText", store::Value::Text(text.as_str().into())),
                    ("description", store::Value::Text(description.as_str().into())),
                    ("startDateTime", int(start)),
                    ("endDateTime", int(end)),
                ],
            )
            .map_err(|e| db_failed("recording the vote", e))?;
            for (i, l) in lines.iter().enumerate() {
                let param = |k: usize| l.get(k).and_then(|v| v.as_i64()).map_or(store::Value::Null, int);
                let option_text = l.first().and_then(|v| v.as_string()).unwrap_or_default();
                tx.insert(
                    "crpVoteOptions",
                    &[
                        ("voteCaseID", int(case)),
                        ("optionID", int(i as i64 + 1)),
                        ("optionText", store::Value::Text(option_text.into())),
                        ("parameter", param(1)),
                        ("parameter1", param(2)),
                        ("parameter2", param(3)),
                    ],
                )
                .map_err(|e| db_failed("recording an option", e))?;
            }
            Ok(case)
        })
        .map_err(|Failed(o)| o)?;
    let new = vec![
        Value::Int(case), Value::Int(corp), Value::Int(vote_type), Value::text(&text), Value::text(&description),
        Value::Long(start), Value::Long(end),
    ];
    info!("corporation {corp} put vote {case} ({text:?}) to its shareholders");
    tell(ctx.world, corp, "OnCorporationVoteCaseChanged", vec![Value::Int(corp), Value::Int(case), change(CASE_HEADER, None, Some(&new))]);
    Ok(Value::None)
}

/// The lines of a util.Rowset argument.
fn rowset_lines(v: Option<&Value>) -> Vec<Vec<Value>> {
    let Some(Value::Instance { state, .. }) = v else { return Vec::new() };
    let Value::Dict(d) = state.as_ref() else { return Vec::new() };
    d.iter()
        .find(|(k, _)| *k == Value::bytes("lines"))
        .and_then(|(_, v)| v.seq())
        .map(|lines| lines.iter().filter_map(|l| l.seq().map(|l| l.to_vec())).collect())
        .unwrap_or_default()
}

/// InsertVote(corporationID, voteCaseID, optionID): once per shareholder,
/// while the vote is open.
async fn vote(ctx: &Ctx<'_>, corp: i64, case: i64, option: i64) -> Refused<Value> {
    let char_id = ctx.session.char_id.unwrap_or(0);
    if ctx.store.read(|db| shares_of(db, corp, char_id)) <= 0 {
        return Err(access_denied("only shareholders may vote"));
    }
    let now = crate::bulkdata::filetime_now();
    let open = ctx.store.read(|db| {
        let case_open = db.table("crpVoteCases").get_id(case).is_some_and(|c| {
            c.int("corporationID") == Some(corp)
                && c.int("startDateTime").is_some_and(|s| s <= now)
                && c.int("endDateTime").is_some_and(|e| e > now)
        });
        case_open && db.table("crpVoteOptions").get(&[int(case), int(option)]).is_some()
    });
    if !open {
        return Err(Outcome::custom("That vote is not open."));
    }
    let added = ctx
        .store
        .write(|tx| {
            tx.insert_or_ignore("crpVotes", &[("voteCaseID", int(case)), ("characterID", int(char_id)), ("optionID", int(option))])
        })
        .map_err(|e| db_failed("recording the vote", e))?;
    if added.is_none() {
        return Err(message("CrpAlreadyVoted"));
    }
    let new = vec![Value::Int(case), Value::Int(char_id), Value::Int(option)];
    tell(ctx.world, corp, "OnCorporationVoteChanged", vec![Value::Int(corp), Value::Int(case), Value::Int(char_id), change(VOTE_HEADER, None, Some(&new))]);
    Ok(Value::None)
}

/// UpdateSanctionedAction(voteCaseID, inEffect, reason): the CEO puts a
/// sanctioned action into effect, or rescinds it.
async fn act(ctx: &Ctx<'_>, case: i64, in_effect: bool, reason: &str) -> Refused<Value> {
    let corp = ctx.session.corp_id.unwrap_or(0);
    let ceo = ctx.store.read(|db| ceo_of(db, corp));
    if ceo != ctx.session.char_id {
        return Err(access_denied("only the CEO may act on what the shareholders sanctioned"));
    }
    let old = ctx
        .store
        .read(|db| {
            db.table("crpSanctionedActions")
                .get_id(case)
                .filter(|s| s.int("corporationID") == Some(corp))
                .and_then(|s| sanction_line(db, &s))
        })
        .ok_or_else(|| Outcome::custom("There is no such sanctioned action."))?;
    let at = |i: usize| old[i].as_i64();
    let now = crate::bulkdata::filetime_now();
    if in_effect {
        if at(8) == Some(1) {
            return Ok(Value::None);
        }
        if at(7).is_some_and(|expires| expires < now) {
            return Err(Outcome::custom("That sanctioned action has expired."));
        }
        let (vote_type, parameter) = (at(2).unwrap_or(-1), at(4).unwrap_or(0));
        if parameter == 0 {
            return Err(Outcome::custom("The shareholders decided against it."));
        }
        carry_out(ctx, corp, vote_type, parameter, at(5), at(6)).await?;
    }
    let saved = ctx.store.write(|tx| {
        let Some(row) = tx.table("crpSanctionedActions").get_id(case).map(|s| s.id) else { return Ok(()) };
        let when = if in_effect { "timeActedUpon" } else { "timeRescended" };
        tx.update(
            "crpSanctionedActions",
            row,
            &[
                ("inEffect", int(in_effect.into())),
                ("actedUpon", int(1)),
                ("reason", store::Value::Text(reason.into())),
                (when, int(now)),
            ],
        )
    });
    saved.map_err(|e| db_failed("recording the action", e))?;
    let mut new = old.clone();
    new[8] = Value::Int(1);
    new[9] = Value::Int(in_effect.into());
    if in_effect { new[10] = Value::Long(now) } else { new[11] = Value::Long(now) }
    new[12] = Value::text(reason);
    info!("CEO of {corp} {} sanctioned action {case}", if in_effect { "carried out" } else { "rescinded" });
    tell(ctx.world, corp, "OnSanctionedActionChanged", vec![Value::Int(corp), Value::Int(case), change(SANCTION_HEADER, Some(&old), Some(&new))]);
    Ok(Value::None)
}

/// What a sanctioned action does.
async fn carry_out(ctx: &Ctx<'_>, corp: i64, vote_type: i64, parameter: i64, type_id: Option<i64>, location: Option<i64>) -> Refused<()> {
    match vote_type {
        VOTE_WAR => {
            if ctx.session.alliance_id.is_some() {
                return Err(Outcome::custom("Your alliance declares war for you."));
            }
            crate::svc::war::declare(ctx, corp, corp, parameter).await.map(|_| ())
        }
        VOTE_KICK_MEMBER => crate::svc::membership::kick_out(ctx, parameter).await.map(|_| ()),
        VOTE_SHARES => ctx
            .store
            .write(|tx| {
                let Some((row, shares)) = tx.table("corporation").get_id(corp).map(|c| (c.id, c.int("shares").unwrap_or(0))) else { return Ok(()) };
                tx.update("corporation", row, &[("shares", int(shares + parameter))])
            })
            .map_err(|e| db_failed("creating shares", e)),
        VOTE_ITEM_LOCKDOWN => ctx
            .store
            .write(|tx| {
                let optional = |v: Option<i64>| v.map_or(store::Value::Null, int);
                tx.insert(
                    "crpItemLocks",
                    &[
                        ("corporationID", int(corp)),
                        ("itemID", int(parameter)),
                        ("locationID", optional(location)),
                        ("typeID", optional(type_id)),
                        ("lockedAt", int(crate::bulkdata::filetime_now())),
                    ],
                )
                .map(|_| ())
            })
            .map_err(|e| db_failed("locking the item", e)),
        VOTE_ITEM_UNLOCK => ctx
            .store
            .write(|tx| tx.delete_where("crpItemLocks", &["corporationID", "itemID"], &[int(corp), int(parameter)]).map(|_| ()))
            .map_err(|e| db_failed("unlocking the item", e)),
        _ => Ok(()),
    }
}

// ---- CEO --------------------------------------------------------------------------------

/// Whether the caller may run for CEO: a member of a player corporation who
/// is not its CEO, has Corporation Management, and no election is open.
async fn can_run(ctx: &Ctx<'_>) -> Refused<()> {
    let (corp, me) = (ctx.session.corp_id.unwrap_or(0), ctx.session.char_id.unwrap_or(0));
    let (ceo, player, skilled, open) = ctx.store.read(|db| {
        let player = crate::svc::membership::is_player_corp(db, corp).unwrap_or(false);
        let skilled = crate::svc::skills::levels(db, me, &[SKILL_CORPORATION_MANAGEMENT])
            .map(|l| l.get(&SKILL_CORPORATION_MANAGEMENT).copied().unwrap_or(0) > 0)
            .unwrap_or(false);
        (ceo_of(db, corp), player, skilled, ceo_vote_open(db, corp))
    });
    if !player || ceo == Some(me) || !skilled || open {
        return Err(message("CantRunForCEOAtTheMoment"));
    }
    Ok(())
}

/// typeCorporationManagement.
const SKILL_CORPORATION_MANAGEMENT: i32 = 3363;

fn ceo_vote_open(db: &Db, corp: i64) -> bool {
    let now = crate::bulkdata::filetime_now();
    db.table("crpVoteCases")
        .find(&["corporationID"], &[int(corp)])
        .any(|c| c.int("voteType") == Some(VOTE_CEO) && c.int("endDateTime").is_some_and(|e| e > now))
}

/// A corporation's CEO.
fn ceo_of(db: &Db, corp: i64) -> Option<i64> {
    db.table("corporation").get_id(corp).and_then(|c| c.int("ceoID"))
}

/// ResignFromCEO(newCeoID=None): without a successor, the members to choose
/// from (or, with nobody else left, the corporation closes); with one, they
/// take over.
async fn resign(ctx: &Ctx<'_>, successor: Option<i64>) -> Refused<Value> {
    let (corp, me) = (ctx.session.corp_id.unwrap_or(0), ctx.session.char_id.unwrap_or(0));
    let ceo = ctx.store.read(|db| ceo_of(db, corp));
    if ceo != Some(me) {
        return Err(message("OnlyCEOCanResign"));
    }
    if ctx.session.station_id.is_none() {
        return Err(message("CrpCanNotChangeCorpInSpace"));
    }
    let others: Vec<i64> = ctx.store.read(|db| {
        db.table("character_").find(&["corporationID"], &[int(corp)]).filter_map(|r| r.int("characterID")).filter(|id| *id != me).collect()
    });
    match successor {
        None if others.is_empty() => close(ctx, corp, me).await.map(|_| Value::None),
        None => Ok(Value::List(others.into_iter().map(Value::Int).collect())),
        Some(next) if others.contains(&next) => {
            ctx.store.write(|tx| hand_over(tx, ctx.world, corp, me, next).map_err(Failed)).map_err(|Failed(o)| o)?;
            announce_resignation(ctx, corp, me, next).await;
            Ok(Value::None)
        }
        Some(_) => Err(message("CrpCharNotInThisCorp")),
    }
}

/// A new CEO takes over: they get every role, and both sessions are told.
pub fn hand_over(tx: &mut Tx<'_>, world: &World, corp: i64, old: i64, new: i64) -> Refused<()> {
    if let Some(row) = tx.table("corporation").get_id(corp).map(|c| c.id) {
        tx.update("corporation", row, &[("ceoID", int(new))]).map_err(|e| db_failed("changing the CEO", e))?;
    }
    let all = int(crate::svc::membership::ALL_ROLES);
    tx.upsert(
        "chrCorporationRoles",
        &["characterID"],
        &[("characterID", int(new)), ("rolesAtAll", all.clone()), ("grantableRoles", all.clone())],
        &[("rolesAtAll", all.clone()), ("grantableRoles", all)],
    )
    .map_err(|e| db_failed("giving the CEO their roles", e))?;
    info!("{new} is the new CEO of corporation {corp}, after {old}");
    let c = Value::Dict(vec![(Value::bytes("ceoID"), Value::Tuple(vec![Value::Int(old), Value::Int(new)]))]);
    tell(world, corp, "OnCorporationChanged", vec![Value::Int(corp), c]);
    world.lock().reaffiliate(&[], &[old, new]);
    Ok(())
}

/// A mail to the corporation's mailbox, from the outgoing CEO, naming who
/// takes over.
async fn announce_resignation(ctx: &Ctx<'_>, corp: i64, old: i64, new: i64) {
    let names = ctx.store.read(|db| {
        let name = |table: &str, id: i64, col: &str| db.table(table).get_id(id).and_then(|r| r.text(col)).unwrap_or_default().to_string();
        [name("character_", old, "characterName"), name("character_", new, "characterName"), name("corporation", corp, "corporationName")]
    });
    let name = |i: usize| names[i].as_str();
    let subject = format!("{} has resigned as CEO", name(0));
    let body = format!("{} has resigned as CEO of {}.<br>{} is the new CEO.", name(0), name(2), name(1));
    if let Err(e) = ctx.store.write(|tx| crate::svc::chat::deliver(tx, ctx.world, old, &[corp], &subject, &body)) {
        error!("mailing corporation {corp} about its new CEO: {}", crate::logging::chain(&e));
    }
}

/// The last member resigns: the corporation closes and they go back to
/// their NPC corporation.
async fn close(ctx: &Ctx<'_>, corp: i64, me: i64) -> Refused<()> {
    if ctx.session.alliance_id.is_some() {
        return Err(message("CanNotDestroyCorpInAlliance"));
    }
    let npc_corporations: Vec<i64> = crate::sde::table("crpNPCCorporations").rows().filter_map(|n| n.int("corporationID")).collect();
    let home = ctx
        .store
        .write(|tx| -> Result<i64, Failed> {
            // The NPC corporation they were in most recently.
            let home = tx
                .table("chrEmployment")
                .find(&["characterID"], &[int(me)])
                .filter(|e| e.int("corporationID").is_some_and(|c| npc_corporations.contains(&c)))
                .fold(None, |best: Option<(i64, i64)>, e| {
                    let (Some(corp), Some(start)) = (e.int("corporationID"), e.int("startDate")) else { return best };
                    if best.is_some_and(|(_, at)| at >= start) { best } else { Some((corp, start)) }
                })
                .map(|(corp, _)| corp)
                .ok_or_else(|| Outcome::custom("There is no NPC corporation for you to go back to."))?;
            crate::svc::membership::change_corporation(tx, me, home, 0, crate::bulkdata::filetime_now())?;
            if let Some(row) = tx.table("corporation").get_id(corp).map(|c| c.id) {
                tx.update("corporation", row, &[("deleted", int(1)), ("ceoID", int(0))]).map_err(|e| db_failed("closing the corporation", e))?;
            }
            Ok(home)
        })
        .map_err(|Failed(o)| o)?;
    crate::svc::membership::member_moved(ctx.world, me, corp, home);
    info!("{me} resigned as the last member of corporation {corp}, which is closed");
    ctx.world.lock().reaffiliate(&[], &[me]);
    Ok(())
}

/// Votes past their end: the option with the most shares behind it wins (the
/// first, on a tie) and becomes a sanctioned action. Run by the heartbeat.
pub async fn close_votes(world: &World, store: &Store) {
    let now = crate::bulkdata::filetime_now();
    let closed: Vec<(i64, i64, i64, Option<i64>)> = store.read(|db| {
        let sanctioned = db.table("crpSanctionedActions");
        db.table("crpVoteCases")
            .rows()
            .filter(|c| c.int("endDateTime").is_some_and(|e| e <= now) && sanctioned.get(std::slice::from_ref(c.value("voteCaseID"))).is_none())
            .map(|c| {
                let (case, corp) = (c.int("voteCaseID").unwrap_or(0), c.int("corporationID").unwrap_or(0));
                // The most shares behind it, then the first.
                let option = db
                    .table("crpVoteOptions")
                    .find(&["voteCaseID"], &[int(case)])
                    .filter_map(|o| o.int("optionID"))
                    .map(|option| (std::cmp::Reverse(votes_for(db, case, corp, option)), option))
                    .min()
                    .map(|(_, option)| option);
                (case, corp, c.int("endDateTime").unwrap_or(now), option)
            })
            .collect()
    });
    for (case, corp, end, option) in closed {
        let saved = store.write(|tx| {
            tx.insert(
                "crpSanctionedActions",
                &[
                    ("voteCaseID", int(case)),
                    ("corporationID", int(corp)),
                    ("timeActioned", int(now)),
                    ("actedUpon", int(0)),
                    ("inEffect", int(0)),
                    ("optionID", option.map_or(store::Value::Null, int)),
                    ("expires", int(end + ACT_WITHIN)),
                ],
            )
        });
        if let Err(e) = saved {
            error!("closing vote {case}: {}", crate::logging::chain(&e));
            continue;
        }
        info!("vote {case} of corporation {corp} closed: option {option:?}");
        elect(world, store, case, corp).await;
        if let Some(new) = store.read(|db| db.table("crpSanctionedActions").get_id(case).and_then(|s| sanction_line(db, &s))) {
            tell(world, corp, "OnSanctionedActionChanged", vec![Value::Int(corp), Value::Int(case), change(SANCTION_HEADER, None, Some(&new))]);
        }
    }
}

/// A CEO vote the candidate won takes effect as it closes.
async fn elect(world: &World, store: &Store, case: i64, corp: i64) {
    let found = store.read(|db| {
        let s = db.table("crpSanctionedActions").get_id(case)?;
        let c = db.table("crpVoteCases").get_id(case).filter(|c| c.int("voteType") == Some(VOTE_CEO))?;
        let o = db.table("crpVoteOptions").get(&[int(case), s.int("optionID").map_or(store::Value::Null, int)])?;
        let co = db.table("corporation").get_id(c.int("corporationID")?)?;
        let winner = o.int("parameter").filter(|w| *w != 0);
        let member = winner.is_some_and(|w| db.table("character_").get_id(w).is_some_and(|ch| ch.int("corporationID") == Some(corp)));
        Some((winner, co.int("ceoID").unwrap_or(0), member))
    });
    let Some((Some(winner), old, member)) = found else { return };
    if !member || winner == old {
        return;
    }
    if store.write(|tx| hand_over(tx, world, corp, old, winner).map_err(Failed)).is_ok() {
        let now = crate::bulkdata::filetime_now();
        let done = store.write(|tx| {
            let Some(row) = tx.table("crpSanctionedActions").get_id(case).map(|s| s.id) else { return Ok(()) };
            tx.update("crpSanctionedActions", row, &[("actedUpon", int(1)), ("inEffect", int(1)), ("timeActedUpon", int(now))])
        });
        if let Err(e) = done {
            error!("recording the election {case}: {}", crate::logging::chain(&e));
        }
    }
}

/// Whether an item is locked down by its corporation's vote.
pub fn locked(db: &Db, item: i64) -> bool {
    db.table("crpItemLocks").find(&["itemID"], &[int(item)]).next().is_some()
}

/// The stations where a corporation has items locked down.
pub async fn locked_locations(ctx: &Ctx<'_>) -> Value {
    let corp = ctx.session.corp_id.unwrap_or(0);
    let mut locations: Vec<i64> = Vec::new();
    ctx.store.read(|db| {
        for location in db.table("crpItemLocks").find(&["corporationID"], &[int(corp)]).filter_map(|r| r.int("locationID")) {
            if !locations.contains(&location) {
                locations.push(location);
            }
        }
    });
    Value::List(locations.into_iter().map(Value::Int).collect())
}

fn tell(world: &World, corp: i64, event: &'static str, args: Vec<Value>) {
    let w = world.lock();
    w.notify(&w.corp_members(corp), event, Value::Tuple(args));
}
