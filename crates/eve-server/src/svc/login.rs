//! The calls a client makes between login and entering the game: cached bulk
//! objects, authentication housekeeping, and the character selection screen.
//! (`character::SelectCharacterID` itself changes the session, so `conn` serves it.)

use eve_marshal::Value;
use tracing::info;

use crate::services::{Call, Ctx, Outcome};
use crate::store::{self, Db, Row, StoreError};

pub async fn object_caching(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    if call.method != "GetCachableObject" {
        return None;
    }
    let object_id = call.arg_text(1).unwrap_or_default();
    Some(match ctx.bulk.entries.get(&object_id) {
        Some(entry) => Outcome::Value(ctx.bulk.payload(entry)),
        None => Outcome::Unknown(format!("no cached object {object_id:?}")),
    })
}

pub async fn authentication(call: &Call, _ctx: &Ctx<'_>) -> Option<Outcome> {
    let value = match call.method.as_str() {
        "Ping" => Value::Long(crate::bulkdata::filetime_now()),
        "GetPostAuthenticationMessage" => Value::None,
        // The connection handles this call to update both the durable account
        // preference and the session. Never report success without that path.
        "SetLanguageID" => return Some(Outcome::custom("A language change requires an authenticated connection.")),
        // Anti-addiction play-time limits (antiaddiction.py) apply only to
        // the Chinese region; answered for completeness.
        "AmUnderage" => Value::Bool(false),
        "AccruedTime" => Value::Int(0),
        _ => return None,
    };
    Some(Outcome::Value(value))
}

pub async fn character(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let char_id = call.arg_id(0);
    let value = match call.method.as_str() {
        "GetCharactersToSelect" => characters_to_select(ctx).await,
        "GetCharacterToSelect" => character_to_select(ctx, char_id).await,
        "PrepareCharacterForDelete" => prepare_for_delete(ctx, char_id).await,
        "CancelCharacterDeletePrepare" => {
            let account = account(ctx);
            let result = ctx.store.write(|tx| {
                if let Some(id) = own_character(tx.db(), char_id, account).map(|r| r.id) {
                    tx.update("character_", id, &[("deletePrepareDateTime", store::Value::Null)])?;
                }
                Ok::<_, StoreError>(())
            });
            if let Err(e) = result {
                tracing::error!("statement failed: {}", crate::logging::chain(&e));
            }
            Value::None
        }
        "DeleteCharacter" => delete_character(ctx, char_id).await,
        _ => return None,
    };
    Some(Outcome::Value(value))
}

const FILETIME_PER_SECOND: i64 = 10_000_000;

/// How long a character waits in the biomass queue before it can be deleted.
/// The client only displays it; the length is the server's choice.
const BIOMASS_DELAY: i64 = 60 * 60 * FILETIME_PER_SECOND;

fn account(ctx: &Ctx<'_>) -> i64 {
    ctx.session.user_id.unwrap_or(0)
}

/// The character `char_id` if it is on `account`.
fn own_character(db: &Db, char_id: i64, account: i64) -> Option<Row<'_>> {
    db.table("character_").get_id(char_id).filter(|c| c.int("accountID") == Some(account))
}

async fn characters_to_select(ctx: &Ctx<'_>) -> Value {
    const HEADER: &[&str] = &[
        "characterID", "characterName", "deletePrepareDateTime", "gender", "accessoryID",
        "beardID", "costumeID", "decoID", "eyebrowsID", "eyesID", "hairID", "lipstickID",
        "makeupID", "skinID", "backgroundID", "lightID", "headRotation1", "headRotation2",
        "headRotation3", "eyeRotation1", "eyeRotation2", "eyeRotation3", "camPos1", "camPos2",
        "camPos3", "morph1e", "morph1n", "morph1s", "morph1w", "morph2e", "morph2n", "morph2s",
        "morph2w", "morph3e", "morph3n", "morph3s", "morph3w", "morph4e", "morph4n", "morph4s",
        "morph4w",
    ];
    let account = account(ctx);
    let lines = ctx.store.read(|db| {
        let mut characters: Vec<Row> = db.table("character_").find(&["accountID"], &[store::Value::Int(account)]).collect();
        characters.sort_by_key(|c| c.int("characterID"));
        characters.iter().map(|c| Value::List(c.line(HEADER))).collect()
    });
    crate::svc::rows::rowset(HEADER, lines)
}

async fn character_to_select(ctx: &Ctx<'_>, char_id: i64) -> Value {
    const HEADER: &[&str] = &[
        "characterID", "petitionMessage", "gender", "bloodlineID", "stationID", "solarSystemID",
        "constellationID", "regionID", "startDateTime", "corporationID", "bounty",
        "securityRating", "title", "balance",
        // Trinity's charsel.AddCharacter also reads the character's age and
        // its corporation's alliance (ticker and join date).
        "createDateTime", "allianceID", "shortName", "startDate",
    ];
    /// The columns read from the character's own row.
    const OWN: usize = 15;
    let account = account(ctx);
    let lines = ctx.store.read(|db| {
        let Some(c) = own_character(db, char_id, account) else { return Vec::new() };
        let mut line = c.line(&HEADER[..OWN]);
        let corp = db.table("corporation").get(std::slice::from_ref(c.value("corporationID")));
        let alliance = corp.map_or(store::Value::Null, |co| co.value("allianceID").clone());
        // NULLIF(allianceID, 0): a corporation outside any alliance has none.
        line.push(match corp {
            Some(co) if co.int("allianceID") != Some(0) => co.cell("allianceID"),
            _ => Value::None,
        });
        let short = db.table("alliance_shortnames").find_one(&["allianceID"], std::slice::from_ref(&alliance));
        line.push(short.map_or(Value::None, |s| s.cell("shortName")));
        let member = db
            .table("allianceMembers")
            .find_one(&["corporationID"], std::slice::from_ref(c.value("corporationID")))
            .filter(|m| !alliance.is_null() && m.value("allianceID") == &alliance);
        line.push(member.map_or(Value::None, |m| m.cell("startDate")));
        vec![Value::List(line)]
    });
    crate::svc::rows::rowset(HEADER, lines)
}

/// Queues the character for deletion and returns when that becomes possible
/// (charsel.Terminate shows the wait), or None if it is not the caller's.
async fn prepare_for_delete(ctx: &Ctx<'_>, char_id: i64) -> Value {
    let when = crate::bulkdata::filetime_now() + BIOMASS_DELAY;
    let account = account(ctx);
    let queued = ctx.store.write(|tx| {
        let Some(id) = own_character(tx.db(), char_id, account).map(|r| r.id) else { return Ok(false) };
        tx.update("character_", id, &[("deletePrepareDateTime", store::Value::Int(when))])?;
        Ok::<_, StoreError>(true)
    });
    match queued {
        Ok(true) => {
            info!("character {char_id} queued for deletion");
            Value::Long(when)
        }
        Ok(false) => Value::None,
        Err(e) => {
            tracing::error!("query failed: {}", crate::logging::chain(&e));
            Value::None
        }
    }
}

/// Deletes a queued character whose wait is over, with everything that belongs
/// only to it, in one transaction. Shared history (kill logs, the market journal,
/// votes) stays. Returns None, or a (message, args) error the client shows.
async fn delete_character(ctx: &Ctx<'_>, char_id: i64) -> Value {
    match delete_character_checked(ctx,char_id).await {
        Ok(())=>Value::None,
        Err(reason)=>Value::Tuple(vec![Value::bytes("CustomError"),Value::Dict(vec![(Value::bytes("error"),Value::text(reason))])]),
    }
}

/// Whether the character still exists. Contract creation and acceptance,
/// biomass and the other changes to a character's lifecycle check it in the
/// transaction that makes them, so none acts on a character another removed.
pub(crate) fn lock_character_lifecycle(db:&Db,char_id:i64)->Result<bool,StoreError> {
    Ok(db.table("character_").get_id(char_id).is_some())
}

/// Why a deletion did not happen: a refusal the player sees, or a failed
/// change.
enum Refusal {
    Refused(String),
    Db(StoreError),
}

impl From<StoreError> for Refusal {
    fn from(e:StoreError)->Self {Refusal::Db(e)}
}

/// Whether the character has a contract obligation biomass would orphan: an
/// open contract it issued, goods it borrowed or carries, or a winning bid.
fn contract_obligation(db:&Db,char_id:i64)->bool {
    let bids=db.table("contract_bids");
    let items=db.table("contract_items");
    let entity=db.table("entity");
    db.table("contracts").rows().filter(|c|c.bool("deleted")==Some(false)).any(|c| {
        let kind=c.int("type").unwrap_or(0);
        let status=c.int("status").unwrap_or(0);
        let contract=std::slice::from_ref(c.value("contractID"));
        (c.bool("forCorp")==Some(false) && c.int("issuerID")==Some(char_id) && matches!(status,0|1|3))
        || (c.bool("acceptorCorp")==Some(false) && c.int("acceptorID")==Some(char_id) && matches!(kind,3|4|5) && matches!(status,1|3))
        || (kind==2 && matches!(status,0|2) && bids.find(&["contractID"],contract).max_by_key(|b|b.int("id"))
            .is_some_and(|b|b.int("bidderID")==Some(char_id) && b.bool("forCorp")==Some(false)))
        || (matches!(kind,3|4) && matches!(status,1|3) && items.find(&["contractID"],contract)
            .any(|ci|ci.bool("inCrate")==Some(true) && ci.int("itemID").and_then(|i|entity.get_id(i)).is_some_and(|e|e.int("ownerID")==Some(char_id))))
    })
}

/// The tables whose rows belong only to a character, by the column naming it.
const OWN_ROWS:&[(&str,&str)]=&[
    ("eveStaticOwners","ownerID"),("eveNames","itemID"),("bookmarks","ownerID"),("chrNotes","characterID"),
    ("chrOwnerNote","ownerID"),("chrStandings","characterID"),("chrNPCStandings","characterID"),
    ("chrCorporationRoles","characterID"),("chrEmployment","characterID"),("chrMissionState","characterID"),
    ("chrOffers","characterID"),("chrAgentLoyalty","characterID"),("chrAgentResearch","characterID"),
    ("crpCharShares","characterID"),("crpApplications","characterID"),("chrJumpClones","ownerID"),
    ("chrLoyaltyPoints","characterID"),("chrSystemVisits","characterID"),("chrTutorialState","characterID"),
    ("insuranceContracts","ownerID"),("channelChars","charID"),
];

async fn delete_character_checked(ctx:&Ctx<'_>,char_id:i64)->Result<(),String> {
    let invalid="That character is not on this account or is not ready for deletion.";
    if char_id<=0 || char_id>i32::MAX as i64 {return Err(invalid.into());}
    let failed=|e:StoreError| {tracing::error!("deleting character {char_id}: {}",crate::logging::chain(&e));"The character could not be deleted.".to_string()};
    let refuse=|reason:&str|Err(Refusal::Refused(reason.into()));
    let account=account(ctx);
    let deleted=ctx.store.write(|tx| {
        if !lock_character_lifecycle(tx.db(),char_id)? {return refuse(invalid);}
        let now=crate::bulkdata::filetime_now();
        let eligible=own_character(tx.db(),char_id,account).filter(|c|c.int("deletePrepareDateTime").is_some_and(|t|t<=now));
        let Some(corp)=eligible.map(|c|c.int("corporationID")) else {return refuse(invalid);};
        // This account-level RPC has no selected character for the connection's
        // arrival guard. Preserve the durable final-snapshot/loss obligation until
        // recovery resolves it; deleting its hull would cascade away the marker.
        // A derelict loss does not block normal play, but biomass would also erase
        // that unoccupied hull and its outstanding loss/insurance obligation.
        let unsettled_owned_loss=ctx.world.lock().pending_ship_losses.values().any(|loss|loss.char_id==char_id);
        if unsettled_owned_loss || crate::travel::blocked_character_in_memory(ctx.world,char_id)
            || crate::svc::jumps::portal::blocked_character_in_memory(ctx.world,char_id)
            || crate::conn::logout::blocked_character_in_memory(ctx.world,char_id)
            || tx.table("pendingShipLosses").find(&["characterID"],&[store::Value::Int(char_id)]).next().is_some()
            || crate::losses::pending::blocked_character(tx.db(),char_id)? {
            return refuse("This character's station arrival or ship loss must finish before deletion.");
        }
        {
            let world=ctx.world.lock();
            if !world.clients_of(char_id).is_empty() {return refuse("Log out of that character before deleting it.");}
            if world.retained_logouts.values().any(|(pilot,_)|*pilot==char_id) {return refuse("Your ship's active cycle or pending travel must finish before deleting this character.");}
            if world.trades.values().any(|trade|trade.traders.contains(&char_id)) {return refuse("Finish or cancel that character's trade before deleting it.");}
        }
        if tx.table("corporation").find(&["ceoID"],&[store::Value::Int(char_id)]).next().is_some() {
            return refuse("Appoint another corporation CEO before deleting this character.");
        }
        if contract_obligation(tx.db(),char_id) {
            return refuse("Settle or cancel this character's outstanding contracts before deleting it.");
        }
        let me=store::Value::Int(char_id);
        // Its items, and the character's own item row, with their attributes.
        let entity=tx.table("entity");
        let mut items:Vec<(store::RowId,i64)>=entity.find(&["ownerID"],std::slice::from_ref(&me)).filter_map(|e|Some((e.id,e.int("itemID")?))).collect();
        if let Some(e)=entity.get_id(char_id).filter(|e|e.int("ownerID")!=Some(char_id)) {items.push((e.id,char_id));}
        for (row,item) in &items {
            tx.delete_where("entity_attributes",&["itemID"],&[store::Value::Int(*item)])?;
            tx.delete("entity",*row)?;
        }
        for (table,column) in OWN_ROWS {
            tx.delete_where(table,&[*column],std::slice::from_ref(&me))?;
        }
        // Orders it placed for itself; those for its corporation's wallet stay.
        let orders:Vec<store::RowId>=tx.table("market_orders").find(&["charID"],std::slice::from_ref(&me))
            .filter(|o|o.int("walletOwnerID").is_none_or(|w|w==char_id)).map(|o|o.id).collect();
        for id in orders {tx.delete("market_orders",id)?;}
        if let Some(c)=corp.and_then(|corp|tx.table("corporation").get_id(corp)) {
            let members=(c.int("memberCount").unwrap_or(0)-1).max(0);
            tx.update("corporation",c.id,&[("memberCount",store::Value::Int(members))])?;
        }
        let Some(row)=tx.table("character_").get_id(char_id).map(|c|c.id) else {return refuse(invalid);};
        tx.delete("character_",row)?;
        Ok(items.into_iter().map(|(_,item)|item).collect::<std::collections::HashSet<i64>>())
    });
    let removed=match deleted {
        Ok(removed)=>removed,
        Err(Refusal::Refused(reason))=>return Err(reason),
        Err(Refusal::Db(e))=>return Err(failed(e)),
    };
    // Only the hulls the deletion actually removed may lose their live balls:
    // a pre-delete owner snapshot could otherwise erase a ship that another
    // pilot had acquired first.
    let mut world=ctx.world.lock();
    let empty_hulls:Vec<_>=world.parks.iter().flat_map(|(system,park)|park.ships.values()
        .filter(|ship|ship.unpiloted && ship.pilot.char_id==char_id && removed.contains(&ship.id))
        .map(|ship|(*system,ship.id))).collect();
    for (system,ship) in empty_hulls { crate::svc::space::remove_ship(&mut world,system,ship); }
    info!("character {char_id} deleted");
    Ok(())
}

#[cfg(test)]
mod deletion_tests {
    use super::*;
    use crate::store::{Store,Value as V};
    use crate::{session::Session,test_support::Database};

    fn write(store:&Store,f:impl FnOnce(&mut crate::store::Tx)->Result<(),StoreError>) {store.write(f).unwrap()}
    fn exists(store:&Store,table:&str,id:i64)->bool {store.read(|db|db.table(table).get_id(id).is_some())}
    fn set(store:&Store,table:&str,id:i64,sets:&[(&str,V)]) {
        write(store,|tx|{let row=tx.table(table).get_id(id).unwrap().id;tx.update(table,row,sets)})
    }

    async fn pilot(db:&Database,account:i64)->Session {
        let id=db.id().await as i64;
        write(&db.store,|tx|{tx.insert("character_",&[("characterID",V::Int(id)),("accountID",V::Int(account)),("characterName",V::Text(format!("Deletion fixture {id}").into())),
            ("balance",V::Float(10_000_000.0)),("deletePrepareDateTime",V::Int(1))])?;Ok(())});
        Session {char_id:Some(id),user_id:Some(account),station_id:Some(60000004),..Default::default()}
    }
    fn contract(db:&Database,issuer:i64,kind:i64,status:i64,acceptor:i64)->i64 {
        let now=crate::bulkdata::filetime_now();
        db.store.write(|tx|{
            let row=tx.insert("contracts",&[("type",V::Int(kind)),("issuerID",V::Int(issuer)),("issuerCorpID",V::Int(1000002)),("forCorp",V::Bool(false)),
                ("availability",V::Int(0)),("assigneeID",V::Int(0)),("acceptorID",V::Int(acceptor)),("dateIssued",V::Int(now)),("dateExpired",V::Int(now+864_000_000_000)),
                ("numDays",V::Int(1)),("startStationID",V::Int(60000004)),("startSolarSystemID",V::Int(30000142)),("startRegionID",V::Int(10000002)),
                ("endStationID",V::Int(0)),("endSolarSystemID",V::Int(0)),("endRegionID",V::Int(0)),("price",V::Float(0.0)),("reward",V::Float(0.0)),
                ("collateral",V::Float(0.0)),("title",V::Text("Deletion fixture".into())),("description",V::Text("".into())),
                ("sourceLocation",V::Int(60000004)),("sourceFlag",V::Int(4)),("status",V::Int(status))])?;
            Ok::<_,StoreError>(tx.table("contracts").row(row).unwrap().int("contractID").unwrap())
        }).unwrap()
    }
    fn escrow_item(db:&Database,contract:i64,owner:i64)->i64 {
        db.store.write(|tx|{
            let row=tx.insert("entity",&[("typeID",V::Int(34)),("ownerID",V::Int(owner)),("locationID",V::Int(contract)),("flag",V::Int(0)),("quantity",V::Int(3))])?;
            let item=tx.table("entity").row(row).unwrap().int("itemID").unwrap();
            tx.insert("contract_items",&[("contractID",V::Int(contract)),("itemID",V::Int(item)),("typeID",V::Int(34)),("quantity",V::Int(3)),("inCrate",V::Bool(true))])?;
            Ok::<_,StoreError>(item)
        }).unwrap()
    }
    fn hull(db:&Database,id:i64,owner:i64,location:i64,flag:i64) {
        write(&db.store,|tx|{tx.insert("entity",&[("itemID",V::Int(id)),("typeID",V::Int(587)),("ownerID",V::Int(owner)),("locationID",V::Int(location)),
            ("flag",V::Int(flag)),("quantity",V::Int(1)),("singleton",V::Int(1))])?;Ok(())});
    }
    fn bid(db:&Database,contract:i64,bidder:i64,amount:f64,when:i64) {
        write(&db.store,|tx|{tx.insert("contract_bids",&[("contractID",V::Int(contract)),("bidderID",V::Int(bidder)),("amount",V::Float(amount)),
            ("dateBid",V::Int(when)),("forCorp",V::Bool(false))])?;Ok(())});
    }

    #[tokio::test]
    async fn deletion_preserves_pending_derelict_losses_in_memory_and_after_restart() {
        let db=Database::load().await;let account=db.id().await as i64;
        let target=pilot(&db,account).await;let me=target.char_id.unwrap();
        let hull_id=db.id().await as i64;let system=30004971;
        hull(&db,hull_id,me,system,0);
        let loss=crate::losses::Loss {client_id:0,char_id:me,ship_id:hull_id,ship_type:587,system,
            pos:crate::ballpark::Vec3::ZERO,arrival_custody:None,capsule:false,unpiloted:true,final_blow:None,attackers:vec![],items:vec![]};
        let caller=Session {user_id:Some(account),..Default::default()};let ctx=db.ctx(&caller);
        db.world.lock().pending_ship_losses.insert(hull_id,loss.clone());
        assert!(!crate::losses::journal::blocked_character_in_memory(&db.world,me),"a derelict loss does not immobilize its owner's current ship");
        assert!(delete_character_checked(&ctx,me).await.is_err(),"biomass must still retain its owner's outstanding hull loss");
        assert!(exists(&db.store,"entity",hull_id));
        assert!(db.store.write(|tx|{
            assert!(lock_character_lifecycle(tx.db(),me)?);
            crate::losses::journal::stage(tx,&loss)
        }).unwrap());
        db.world.lock().pending_ship_losses.clear();
        assert!(!db.store.read(|db|crate::losses::pending::blocked_character(db,me)).unwrap(),"normal RPC quarantine remains occupied-only");
        assert!(delete_character_checked(&ctx,me).await.is_err(),"restart cannot erase a durable derelict loss");
        assert_eq!(db.store.read(|db|db.table("pendingShipLosses").get_id(hull_id).unwrap().bool("occupied")),Some(false));
        assert!(exists(&db.store,"character_",me));
        write(&db.store,|tx|tx.delete_where("pendingShipLosses",&["shipID"],&[V::Int(hull_id)]).map(drop));
        delete_character_checked(&ctx,me).await.unwrap();
        assert!(!exists(&db.store,"entity",hull_id));
    }

    // Once committed, an arrival marker refuses biomass, whichever order the
    // two were asked for in: the store runs one write at a time.
    #[tokio::test]
    async fn deletion_preserves_pending_arrival_committed_before_it() {
        let db=Database::load().await;let account=db.id().await as i64;
        let target=pilot(&db,account).await;let me=target.char_id.unwrap();
        let hull_id=db.id().await as i64;let station=60014659;let source=30004971;
        hull(&db,hull_id,me,station,4);
        let caller=Session {user_id:Some(account),..Default::default()};
        let ctx=db.ctx(&caller);
        let marker=db.store.write(|tx|{
            assert!(lock_character_lifecycle(tx.db(),me)?);
            let row=tx.table("character_").get_id(me).unwrap().id;
            tx.update("character_",row,&[("shipID",V::Int(hull_id)),("stationID",V::Int(station))])?;
            crate::losses::pending::begin(tx,me,hull_id,station,source)
        }).unwrap();
        assert!(delete_character_checked(&ctx,me).await.unwrap_err().contains("station arrival"));
        assert!(db.store.read(|db|crate::losses::pending::blocked_character(db,me)).unwrap());
        assert!(exists(&db.store,"entity",hull_id));
        // Both an unknown final snapshot and an unreadable recorded loss stay
        // quarantined; biomass must not choose a survival or death outcome.
        write(&db.store,|tx|{
            let row=tx.table("pendingStationArrivals").find(&["shipID"],&[V::Int(hull_id)]).find(|r|r.int("generation")==Some(marker.generation)).unwrap().id;
            tx.update("pendingStationArrivals",row,&[("loss",V::Text("{}".into()))])
        });
        assert!(delete_character_checked(&ctx,me).await.unwrap_err().contains("station arrival"));
        let foreign=Session {user_id:Some(account+1),..Default::default()};
        assert!(delete_character_checked(&db.ctx(&foreign),me).await.unwrap_err().contains("not on this account"));
        write(&db.store,|tx|tx.delete_where("pendingStationArrivals",&["shipID"],&[V::Int(hull_id)]).map(drop));
        delete_character_checked(&ctx,me).await.unwrap();
        assert!(!exists(&db.store,"character_",me));
        assert!(!exists(&db.store,"entity",hull_id));
    }

    #[tokio::test]
    async fn deletion_removes_empty_hulls_but_keeps_a_hull_claimed_before_it() {
        use crate::ballpark::{Park,Pilot,Ship,ShipStats,Vec3};
        let db=Database::load().await;
        let account=db.id().await as i64;
        let target=pilot(&db,account).await;
        let other=pilot(&db,account).await.char_id.unwrap();
        let owner=target.char_id.unwrap();
        let ghost=db.id().await as i64;
        let claimed=db.id().await as i64;
        let foreign=db.id().await as i64;
        let observer=db.id().await as i64;
        let system=30000142;
        let mut park=Park::new(system,vec![],crate::destiny::tick_now());
        for (id,property_owner,unpiloted) in [(ghost,owner,true),(claimed,owner,true),(foreign,other,true),(observer,other,false)] {
            hull(&db,id,property_owner,system,0);
            let mut ship=Ship::new(id,if unpiloted {0} else {9},ShipStats::new(37.0,1.1e6,3.1,320.0,2.0),
                Pilot{type_id:587,group_id:25,name:"Biomass hull".into(),char_id:property_owner,
                    corp_id:1000002,alliance_id:None,security:0.0,bounty:0.0},Vec3::ZERO,Vec3::ZERO);
            ship.unpiloted=unpiloted;
            park.ships.insert(id,ship);
        }
        let kind=crate::npc::NpcType::with(3634,53,1.0,1.0,&[]);
        let mut fighter=crate::npc::entity_fighter(&kind,10000.0,1,100.0,crate::combat::now_ms());
        fighter.locks.insert(ghost,crate::combat::Lock::Locked);
        fighter.locks.insert(claimed,crate::combat::Lock::Locked);
        park.ships.get_mut(&observer).unwrap().fight=Some(fighter);
        let mut receiver={
            let mut world=db.world.lock();
            let rx=world.connect(9);
            world.set_presence(9,Some(crate::world::Presence{char_id:other,corp_id:1000002,
                solar_system_id:system,docked:false,station_id:None,role:0}));
            world.sees(9,system);
            world.parks.insert(system,park);
            rx
        };
        let caller=Session{user_id:Some(account),..Default::default()};
        let ctx=db.ctx(&caller);
        let wrong=Session{user_id:Some(account+1),..Default::default()};
        assert!(delete_character_checked(&db.ctx(&wrong),owner).await.is_err());
        assert!(db.world.lock().parks[&system].ships.contains_key(&ghost));
        assert!(receiver.try_recv().is_err(),"refused deletion cannot remove live ships");

        // Another pilot claims one of the hulls while the park still shows
        // the old owner.
        set(&db.store,"entity",claimed,&[("ownerID",V::Int(other))]);
        delete_character_checked(&ctx,owner).await.unwrap();
        assert!(!exists(&db.store,"entity",ghost));
        assert_eq!(db.store.read(|db|db.table("entity").get_id(claimed).unwrap().int("ownerID")),Some(other));
        {
            let world=db.world.lock();
            let park=&world.parks[&system];
            assert!(!park.ships.contains_key(&ghost));
            assert!(park.ships.contains_key(&claimed),"stale live ownership must not erase a hull the store retained");
            assert!(park.ships.contains_key(&foreign));
            let locks=&park.ships[&observer].fight.as_ref().unwrap().locks;
            assert!(!locks.contains_key(&ghost),"normal departure bookkeeping clears locks");
            assert!(locks.contains_key(&claimed));
        }
        let mut removed=Vec::new();
        while let Ok(crate::world::Push::Notify(notification))=receiver.try_recv() {
            if notification.event!="DoDestinyUpdate" {continue;}
            for entry in notification.args.seq().unwrap()[0].seq().unwrap() {
                let event=entry.seq().unwrap()[1].seq().unwrap();
                if event[0].as_bytes()==Some(b"RemoveBalls") {
                    removed.extend(event[1].seq().unwrap()[0].seq().unwrap().iter().filter_map(Value::as_i64));
                }
            }
        }
        assert_eq!(removed,vec![ghost],"observers lose only the hull actually deleted");
    }

    #[tokio::test]
    async fn deletion_preserves_ceos_contract_debtors_and_winning_bidders() {
        let db=Database::load().await;let account=db.id().await as i64;let target=pilot(&db,account).await;
        let me=target.char_id.unwrap();let caller=Session {user_id:Some(account),..Default::default()};let ctx=db.ctx(&caller);
        let corp=db.id().await as i64;
        write(&db.store,|tx|{tx.insert("corporation",&[("corporationID",V::Int(corp)),("corporationName",V::Text(format!("CEO fixture {corp}").into())),("ceoID",V::Int(me))])?;Ok(())});
        assert!(!matches!(delete_character(&ctx,me).await,Value::None));
        set(&db.store,"corporation",corp,&[("ceoID",V::Int(0))]);
        let offered=contract(&db,me,1,0,0);
        assert!(!matches!(delete_character(&ctx,me).await,Value::None));
        set(&db.store,"contracts",offered,&[("status",V::Int(4))]);
        let borrowed=contract(&db,1,4,1,me);
        let item=escrow_item(&db,borrowed,me);
        assert!(!matches!(delete_character(&ctx,me).await,Value::None));
        set(&db.store,"contracts",borrowed,&[("acceptorID",V::Int(2))]);
        // Borrowed goods still belong to a live obligation even when passed on.
        assert!(!matches!(delete_character(&ctx,me).await,Value::None));
        assert!(exists(&db.store,"entity",item));
        set(&db.store,"contracts",borrowed,&[("status",V::Int(7))]);
        let auction=contract(&db,1,2,0,0);
        bid(&db,auction,me,1000.0,1);
        assert!(!matches!(delete_character(&ctx,me).await,Value::None));
        set(&db.store,"contracts",auction,&[("status",V::Int(2))]);
        assert!(!matches!(delete_character(&ctx,me).await,Value::None));
        bid(&db,auction,2,2000.0,2);
        // An already-paid auction issuer and a corporation's former acting
        // issuer have no personal obligation left to block deletion.
        contract(&db,me,2,2,2);
        let corp_contract=contract(&db,me,1,0,0);
        set(&db.store,"contracts",corp_contract,&[("forCorp",V::Bool(true)),("issuerCorpID",V::Int(corp))]);
        let foreign=Session {user_id:Some(account+1),..Default::default()};
        assert!(!matches!(delete_character(&db.ctx(&foreign),me).await,Value::None));
        set(&db.store,"character_",me,&[("deletePrepareDateTime",V::Int(crate::bulkdata::filetime_now()+864_000_000_000))]);
        assert!(!matches!(delete_character(&ctx,me).await,Value::None));
        set(&db.store,"character_",me,&[("deletePrepareDateTime",V::Int(1))]);
        assert!(matches!(delete_character(&ctx,me).await,Value::None));
        assert!(!exists(&db.store,"character_",me));
    }

    #[tokio::test]
    async fn deletion_and_zero_value_loan_acceptance_cannot_orphan_goods() {
        let db=Database::load().await;let account=db.id().await as i64;
        let issuer=pilot(&db,account).await;
        for _ in 0..4 {
            let borrower=pilot(&db,account).await;let me=borrower.char_id.unwrap();
            let contract=contract(&db,issuer.char_id.unwrap(),4,0,0);
            let item=escrow_item(&db,contract,1);
            let caller=Session {user_id:Some(account),..Default::default()};
            let deletion=db.ctx(&caller);let acceptance=db.ctx(&borrower);
            let call=Call {service:"contractMgr".into(),method:"AcceptContract".into(),args:vec![Value::Int(contract),Value::Bool(false)],kwargs:vec![]};
            let (deleted,accepted)=tokio::join!(delete_character(&deletion,me),crate::svc::contracts::contract_mgr(&call,&acceptance));
            let owner=db.store.read(|db|db.table("entity").get_id(item).unwrap().int("ownerID"));
            let exists=exists(&db.store,"character_",me);
            if matches!(accepted,Some(Outcome::Value(_))) {assert!(exists);assert_eq!(owner,Some(me));assert!(!matches!(deleted,Value::None));}
            else {assert!(!exists);assert_eq!(owner,Some(1));assert!(matches!(deleted,Value::None));}
        }
    }
}
