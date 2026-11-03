//! Trinity fleets, using the same membership as gangsta, bot hire and jump bridges.
//! Wire shapes and event signatures follow client/script/parklife/gang.py.
use std::collections::{BTreeMap, BTreeSet};

use super::membership::Refused;
use super::{bound, gang, rows::key_val};
use crate::services::{Call, Ctx, Outcome};
use crate::world::{Move, WorldState};
use eve_marshal::Value;

const LEADER: i64 = 1;
const WING: i64 = 2;
const SQUAD: i64 = 3;
const MEMBER: i64 = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct Member {
    wing: i64,
    squad: i64,
    role: i64,
    job: i64,
    booster: i64,
}

impl Member {
    fn value(&self, id: i64) -> Value {
        key_val(vec![
            ("charID", Value::Int(id)),
            ("wingID", Value::Int(self.wing)),
            ("squadID", Value::Int(self.squad)),
            ("role", Value::Int(self.role)),
            ("job", Value::Int(self.job)),
            ("roleBooster", Value::Int(self.booster)),
        ])
    }
}

#[derive(Debug, Clone, PartialEq)]
struct Wing {
    name: String,
    squads: BTreeMap<i64, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fleet {
    members: BTreeMap<i64, Member>,
    invitations: BTreeMap<i64, (i64, Member)>,
    wings: BTreeMap<i64, Wing>,
    next_id: i64,
    free_move: bool,
    observers: BTreeSet<i64>,
    watchlists: BTreeMap<i64, Vec<i64>>,
    next_update: i64,
    skills: BTreeMap<i64, BTreeMap<i32, i64>>,
    last_status: BTreeMap<i64, Value>,
}

impl Fleet {
    /// The formation a command rank can warp (menusvc.ImGangCommander).
    /// Being the creator by itself does not confer a command position.
    pub(super) fn warp_members(&self, me: i64) -> Refused<Vec<i64>> {
        let Some(commander) = self.members.get(&me) else {
            return Err(Outcome::custom("You are not a member of this fleet."));
        };
        if !matches!(commander.role, LEADER | WING | SQUAD) {
            return Err(Outcome::custom("Only a fleet, wing or squad commander can order a fleet warp."));
        }
        Ok(self.members.iter().filter(|(_, member)| match commander.role {
            LEADER => true,
            WING => member.wing == commander.wing && commander.wing > 0,
            SQUAD => member.wing == commander.wing && member.squad == commander.squad && commander.squad > 0,
            _ => false,
        }).map(|(id,_)|*id).collect())
    }

    pub(super) fn decline(&mut self, who: i64) {
        self.invitations.remove(&who);
    }
    fn new(leader: i64) -> Self {
        Self {
            members: [(
                leader,
                Member {
                    wing: -1,
                    squad: -1,
                    role: LEADER,
                    job: 2,
                    booster: 1,
                },
            )]
            .into(),
            invitations: BTreeMap::new(),
            wings: [(
                1,
                Wing {
                    name: "Wing 1".into(),
                    squads: [(2, "Squad 1".into())].into(),
                },
            )]
            .into(),
            next_id: 2,
            free_move: false,
            observers: BTreeSet::new(),
            watchlists: BTreeMap::new(),
            next_update: 0,
            skills: BTreeMap::new(),
            last_status: BTreeMap::new(),
        }
    }

    fn options(&self) -> Value {
        key_val(vec![
            ("isFreeMove", Value::Bool(self.free_move)),
            ("isVoiceEnabled", Value::Bool(false)),
        ])
    }

    fn commander(&self, me: i64) -> bool {
        self.members
            .get(&me)
            .is_some_and(|m| m.role == LEADER || m.job & 2 != 0)
    }

    fn authorize(&self, me: i64) -> Refused<()> {
        if self.commander(me) {
            Ok(())
        } else {
            Err(Outcome::custom(
                "Only the fleet commander or creator can do that.",
            ))
        }
    }

    /// Wing and squad command actions are confined to their own formation.
    /// Trinity exposes these actions to all three command ranks.
    fn authorize_group(&self, me: i64, wing: i64, squad: i64) -> Refused<()> {
        if self.commander(me) || self.members.get(&me).is_some_and(|m|
            (m.role == WING && m.wing == wing && wing > 0)
                || (m.role == SQUAD && m.wing == wing && m.squad == squad && squad > 0)) {
            Ok(())
        } else {
            Err(Outcome::custom("That formation is outside your command."))
        }
    }

    fn authorize_member(&self, me: i64, member: &Member) -> Refused<()> {
        self.authorize_group(me, member.wing, member.squad)?;
        if self.commander(me) || self.members.get(&me).is_some_and(|m| member.role >= m.role) {
            Ok(())
        } else {
            Err(Outcome::custom("That command position is above your rank."))
        }
    }

    /// Validate the destination before mutating, including reservations from invites.
    fn placement(&self, who: i64, wing: i64, squad: i64, role: i64) -> Refused<Member> {
        if !(LEADER..=MEMBER).contains(&role) {
            return Err(Outcome::custom("Invalid fleet role."));
        }
        let (wing, squad) = if role == LEADER {
            (-1, -1)
        } else if role == WING {
            (wing, -1)
        } else {
            (wing, squad)
        };
        if role != LEADER && !self.wings.contains_key(&wing) {
            return Err(Outcome::custom("That wing does not exist."));
        }
        if role >= SQUAD && !self.wings[&wing].squads.contains_key(&squad) {
            return Err(Outcome::custom("That squad does not exist."));
        }
        let others = self
            .members
            .iter()
            .map(|(id, m)| (*id, m))
            .chain(self.invitations.iter().map(|(id, (_, m))| (*id, m)))
            .filter(|(id, _)| *id != who)
            .map(|(_, m)| m)
            .collect::<Vec<_>>();
        if role < MEMBER
            && others
                .iter()
                .any(|m| m.role == role && m.wing == wing && m.squad == squad)
        {
            return Err(Outcome::custom(
                "That command position is already occupied.",
            ));
        }
        if squad > 0 && others.iter().filter(|m| m.squad == squad).count() >= 10 {
            return Err(Outcome::custom("A squad can contain at most ten pilots."));
        }
        Ok(Member {
            wing,
            squad,
            role,
            job: self.members.get(&who).map_or(0, |m| m.job),
            booster: 0,
        })
    }

    fn default_placement(&self, who: i64) -> Refused<Member> {
        for (wing, w) in &self.wings {
            for squad in w.squads.keys() {
                if let Ok(m) = self.placement(who, *wing, *squad, MEMBER) {
                    return Ok(m);
                }
            }
        }
        Err(Outcome::custom(
            "There is no free squad position. Create a squad first.",
        ))
    }
}

fn fleet(w: &WorldState, id: i64) -> Refused<&Fleet> {
    w.gangs
        .get(id)
        .and_then(|g| g.fleet.as_ref())
        .ok_or_else(|| Outcome::custom("That fleet no longer exists."))
}

fn fleet_mut(w: &mut WorldState, id: i64) -> Refused<&mut Fleet> {
    w.gangs
        .gangs
        .get_mut(&id)
        .and_then(|g| g.fleet.as_mut())
        .ok_or_else(|| Outcome::custom("That fleet no longer exists."))
}

fn tell(w: &WorldState, id: i64, event: &'static str, args: Vec<Value>) {
    if let Some(g) = w.gangs.get(id) {
        gang::tell(w, &g.members, event, args);
    }
}

fn session(w: &WorldState, id: i64, me: i64) {
    let Ok(f) = fleet(w, id) else { return };
    let Some(m) = f.members.get(&me) else { return };
    for client in w.clients_of(me) {
        w.push_move(
            client,
            Move::Fleet {
                gang_id: Some(id),
                role: m.role,
                wing_id: Some(m.wing),
                squad_id: Some(m.squad),
            },
        );
    }
}

fn changed(w: &WorldState, id: i64, who: i64, old: &Member, new: &Member) {
    session(w, id, who);
    tell(
        w,
        id,
        "OnGangMemberChanged",
        vec![
            Value::Int(who),
            Value::Int(id),
            Value::Int(old.wing),
            Value::Int(old.squad),
            Value::Int(old.role),
            Value::Int(old.job),
            Value::Int(old.booster),
            Value::Int(new.wing),
            Value::Int(new.squad),
            Value::Int(new.role),
            Value::Int(new.job),
            Value::Int(new.booster),
        ],
    );
}

pub async fn object_handler(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let me = ctx.session.char_id.unwrap_or(0);
    if me == 0 {
        return Some(Outcome::custom("Select a character first."));
    }
    if call.method == "Invite" {
        return Some(
            invite(call, ctx, me)
                .await
                .map_or_else(|e| e, Outcome::Value),
        );
    }
    let mut w = ctx.world.lock();
    Some(object_call(call, &mut w, me, ctx.node_id).map_or_else(|e| e, Outcome::Value))
}

fn object_call(call: &Call, w: &mut WorldState, me: i64, node: i64) -> Refused<Value> {
    if call.method == "CreateGang" {
        if w.gangs.of(me).is_some() {
            return Err(Outcome::custom("You already belong to a fleet."));
        }
        let id = w.gangs.form(me);
        w.gangs.gangs.get_mut(&id).unwrap().fleet = Some(Fleet::new(me));
        session(w, id, me);
        gang::tell_system(w, me, Some(id));
        return Ok(bound::object_ref(node, "gangSvcObjectHandler", &[id]));
    }
    let id = bound::parse(&call.service)
        .and_then(|b| b.params.first().copied())
        .or_else(|| w.gangs.of(me))
        .ok_or_else(|| Outcome::custom("You do not belong to a fleet."))?;
    if call.method == "AcceptInvite" {
        if w.gangs.of(me).is_some() {
            return Err(Outcome::custom(
                "Leave your current fleet before accepting.",
            ));
        }
        let f = fleet(w, id)?;
        let (_, proposed) = f
            .invitations
            .get(&me)
            .ok_or_else(|| Outcome::custom("You have not been invited to that fleet."))?;
        let member = f.placement(me, proposed.wing, proposed.squad, proposed.role)?;
        if !w.gangs.join(id, me) {
            return Err(Outcome::custom("That invitation has expired."));
        }
        let f = fleet_mut(w, id)?;
        f.invitations.remove(&me);
        f.members.insert(me, member.clone());
        session(w, id, me);
        gang::tell_system(w, me, Some(id));
        // The accepting client initializes its own object after AcceptInvite returns.
        let others = w
            .gangs
            .get(id)
            .unwrap()
            .members
            .iter()
            .copied()
            .filter(|c| *c != me)
            .collect::<Vec<_>>();
        gang::tell(w, &others, "OnGangJoin", vec![member.value(me)]);
        return Ok(Value::None);
    }
    if call.method == "RejectInvite" {
        let (inviter, _) = fleet_mut(w, id)?
            .invitations
            .remove(&me)
            .ok_or_else(|| Outcome::custom("No invitation is pending."))?;
        w.gangs
            .gangs
            .get_mut(&id)
            .unwrap()
            .invited
            .retain(|c| *c != me);
        gang::tell(
            w,
            &[inviter],
            "OnGangJoinReject",
            vec![
                Value::Int(me),
                Value::text(if call.arg_id(0) != 0 {
                    "Already in a fleet"
                } else {
                    ""
                }),
            ],
        );
        return Ok(Value::None);
    }
    if !fleet(w, id)?.members.contains_key(&me) {
        return Err(Outcome::custom("You are not a member of that fleet."));
    }
    match call.method.as_str() {
        "Init" | "FinishMove" => {
            session(w, id, me);
            Ok(Value::None)
        }
        "GetGangID" => Ok(Value::Int(id)),
        "GetMembers" => Ok(Value::Dict(
            fleet(w, id)?
                .members
                .iter()
                .map(|(id, m)| (Value::Int(*id), m.value(*id)))
                .collect(),
        )),
        "GetWings" => Ok(Value::Dict(
            fleet(w, id)?
                .wings
                .iter()
                .map(|(id, wing)| {
                    (
                        Value::Int(*id),
                        key_val(vec![
                            ("name", Value::text(&wing.name)),
                            (
                                "squads",
                                Value::Dict(
                                    wing.squads
                                        .iter()
                                        .map(|(sid, name)| {
                                            (
                                                Value::Int(*sid),
                                                key_val(vec![("name", Value::text(name))]),
                                            )
                                        })
                                        .collect(),
                                ),
                            ),
                        ]),
                    )
                })
                .collect(),
        )),
        "GetOptions" => Ok(fleet(w, id)?.options()),
        "GetVoiceMuteStatus" | "GetExcludedFromMute" => Ok(Value::Dict(Vec::new())),
        "GetGangMemberLocations" => Ok(super::rows::rowset(
            &["characterID", "locationID"],
            w.gangs
                .get(id)
                .unwrap()
                .members
                .iter()
                .filter_map(|c| {
                    w.presence_of(*c)
                        .map(|p| Value::List(vec![Value::Int(*c), Value::Int(p.solar_system_id)]))
                })
                .collect(),
        )),
        "LeaveGang" => {
            gang::leave(w, me)?;
            Ok(Value::None)
        }
        "KickMember" => {
            let target = call.arg_id(0);
            let f = fleet(w, id)?;
            let member = f.members.get(&target).ok_or_else(|| Outcome::custom("That pilot is not in this fleet."))?;
            f.authorize_member(me, member)?;
            gang::part(w, id, target, true);
            Ok(Value::None)
        }
        "DisbandGang" => {
            fleet(w, id)?.authorize(me)?;
            let members = w.gangs.get(id).unwrap().members.clone();
            for member in members {
                gang::part(w, id, member, true);
            }
            Ok(Value::None)
        }
        "SetOptions" => {
            fleet(w, id)?.authorize(me)?;
            let old = fleet(w, id)?.options();
            let arg = call
                .args
                .first()
                .ok_or_else(|| Outcome::custom("Missing fleet options."))?;
            if field_bool(arg, "isVoiceEnabled") == Some(true) {
                return Err(Outcome::custom(
                    "Voice chat is not available on this server.",
                ));
            }
            if let Some(enabled) = field_bool(arg, "isFreeMove") {
                fleet_mut(w, id)?.free_move = enabled;
            }
            tell(
                w,
                id,
                "OnGangOptionsChanged",
                vec![old, fleet(w, id)?.options()],
            );
            Ok(Value::None)
        }
        "CreateWing" | "CreateSquad" | "DeleteWing" | "DeleteSquad" | "ChangeWingName"
        | "ChangeSquadName" => hierarchy(call, w, id, me),
        "MoveMember" | "MakeLeader" | "SetScout" | "SetBooster" => move_member(call, w, id, me),
        method if method.starts_with("SendBroadcast_") => {
            broadcast(w, id, me, &method[14..], &call.args, BroadcastScope::Fleet)
        }
        _ => Err(Outcome::Unknown(format!(
            "gangSvcObjectHandler::{}",
            call.method
        ))),
    }
}

fn field_bool(value: &Value, key: &str) -> Option<bool> {
    match value {
        Value::Instance { state, .. } => field_bool(state, key),
        Value::Dict(entries) => entries
            .iter()
            .find(|(k, _)| k.as_string().as_deref() == Some(key))
            .and_then(|(_, v)| v.as_i64())
            .map(|v| v != 0),
        _ => None,
    }
}

async fn invite(call: &Call, ctx: &Ctx<'_>, me: i64) -> Refused<Value> {
    let target = call.arg_id(0);
    let (id, member, is_bot, members) = {
        let w = ctx.world.lock();
        let id = w
            .gangs
            .of(me)
            .ok_or_else(|| Outcome::custom("Create a fleet first."))?;
        if bound::parse(&call.service)
            .and_then(|b| b.params.first().copied())
            .is_some_and(|b| b != id)
        {
            return Err(Outcome::custom("That is not your fleet."));
        }
        let f = fleet(&w, id)?;
        if target == me || w.gangs.of(target).is_some() {
            return Err(Outcome::custom("That pilot already belongs to a fleet."));
        }
        if f.invitations.contains_key(&target) {
            return Err(Outcome::custom("That pilot has already been invited."));
        }
        if f.members.len() + f.invitations.len() >= 256 {
            return Err(Outcome::custom("The fleet is full."));
        }
        let bot = w.bot_index.contains_key(&target);
        if !bot && !w.is_online(target) {
            return Err(Outcome::custom("That pilot is offline."));
        }
        let role = call.arg_i64(3).filter(|v| *v > 0).unwrap_or(MEMBER);
        let m = if role == MEMBER && call.arg_id(2) <= 0 {
            f.wings.iter().flat_map(|(wid, wing)| wing.squads.keys().map(move |sid| (*wid, *sid)))
                .filter_map(|(wid, sid)| f.placement(target, wid, sid, MEMBER).ok())
                .find(|m| f.authorize_member(me, m).is_ok())
                .ok_or_else(|| Outcome::custom("There is no free squad position in your command."))?
        } else {
            f.placement(target, call.arg_id(1), call.arg_id(2), role)?
        };
        f.authorize_member(me, &m)?;
        (id, m, bot, w.gangs.get(id).unwrap().members.clone())
    };
    let mut enemies = Vec::new();
    for member in members {
        enemies.extend(super::war::enemies_of(gang::side_of(ctx.db, member).await));
    }
    enemies.sort_unstable();
    enemies.dedup();
    let brought = super::war::enemies_of(gang::side_of(ctx.db, target).await)
        .into_iter()
        .filter(|e| !enemies.contains(e))
        .collect::<Vec<_>>();
    if call.arg_id(4) == 0 && !brought.is_empty() {
        let names = gang::names_of(ctx.db, &brought).await;
        return Err(gang::message_with(
            "GangCandidateDodgyN",
            &[("corpNames", Value::text(names.join(", ")))],
        ));
    }
    let name = gang::name_of(ctx.db, me).await;
    let enemy_names = gang::names_of(ctx.db, &enemies).await;
    let mut w = ctx.world.lock();
    // Revalidate after database awaits: the fleet or destination may have changed.
    let f = fleet(&w, id)?;
    if f.invitations.contains_key(&target) {
        return Err(Outcome::custom("That pilot has already been invited."));
    }
    if f.members.len() + f.invitations.len() >= 256 {
        return Err(Outcome::custom("The fleet is full."));
    }
    let member = f.placement(target, member.wing, member.squad, member.role)?;
    f.authorize_member(me, &member)?;
    if w.gangs.of(target).is_some() {
        return Err(Outcome::custom("That pilot already joined a fleet."));
    }
    fleet_mut(&mut w, id)?
        .invitations
        .insert(target, (me, member));
    w.gangs.gangs.get_mut(&id).unwrap().invited.push(target);
    if is_bot {
        w.hire_requests.push((me, target));
    } else {
        let message = if enemy_names.is_empty() {
            "AskJoinGang"
        } else {
            "GangMembersDodgyN"
        };
        let args = Value::Dict(vec![
            (Value::bytes("name"), Value::text(name)),
            (
                Value::bytes("corpNames"),
                Value::text(enemy_names.join(", ")),
            ),
        ]);
        gang::tell(
            &w,
            &[target],
            "OnGangInvite",
            vec![Value::Int(id), Value::Int(me), Value::text(message), args],
        );
    }
    Ok(Value::None)
}

fn hierarchy(call: &Call, w: &mut WorldState, id: i64, me: i64) -> Refused<Value> {
    let f = fleet_mut(w, id)?;
    let target = call.arg_id(0);
    match call.method.as_str() {
        "CreateWing" => f.authorize(me)?,
        "CreateSquad" | "DeleteWing" | "ChangeWingName" => f.authorize_group(me, target, -1)?,
        _ => {
            let wing = f.wings.iter().find(|(_, wing)| wing.squads.contains_key(&target)).map(|(id,_)| *id)
                .ok_or_else(|| Outcome::custom("That squad does not exist."))?;
            f.authorize_group(me, wing, target)?;
        }
    }
    let (event, args, result) = match call.method.as_str() {
        "CreateWing" => {
            if f.wings.len() >= 5 {
                return Err(Outcome::custom("A fleet can contain at most five wings."));
            }
            f.next_id += 1;
            let wid = f.next_id;
            f.wings.insert(
                wid,
                Wing {
                    name: format!("Wing {}", f.wings.len() + 1),
                    squads: BTreeMap::new(),
                },
            );
            ("OnGangWingAdded", vec![Value::Int(wid)], Value::Int(wid))
        }
        "CreateSquad" => {
            let wing = f
                .wings
                .get_mut(&target)
                .ok_or_else(|| Outcome::custom("That wing does not exist."))?;
            if wing.squads.len() >= 5 {
                return Err(Outcome::custom("A wing can contain at most five squads."));
            }
            f.next_id += 1;
            let sid = f.next_id;
            wing.squads
                .insert(sid, format!("Squad {}", wing.squads.len() + 1));
            (
                "OnGangSquadAdded",
                vec![Value::Int(target), Value::Int(sid)],
                Value::Int(sid),
            )
        }
        "DeleteWing" | "DeleteSquad" => {
            let wing = call.method == "DeleteWing";
            if f.members
                .values()
                .chain(f.invitations.values().map(|(_, m)| m))
                .any(|m| {
                    if wing {
                        m.wing == target
                    } else {
                        m.squad == target
                    }
                })
            {
                return Err(Outcome::custom(
                    "Move the pilots and clear pending invitations before deleting this group.",
                ));
            }
            let removed = if wing {
                f.wings.remove(&target).is_some()
            } else {
                f.wings
                    .values_mut()
                    .any(|w| w.squads.remove(&target).is_some())
            };
            if !removed {
                return Err(Outcome::custom("That group does not exist."));
            }
            (
                if wing {
                    "OnGangWingDeleted"
                } else {
                    "OnGangSquadDeleted"
                },
                vec![Value::Int(target)],
                Value::None,
            )
        }
        _ => {
            let name = call.arg_text(1).unwrap_or_default().trim().to_string();
            if name.is_empty() || name.chars().count() > 10 {
                return Err(Outcome::custom(
                    "Group names must contain one to ten characters.",
                ));
            }
            let wing = call.method == "ChangeWingName";
            let slot = if wing {
                f.wings.get_mut(&target).map(|w| &mut w.name)
            } else {
                f.wings.values_mut().find_map(|w| w.squads.get_mut(&target))
            };
            *slot.ok_or_else(|| Outcome::custom("That group does not exist."))? = name.clone();
            (
                if wing {
                    "OnGangWingNameChanged"
                } else {
                    "OnGangSquadNameChanged"
                },
                vec![Value::Int(target), Value::text(name)],
                Value::None,
            )
        }
    };
    tell(w, id, event, args);
    Ok(result)
}

fn move_member(call: &Call, w: &mut WorldState, id: i64, me: i64) -> Refused<Value> {
    let target = call.arg_id(0);
    let f = fleet(w, id)?;
    let old = f
        .members
        .get(&target)
        .cloned()
        .ok_or_else(|| Outcome::custom("That pilot is not a fleet member."))?;
    let free_move = call.method == "MoveMember"
        && target == me
        && f.free_move
        && old.role == MEMBER
        && call.arg_id(3) == MEMBER;
    if matches!(call.method.as_str(), "MakeLeader" | "SetScout") {
        f.authorize(me)?;
    } else if !free_move {
        f.authorize_member(me, &old)?;
    }
    if call.method == "MakeLeader" {
        if let Some((&leader, current)) = f.members.iter().find(|(_, m)| m.role == LEADER) {
            if leader == target {
                return Ok(Value::None);
            }
            let current = current.clone();
            // Swap positions so the former commander always has a valid destination.
            let replacement = Member {
                job: current.job & !2,
                ..old.clone()
            };
            fleet_mut(w, id)?
                .members
                .insert(leader, replacement.clone());
            changed(w, id, leader, &current, &replacement);
        }
        let new = Member {
            wing: -1,
            squad: -1,
            role: LEADER,
            job: old.job | 2,
            booster: 1,
        };
        fleet_mut(w, id)?.members.insert(target, new.clone());
        w.gangs.gangs.get_mut(&id).unwrap().leader = Some(target);
        changed(w, id, target, &old, &new);
        return Ok(Value::Bool(true));
    }
    let f = fleet(w, id)?;
    let mut new = old.clone();
    match call.method.as_str() {
        "MoveMember" => {
            new = f.placement(target, call.arg_id(1), call.arg_id(2), call.arg_id(3))?;
            new.booster = call.arg_i64(4).unwrap_or(old.booster);
            if free_move && ((old.booster == 3 && old.squad != new.squad) || (old.booster == 2 && old.wing != new.wing)) {
                new.booster = 0;
            }
            if !free_move { f.authorize_member(me, &new)?; }
        }
        "SetScout" => new.job = (old.job & !1) | i64::from(call.arg_id(1) != 0),
        "SetBooster" => new.booster = call.arg_id(1),
        _ => unreachable!(),
    }
    if new.booster != old.booster && !f.commander(me) {
        let rank = f.members.get(&me).map_or(MEMBER, |m| m.role);
        if (free_move && new.booster != 0)
            || (!free_move && ((new.booster != 0 && new.booster < rank) || (old.booster != 0 && old.booster < rank))) {
            return Err(Outcome::custom("That booster assignment is outside your command."));
        }
    }
    if !(0..=3).contains(&new.booster)
        || (new.booster >= 2 && new.wing <= 0)
        || (new.booster == 3 && new.squad <= 0)
    {
        return Err(Outcome::custom(
            "That booster role is not valid for this position.",
        ));
    }
    // Each group can have one booster. Remove its former designation first.
    let duplicates = f
        .members
        .iter()
        .filter(|(id, m)| {
            **id != target
                && new.booster > 0
                && m.booster == new.booster
                && (new.booster == 1
                    || (m.wing == new.wing && (new.booster == 2 || m.squad == new.squad)))
        })
        .map(|(id, m)| (*id, m.clone()))
        .collect::<Vec<_>>();
    for (other, previous) in duplicates {
        let mut replacement = previous.clone();
        replacement.booster = 0;
        fleet_mut(w, id)?.members.insert(other, replacement.clone());
        changed(w, id, other, &previous, &replacement);
    }
    fleet_mut(w, id)?.members.insert(target, new.clone());
    if old.role == LEADER || new.role == LEADER {
        w.gangs.gangs.get_mut(&id).unwrap().leader = if new.role == LEADER {
            Some(target)
        } else {
            None
        };
    }
    changed(w, id, target, &old, &new);
    Ok(Value::Bool(true))
}

pub fn bot_joined(w: &mut WorldState, id: i64, bot: i64, before: &[i64]) {
    let Ok(f) = fleet_mut(w, id) else { return };
    let member = f
        .invitations
        .remove(&bot)
        .map(|(_, m)| m)
        .or_else(|| f.default_placement(bot).ok());
    let Some(member) = member else {
        gang::part(w, id, bot, false);
        return;
    };
    f.members.insert(bot, member.clone());
    gang::tell(w, before, "OnGangJoin", vec![member.value(bot)]);
    for member in before {
        gang::tell_system(w, *member, Some(id));
    }
}

pub fn departed(w: &mut WorldState, id: i64, who: i64) {
    let Ok(f) = fleet_mut(w, id) else { return };
    f.observers.remove(&who);
    f.last_status.remove(&who);
    f.watchlists.remove(&who);
    for watched in f.watchlists.values_mut() {
        watched.retain(|c| *c != who);
    }
    let creator = f.members.remove(&who).is_some_and(|m| m.job & 2 != 0);
    if creator {
        if let Some((&next, member)) = f.members.iter().min_by_key(|(id, m)| (m.role, **id)) {
            let old = member.clone();
            let mut new = old.clone();
            new.job |= 2;
            f.members.insert(next, new.clone());
            changed(w, id, next, &old, &new);
        }
    }
}

/// The command skills' limits are given in the imported Trinity type descriptions:
/// Leadership: two squad members/level; Wing Command: one squad/level;
/// Fleet Command: one wing/level. Bonuses also require commanders in space
/// in the receiving pilot's system, as the skill descriptions state.
fn active_groups(
    w: &WorldState,
    f: &Fleet,
    system: i64,
) -> (bool, BTreeMap<i64, bool>, BTreeMap<i64, bool>) {
    let here = |id: i64| {
        w.presence_of(id)
            .is_some_and(|p| !p.docked && p.solar_system_id == system)
    };
    let level = |id: i64, skill: i32| {
        f.skills
            .get(&id)
            .and_then(|s| s.get(&skill))
            .copied()
            .unwrap_or(0)
    };
    let mut squads = BTreeMap::new();
    let mut wings = BTreeMap::new();
    for (wid, wing) in &f.wings {
        for sid in wing.squads.keys() {
            let members = f
                .members
                .iter()
                .filter(|(_, m)| m.squad == *sid)
                .collect::<Vec<_>>();
            let count = members.len() as i64;
            let present = members.iter().filter(|(id, _)| here(**id)).count();
            let active = present >= 2
                && members
                    .iter()
                    .any(|(id, m)| m.role == SQUAD && here(**id) && level(**id, 3348) * 2 >= count);
            squads.insert(*sid, active);
        }
        let active = !wing.squads.is_empty()
            && f.members.iter().any(|(id, m)| {
                m.wing == *wid
                    && m.role == WING
                    && here(*id)
                    && level(*id, 11574) >= wing.squads.len() as i64
            });
        wings.insert(*wid, active);
    }
    let active = !f.wings.is_empty()
        && f.members.iter().any(|(id, m)| {
            m.role == LEADER && here(*id) && level(*id, 24764) >= f.wings.len() as i64
        });
    (active, wings, squads)
}

fn active_status(w: &WorldState, me: i64) -> Value {
    let f = w.gangs.of(me).and_then(|id| fleet(w, id).ok());
    let system = w
        .presence_of(me)
        .filter(|p| !p.docked)
        .map(|p| p.solar_system_id)
        .unwrap_or(0);
    let (active, wings, squads) = f.map(|f| active_groups(w, f, system)).unwrap_or_default();
    key_val(vec![
        ("fleet", Value::Bool(active)),
        (
            "wings",
            Value::Dict(
                wings
                    .into_iter()
                    .map(|(id, active)| (Value::Int(id), Value::Bool(active)))
                    .collect(),
            ),
        ),
        (
            "squads",
            Value::Dict(
                squads
                    .into_iter()
                    .map(|(id, active)| (Value::Int(id), Value::Bool(active)))
                    .collect(),
            ),
        ),
    ])
}

/// Highest applicable skill bonus for each target attribute, in percent. Fleet,
/// wing and squad bonuses do not add together; only the best reaches the ship.
/// Mining amount (77) applies to mining modules, the rest to the ship.
pub fn bonuses(w: &WorldState, me: i64) -> BTreeMap<i32, f64> {
    let Some(system) = w
        .presence_of(me)
        .filter(|p| !p.docked)
        .map(|p| p.solar_system_id)
    else {
        return BTreeMap::new();
    };
    let Some(f) = w.gangs.of(me).and_then(|id| fleet(w, id).ok()) else {
        return BTreeMap::new();
    };
    let Some(member) = f.members.get(&me) else {
        return BTreeMap::new();
    };
    let (fleet_active, wings, squads) = active_groups(w, f, system);
    let squad_active = squads.get(&member.squad).copied().unwrap_or(false);
    let wing_active = wings.get(&member.wing).copied().unwrap_or(false);
    let mut bonuses = BTreeMap::<i32, f64>::new();
    for (id, booster) in &f.members {
        if !w
            .presence_of(*id)
            .is_some_and(|p| !p.docked && p.solar_system_id == system)
        {
            continue;
        }
        let applies = match booster.booster {
            1 => {
                fleet_active
                    && (member.role == LEADER
                        || (wing_active && (member.role == WING || squad_active)))
            }
            2 => {
                booster.wing == member.wing && wing_active && (member.role == WING || squad_active)
            }
            3 => booster.squad == member.squad && squad_active,
            _ => false,
        };
        if !applies {
            continue;
        }
        for (skill, attr) in [
            (3348, 564),
            (3349, 37),
            (3350, 263),
            (20494, 265),
            (20495, 76),
            (22536, 77),
        ] {
            let percent = f
                .skills
                .get(id)
                .and_then(|s| s.get(&skill))
                .copied()
                .unwrap_or(0) as f64
                * 2.0;
            if percent > 0.0 {
                let best = bonuses.entry(attr).or_default();
                *best = best.max(percent);
            }
        }
    }
    bonuses
}

pub async fn refresh_skills(
    world: &crate::world::World,
    db: &tokio_postgres::Client,
) -> Result<(), tokio_postgres::Error> {
    let ids = world
        .lock()
        .gangs
        .gangs
        .values()
        .filter_map(|g| g.fleet.as_ref())
        .flat_map(|f| f.members.keys().map(|id| *id as i32))
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Ok(());
    }
    let rows = db.query(r#"SELECT e."ownerID", e."typeID", max(coalesce(a."valueInt", a."valueFloat"::bigint, 0))
        FROM entity e JOIN entity_attributes a ON a."itemID" = e."itemID" AND a."attributeID" = 280
        WHERE e."ownerID" = ANY($1) AND e.flag IN (7,61) AND e."typeID" = ANY($2)
        GROUP BY e."ownerID", e."typeID""#, &[&ids, &vec![3348i32,3349,3350,11574,20494,20495,22536,24764]]).await?;
    let mut skills = BTreeMap::<i64, BTreeMap<i32, i64>>::new();
    for row in rows {
        skills
            .entry(row.get::<_, i32>(0) as i64)
            .or_default()
            .insert(row.get(1), row.get::<_, i64>(2).clamp(0, 5));
    }
    for g in world.lock().gangs.gangs.values_mut() {
        if let Some(f) = &mut g.fleet {
            f.skills = f
                .members
                .keys()
                .filter_map(|id| skills.get(id).map(|levels| (*id, levels.clone())))
                .collect();
        }
    }
    Ok(())
}

/// Recompute external dogma modifiers after membership, position or skill
/// changes. Catalog state also serves fitting and item-info RPCs consistently.
pub async fn update_bonuses(world: &crate::world::World, db: &tokio_postgres::Client, catalog: &crate::dogma::Catalog) {
    let next = {
        let w = world.lock();
        w.gangs.gangs.values().filter_map(|g| g.fleet.as_ref()).flat_map(|f| f.members.keys())
            .map(|id| (*id, bonuses(&w, *id))).filter(|(_, b)| !b.is_empty()).collect()
    };
    for (char_id, previous) in catalog.replace_fleet_bonuses(next) {
        if let Err(e) = super::inflight::fleet_refit(db, catalog, world, char_id, &previous).await {
            tracing::warn!("updating fleet bonuses for {char_id}: {e}");
            catalog.restore_fleet_bonuses(char_id, previous);
        }
    }
}

pub async fn gang_svc(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if matches!(
        call.method.as_str(),
        "GetActiveStatus" | "RegisterForDamageUpdates"
    ) {
        if let Err(e) = refresh_skills(ctx.world, ctx.db).await {
            return Some(Outcome::custom(format!("Could not load fleet skills: {e}")));
        }
    }
    let me = ctx.session.char_id.unwrap_or(0);
    let mut w = ctx.world.lock();
    let result = match call.method.as_str() {
        "GetActiveStatus" => Ok(active_status(&w, me)),
        "RegisterForDamageUpdates" => {
            if let Some(id) = w.gangs.of(me) {
                if let Ok(f) = fleet_mut(&mut w, id) {
                    let watched = call
                        .args
                        .first()
                        .and_then(Value::seq)
                        .unwrap_or(&[])
                        .iter()
                        .filter_map(Value::as_i64)
                        .filter(|c| *c != me && f.members.contains_key(c))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .take(10)
                        .collect();
                    f.watchlists.insert(me, watched);
                    f.next_update = 0;
                }
            }
            Ok(active_status(&w, me))
        }
        "ForceLeaveGang" => gang::leave(&mut w, me).map(|_| Value::None),
        "RegisterObserveGang" | "UnRegisterObserveGang" => {
            if let Some(id) = w.gangs.of(me) {
                if let Ok(f) = fleet_mut(&mut w, id) {
                    if call.method == "RegisterObserveGang" {
                        f.observers.insert(me);
                        f.next_update = 0;
                    } else {
                        f.observers.remove(&me);
                    }
                }
            }
            Ok(Value::None)
        }
        "BroadcastToBubble" | "BroadcastToSystem" => relay_broadcast(call, &w, me),
        _ => return None,
    };
    Some(result.map_or_else(|e| e, Outcome::Value))
}

#[derive(Clone, Copy, PartialEq)]
enum BroadcastScope { Fleet, System, Bubble }

fn relay_broadcast(call: &Call, w: &WorldState, me: i64) -> Refused<Value> {
    let id = w.gangs.of(me).ok_or_else(|| Outcome::custom("You are not in a fleet."))?;
    let name = call.arg_text(0).ok_or_else(|| Outcome::custom("Missing fleet broadcast name."))?;
    let args = call.args.get(1..).filter(|args| !args.is_empty())
        .ok_or_else(|| Outcome::custom("Missing fleet broadcast target."))?;
    broadcast(w, id, me, &name, args,
        if call.method == "BroadcastToBubble" { BroadcastScope::Bubble } else { BroadcastScope::System })
}

fn broadcast(
    w: &WorldState,
    id: i64,
    me: i64,
    name: &str,
    args: &[Value],
    scope: BroadcastScope,
) -> Refused<Value> {
    let (event, near) = match name {
        "EnemySpotted" => ("OnGangBroadcast_EnemySpotted", true),
        "NeedBackup" => ("OnGangBroadcast_NeedBackup", true),
        "HoldPosition" => ("OnGangBroadcast_HoldPosition", true),
        "InPosition" => ("OnGangBroadcast_InPosition", true),
        "Location" => ("OnGangBroadcast_Location", false),
        "TravelTo" => ("OnGangBroadcast_TravelTo", false),
        "JumpBeacon" => ("OnGangBroadcast_JumpBeacon", false),
        "HealArmor" => ("OnGangBroadcast_HealArmor", false),
        "HealShield" => ("OnGangBroadcast_HealShield", false),
        "HealCapacitor" => ("OnGangBroadcast_HealCapacitor", false),
        "Target" => ("OnGangBroadcast_Target", false),
        "WarpTo" => ("OnGangBroadcast_WarpTo", false),
        "AlignTo" => ("OnGangBroadcast_AlignTo", false),
        "JumpTo" => ("OnGangBroadcast_JumpTo", false),
        _ => return Err(Outcome::custom("Unknown fleet broadcast.")),
    };
    let p = w
        .presence_of(me)
        .ok_or_else(|| Outcome::custom("You are not online."))?;
    if scope != BroadcastScope::Fleet && p.docked {
        return Err(Outcome::custom("Undock before broadcasting to nearby ships."));
    }
    let park = w.parks.get(&p.solar_system_id);
    let position = |who| park.and_then(|park| park.ships.values().find(|s| s.pilot.char_id == who && s.drone.is_none())).map(|s| s.pos);
    let sender_position = position(me);
    if scope == BroadcastScope::Bubble && sender_position.is_none() {
        return Err(Outcome::custom("Your ship is not in space."));
    }
    let target = args.first().cloned().unwrap_or(Value::None);
    let mut payload = vec![Value::Int(me)];
    if name == "Location" && target.seq().is_none_or(|v| v.len() != 2) {
        return Err(Outcome::custom("Invalid location broadcast."));
    }
    if near {
        payload.extend([Value::Int(p.solar_system_id), target]);
    } else {
        payload.push(target);
    }
    let members = w
        .gangs
        .get(id)
        .ok_or_else(|| Outcome::custom("That fleet no longer exists."))?
        .members
        .iter()
        .copied()
        .filter(|c| {
            scope == BroadcastScope::Fleet || (w.presence_of(*c)
                .is_some_and(|p2| !p2.docked && p2.solar_system_id == p.solar_system_id)
                && (scope != BroadcastScope::Bubble || sender_position.zip(position(*c))
                    .is_some_and(|(a,b)| (a-b).len_sq() <= super::space::GRID_RANGE.powi(2))))
        })
        .collect::<Vec<_>>();
    gang::tell(w, &members, event, payload);
    Ok(Value::None)
}

/// Positions used by the watch list and the tactical fleet view. Out-of-system
/// and docked members have no ballpark position and are represented as None.
pub fn location_info(w: &WorldState, me: i64) -> Value {
    let Some(id) = w.gangs.of(me) else {
        return Value::Dict(Vec::new());
    };
    let system = w
        .presence_of(me)
        .filter(|p| !p.docked)
        .map(|p| p.solar_system_id);
    let park = system.and_then(|s| w.parks.get(&s));
    Value::Dict(
        w.gangs
            .get(id)
            .unwrap()
            .members
            .iter()
            .map(|who| {
                let ship = park.and_then(|p| {
                    p.ships
                        .values()
                        .find(|s| s.pilot.char_id == *who && s.drone.is_none())
                });
                let info = ship.map_or(Value::None, |s| {
                    key_val(vec![
                        (
                            "pos",
                            Value::Tuple(vec![
                                Value::Double(s.pos.x),
                                Value::Double(s.pos.y),
                                Value::Double(s.pos.z),
                            ]),
                        ),
                        ("typeID", Value::Int(s.pilot.type_id)),
                        ("shipID", Value::Int(s.id)),
                        ("solarSystemID", Value::int_or_none(system)),
                        ("nearestBallID", Value::None),
                    ])
                });
                (Value::Int(*who), info)
            })
            .collect(),
    )
}

/// Fleet subscriptions are kept with membership, so disconnects and kicks stop
/// telemetry automatically. Destiny carries damage for ships outside the grid.
pub fn pulse(w: &mut WorldState, now: i64) {
    let due = w
        .gangs
        .gangs
        .iter_mut()
        .filter_map(|(id, g)| {
            let f = g.fleet.as_mut()?;
            if f.next_update > now {
                return None;
            }
            f.next_update = now + 1_000;
            Some((*id, f.observers.clone(), f.watchlists.clone()))
        })
        .collect::<Vec<_>>();
    for (id, observers, watches) in due {
        let members = w.gangs.get(id).unwrap().members.clone();
        for member in members {
            let status = active_status(w, member);
            let f = fleet_mut(w, id).unwrap_or_else(|_| unreachable!());
            if f.last_status.get(&member) != Some(&status) {
                f.last_status.insert(member, status.clone());
                if let Value::Instance { state, .. } = status {
                    gang::tell(
                        w,
                        &[member],
                        "OnFleetActive",
                        vec![state
                            .get_str("fleet")
                            .cloned()
                            .unwrap_or(Value::Bool(false))],
                    );
                    for (field, event) in [("wings", "OnWingActive"), ("squads", "OnSquadActive")] {
                        if let Some(Value::Dict(groups)) = state.get_str(field) {
                            for (id, active) in groups {
                                gang::tell(w, &[member], event, vec![id.clone(), active.clone()]);
                            }
                        }
                    }
                }
            }
        }
        for observer in observers {
            gang::tell(
                w,
                &[observer],
                "OnGangLocationUpdate",
                vec![location_info(w, observer)],
            );
        }
        for (observer, watched) in watches {
            let Some(p) = w.presence_of(observer).filter(|p| !p.docked) else {
                continue;
            };
            let Some(park) = w.parks.get(&p.solar_system_id) else {
                continue;
            };
            let entries = park
                .ships
                .values()
                .filter(|s| s.drone.is_none() && watched.contains(&s.pilot.char_id))
                .filter_map(|s| {
                    let fight = s.fight.as_ref()?;
                    Some(super::space::entry(
                        park.tick,
                        "OnGangDamageStateChange",
                        vec![
                            Value::Int(s.id),
                            crate::fight::damage_state(&fight.vitals, now),
                        ],
                    ))
                })
                .collect::<Vec<_>>();
            if !entries.is_empty() {
                let (event,args)=crate::svc::space::destiny_notification(entries);
                w.notify(&w.clients_of(observer),event,args);
            }
        }
    }
}

pub fn regroup(w: &mut WorldState, me: i64) -> Refused<Value> {
    let id = w
        .gangs
        .of(me)
        .ok_or_else(|| Outcome::custom("You are not in a fleet."))?;
    let f = fleet(w, id)?;
    let commander = f.members.get(&me).unwrap();
    if commander.role == MEMBER && commander.job & 2 == 0 {
        return Err(Outcome::custom("Only a commander can regroup pilots."));
    }
    let members = f
        .members
        .iter()
        .filter(|(id, m)| {
            **id != me
                && (f.commander(me)
                    || (commander.role == WING && m.wing == commander.wing)
                    || (commander.role == SQUAD && m.squad == commander.squad))
        })
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();
    let p = w
        .presence_of(me)
        .filter(|p| !p.docked)
        .ok_or_else(|| Outcome::custom("Undock before regrouping."))?;
    let park = w
        .parks
        .get(&p.solar_system_id)
        .ok_or_else(|| Outcome::custom("Your ship is not in space."))?;
    let leader = park
        .ships
        .values()
        .find(|s| s.pilot.char_id == me && s.drone.is_none())
        .ok_or_else(|| Outcome::custom("Your ship is not in space."))?;
    let target = leader.id;
    let nearby = park
        .ships
        .values()
        .filter(|s| {
            members.contains(&s.pilot.char_id)
                && s.drone.is_none()
                && (s.pos - leader.pos).len() <= super::space::GRID_RANGE
        })
        .map(|s| s.id)
        .collect::<Vec<_>>();
    for ship in nearby {
        super::space::order(
            w,
            p.solar_system_id,
            ship,
            crate::ballpark::Command::Follow {
                target,
                range: 1_000.0,
            },
        );
    }
    Ok(Value::None)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(method: &str, args: Vec<Value>) -> Call {
        Call {
            service: "gangSvcObjectHandler".into(),
            method: method.into(),
            args,
            kwargs: vec![],
        }
    }
    fn add(f: &mut Fleet, id: i64) {
        let m = f.default_placement(id).unwrap_or_else(|_| panic!("room"));
        f.members.insert(id, m);
    }

    fn presence(id: i64, docked: bool) -> crate::world::Presence {
        crate::world::Presence {
            char_id: id,
            corp_id: 1000168,
            solar_system_id: 30004971,
            docked,
            station_id: docked.then_some(60014689),
            role: 0,
        }
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE pointing at a disposable imported database"]
    async fn command_skills_enable_squad_bonuses_only_while_commander_is_in_space() {
        let db = crate::test_support::Database::load().await;
        let commander = db.id().await;
        let member = db.id().await;
        for (skill, level) in [(3348i32, 1i64), (3350, 4)] {
            let item = db.id().await;
            db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton)
                VALUES ($1,$2,$3,$3,7,1,1)"#, &[&item, &skill, &commander]).await.unwrap();
            db.db.execute(r#"INSERT INTO entity_attributes ("itemID","attributeID","valueInt") VALUES ($1,280,$2)"#, &[&item, &level]).await.unwrap();
        }
        let (commander, member) = (i64::from(commander), i64::from(member));
        let (mut commander_rx, _member_rx) = {
            let mut w = db.world.lock();
            let c = w.connect(1);
            let m = w.connect(2);
            w.set_presence(1, Some(presence(commander, false)));
            w.set_presence(2, Some(presence(member, false)));
            assert!(object_call(&call("CreateGang", vec![]), &mut w, commander, 1).is_ok());
            let id = w.gangs.of(commander).unwrap();
            let f = fleet_mut(&mut w, id).unwrap_or_else(|_| panic!("fleet"));
            f.members.insert(
                commander,
                Member {
                    wing: 1,
                    squad: 2,
                    role: SQUAD,
                    job: 2,
                    booster: 3,
                },
            );
            add(f, member);
            w.gangs.gangs.get_mut(&id).unwrap().members.push(member);
            (c, m)
        };
        refresh_skills(&db.world, &db.db).await.unwrap();
        let ship = db.id().await;
        db.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",flag,quantity,singleton)
            VALUES ($1,606,$2,30004971,0,1,1)"#, &[&ship, &(member as i32)]).await.unwrap();
        let base = crate::dogma::Loadout::load(&db.db,&db.dogma,member,Some(ship as i64),&Default::default()).await.unwrap();
        let shield = base.value(base.ship().unwrap(),263);
        assert!(shield > 0.0);
        let profile = super::super::inflight::profile(&db.db,&db.dogma,member,ship as i64,&Default::default(),crate::combat::now_ms()).await.unwrap().unwrap();
        let mut live = crate::ballpark::Ship::new(ship as i64,2,profile.stats,profile.pilot,Default::default(),Default::default());
        live.fight = Some(profile.fighter);
        let mut park = crate::ballpark::Park::new(30004971,vec![],0);
        park.ships.insert(ship as i64,live);
        db.world.lock().parks.insert(30004971,park);
        // A failed query must leave the previous catalog state available so
        // the following heartbeat retries, instead of stranding a live ship.
        let unavailable = crate::db::connect(&db.url).await.unwrap();
        unavailable.batch_execute("SET search_path TO pg_catalog").await.unwrap();
        update_bonuses(&db.world,&unavailable,&db.dogma).await;
        assert!(db.dogma.fleet_bonuses(member).is_empty());
        update_bonuses(&db.world,&db.db,&db.dogma).await;
        let boosted = crate::dogma::Loadout::load(&db.db,&db.dogma,member,Some(ship as i64),&Default::default()).await.unwrap();
        assert!((boosted.value(boosted.ship().unwrap(),263) - shield * 1.08).abs() < 0.0001);
        assert!((db.world.lock().parks[&30004971].ships[&(ship as i64)].fight.as_ref().unwrap().vitals.shield.max - shield * 1.08).abs() < 0.0001);
        {
            let mut w = db.world.lock();
            assert_eq!(bonuses(&w, member).get(&263), Some(&8.0));
            assert_eq!(bonuses(&w, member).get(&564), Some(&2.0));
            pulse(&mut w, 1_000);
            w.set_presence(1, Some(presence(commander, true)));
            assert!(bonuses(&w, member).is_empty());
            pulse(&mut w, 2_000);
        }
        update_bonuses(&db.world,&db.db,&db.dogma).await;
        let restored = crate::dogma::Loadout::load(&db.db,&db.dogma,member,Some(ship as i64),&Default::default()).await.unwrap();
        assert!((restored.value(restored.ship().unwrap(),263) - shield).abs() < 0.0001);
        assert!((db.world.lock().parks[&30004971].ships[&(ship as i64)].fight.as_ref().unwrap().vitals.shield.max - shield).abs() < 0.0001);
        let mut squad_updates = Vec::new();
        while let Ok(push) = commander_rx.try_recv() {
            if let crate::world::Push::Notify(n) = push {
                if n.event == "OnSquadActive" {
                    squad_updates.push(n.args);
                }
            }
        }
        assert_eq!(
            squad_updates,
            vec![
                Value::Tuple(vec![Value::Int(2), Value::Bool(true)]),
                Value::Tuple(vec![Value::Int(2), Value::Bool(false)])
            ]
        );
    }

    #[test]
    fn group_capacity_disables_bonuses_without_preventing_membership() {
        let mut w = WorldState::default();
        let _connections = (1..=4)
            .map(|id| {
                let rx = w.connect(id);
                w.set_presence(id, Some(presence(id, false)));
                rx
            })
            .collect::<Vec<_>>();
        let id = w.gangs.form(1);
        let mut f = Fleet::new(1);
        for member in 2..=4 {
            add(&mut f, member);
        }
        f.members.get_mut(&2).unwrap().role = SQUAD;
        f.members.get_mut(&2).unwrap().booster = 3;
        f.skills.insert(2, [(3348, 1), (3350, 5)].into());
        w.gangs.gangs.get_mut(&id).unwrap().members = vec![1, 2, 3, 4];
        w.gangs.gangs.get_mut(&id).unwrap().fleet = Some(f);
        assert!(
            bonuses(&w, 3).is_empty(),
            "Leadership I cannot command three squad members"
        );
        fleet_mut(&mut w, id)
            .unwrap_or_else(|_| panic!("fleet"))
            .skills
            .get_mut(&2)
            .unwrap()
            .insert(3348, 2);
        assert_eq!(bonuses(&w, 3).get(&263), Some(&10.0));
    }

    #[test]
    fn squads_enforce_capacity_and_command_positions() {
        let mut f = Fleet::new(1);
        for id in 2..12 {
            add(&mut f, id);
        }
        assert!(f.default_placement(12).is_err());
        assert!(f.placement(2, 1, 2, SQUAD).is_ok());
        f.members.get_mut(&2).unwrap().role = SQUAD;
        assert!(f.placement(3, 1, 2, SQUAD).is_err());
        assert!(f.placement(2, 99, 2, MEMBER).is_err());
    }

    #[test]
    fn invitations_reserve_positions_and_require_acceptance() {
        let mut w = WorldState::default();
        assert!(object_call(&call("CreateGang", vec![]), &mut w, 1, 1).is_ok());
        let id = w.gangs.of(1).unwrap();
        let bound = |method: &str| Call {
            service: bound::object_id(1, "gangSvcObjectHandler", &[id]),
            ..call(method, vec![])
        };
        assert!(object_call(&bound("AcceptInvite"), &mut w, 2, 1).is_err());
        let f = fleet_mut(&mut w, id).unwrap_or_else(|_| panic!("fleet"));
        let m = f.default_placement(2).unwrap_or_else(|_| panic!("room"));
        f.invitations.insert(2, (1, m));
        w.gangs.gangs.get_mut(&id).unwrap().invited.push(2);
        assert!(object_call(&bound("AcceptInvite"), &mut w, 2, 1).is_ok());
        assert_eq!(w.gangs.of(2), Some(id));
        assert!(object_call(&bound("CreateWing"), &mut w, 2, 1).is_err());
        assert!(object_call(&bound("LeaveGang"), &mut w, 2, 1).is_ok());
        assert!(fleet(&w, id)
            .unwrap_or_else(|_| panic!("fleet"))
            .members
            .get(&2)
            .is_none());
    }

    #[test]
    fn hired_bots_really_join_and_leave_fleets() {
        let mut w = WorldState::default();
        assert!(object_call(&call("CreateGang", vec![]), &mut w, 1, 1).is_ok());
        let id = w.gangs.of(1).unwrap();
        w.gangs.gangs.get_mut(&id).unwrap().invited.push(2);
        gang::bot_joins(&mut w, id, 2);
        assert_eq!(w.gangs.of(2), Some(id));
        assert!(fleet(&w, id)
            .unwrap_or_else(|_| panic!("fleet"))
            .members
            .contains_key(&2));
        gang::bot_leaves(&mut w, 2);
        assert_eq!(w.gangs.of(2), None);
    }

    #[test]
    fn commanders_manage_only_their_own_formations() {
        let mut w = WorldState::default();
        let id = w.gangs.form(1);
        let mut f = Fleet::new(1);
        f.wings.insert(3, Wing {name:"Other".into(),squads:[(4,"Other".into())].into()});
        f.next_id = 4;
        f.members.insert(2,Member {wing:1,squad:-1,role:WING,job:0,booster:2});
        f.members.insert(3,Member {wing:1,squad:2,role:SQUAD,job:0,booster:3});
        add(&mut f,4);
        f.members.insert(5,Member {wing:3,squad:4,role:MEMBER,job:0,booster:0});
        assert_eq!(f.warp_members(1).unwrap_or_else(|_|panic!("fleet commander")),vec![1,2,3,4,5]);
        assert_eq!(f.warp_members(2).unwrap_or_else(|_|panic!("wing commander")),vec![2,3,4]);
        assert_eq!(f.warp_members(3).unwrap_or_else(|_|panic!("squad commander")),vec![3,4]);
        assert!(f.warp_members(4).is_err());
        // Fleet creator status does not promote a regular member to command.
        f.members.get_mut(&4).unwrap().job=2;
        assert!(f.warp_members(4).is_err());
        f.members.get_mut(&4).unwrap().job=0;
        w.gangs.gangs.get_mut(&id).unwrap().members = vec![1,2,3,4,5];
        w.gangs.gangs.get_mut(&id).unwrap().fleet = Some(f);
        let action = |method, args, w:&mut WorldState, me| object_call(&call(method,args),w,me,1);
        assert!(action("CreateSquad",vec![Value::Int(1)],&mut w,2).is_ok());
        assert!(action("CreateSquad",vec![Value::Int(3)],&mut w,2).is_err());
        assert!(action("ChangeSquadName",vec![Value::Int(2),Value::text("Alpha")],&mut w,3).is_ok());
        assert!(action("ChangeSquadName",vec![Value::Int(4),Value::text("No")],&mut w,3).is_err());
        assert!(action("MoveMember",vec![Value::Int(4),Value::Int(3),Value::Int(4),Value::Int(MEMBER)],&mut w,2).is_err());
        assert!(action("MoveMember",vec![Value::Int(4),Value::Int(-1),Value::Int(-1),Value::Int(LEADER)],&mut w,2).is_err());
        assert!(action("SetBooster",vec![Value::Int(4),Value::Int(1)],&mut w,3).is_err());
        assert!(action("SetBooster",vec![Value::Int(4),Value::Int(3)],&mut w,3).is_ok());
        let f = fleet_mut(&mut w,id).unwrap_or_else(|_|panic!("fleet"));
        f.free_move = true;
        // Moving oneself with free-move may relinquish a squad booster role,
        // but cannot appoint oneself fleet booster or displace another one.
        assert!(action("MoveMember",vec![Value::Int(4),Value::Int(1),Value::Int(2),Value::Int(MEMBER),Value::Int(1)],&mut w,4).is_err());
        assert!(action("MoveMember",vec![Value::Int(4),Value::Int(1),Value::Int(5),Value::Int(MEMBER),Value::None],&mut w,4).is_ok());
        assert_eq!(fleet(&w,id).unwrap_or_else(|_|panic!("fleet")).members[&4].booster,0);
        assert!(action("KickMember",vec![Value::Int(2)],&mut w,3).is_err());
        assert!(action("KickMember",vec![Value::Int(5)],&mut w,2).is_err());
        assert!(action("KickMember",vec![Value::Int(4)],&mut w,2).is_ok());
    }

    #[test]
    fn bubble_broadcasts_stay_on_grid_and_malformed_requests_are_rejected() {
        use crate::ballpark::{Park,Pilot,Ship,ShipStats,Vec3};
        let mut w = WorldState::default();
        let mut receivers = (1..=4).map(|id| {
            let rx = w.connect(id);
            w.set_presence(id,Some(presence(id,id==4)));
            rx
        }).collect::<Vec<_>>();
        let id = w.gangs.form(1);
        let mut f = Fleet::new(1);
        for who in 2..=4 { add(&mut f,who); }
        w.gangs.gangs.get_mut(&id).unwrap().members = vec![1,2,3,4];
        w.gangs.gangs.get_mut(&id).unwrap().fleet = Some(f);
        let mut park = Park::new(30004971,vec![],0);
        for (who,x) in [(1,0.0),(2,200_000.0),(3,1_000_000.0)] {
            let pilot = Pilot {type_id:606,group_id:25,name:who.to_string(),char_id:who,corp_id:1000168,alliance_id:None,security:0.0,bounty:0.0};
            let ship = Ship::new(who+100,who,ShipStats::new(10.0,1e6,1.0,100.0,1.0),pilot,Vec3::new(x,0.0,0.0),Vec3::ZERO);
            park.ships.insert(ship.id,ship);
        }
        w.parks.insert(30004971,park);
        assert!(relay_broadcast(&call("BroadcastToBubble",vec![]),&w,1).is_err());
        assert!(relay_broadcast(&call("BroadcastToBubble",vec![Value::text("Target")]),&w,1).is_err());
        assert!(relay_broadcast(&call("BroadcastToBubble",vec![Value::text("Target"),Value::Int(102)]),&w,1).is_ok());
        for (index,rx) in receivers.iter_mut().enumerate() { assert_eq!(rx.try_recv().is_ok(),index<2); }
        assert!(relay_broadcast(&call("BroadcastToSystem",vec![Value::text("WarpTo"),Value::Int(102)]),&w,1).is_ok());
        for (index,rx) in receivers.iter_mut().enumerate() { assert_eq!(rx.try_recv().is_ok(),index<3); }
        assert!(relay_broadcast(&call("BroadcastToBubble",vec![Value::text("Target"),Value::Int(102)]),&w,4).is_err());
    }
}
