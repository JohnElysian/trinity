//! The calls a client makes between login and entering the game: cached bulk
//! objects, authentication housekeeping, and the character selection screen.
//! (`character::SelectCharacterID` itself changes the session, so `conn` serves it.)

use eve_marshal::Value;
use tracing::info;

use crate::services::{Call, Ctx, Outcome};

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
            let sql = r#"UPDATE "character_" SET "deletePrepareDateTime" = NULL
                          WHERE "characterID" = $1 AND "accountID" = $2"#;
            ctx.execute(sql, &[&(char_id as i32), &account(ctx)]).await;
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

fn account(ctx: &Ctx<'_>) -> i32 {
    ctx.session.user_id.unwrap_or(0) as i32
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
    let sql = r#"SELECT "characterID","characterName","deletePrepareDateTime",
                        "gender","accessoryID","beardID","costumeID","decoID","eyebrowsID",
                        "eyesID","hairID","lipstickID","makeupID","skinID","backgroundID",
                        "lightID","headRotation1","headRotation2","headRotation3",
                        "eyeRotation1","eyeRotation2","eyeRotation3","camPos1","camPos2","camPos3",
                        "morph1e","morph1n","morph1s","morph1w","morph2e","morph2n","morph2s",
                        "morph2w","morph3e","morph3n","morph3s","morph3w","morph4e","morph4n",
                        "morph4s","morph4w"
                   FROM "character_" WHERE "accountID" = $1 ORDER BY "characterID""#;
    ctx.rowset(HEADER, sql, &[&account(ctx)]).await
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
    let sql = r#"SELECT c."characterID",c."petitionMessage",c."gender",c."bloodlineID",c."stationID",
                        c."solarSystemID",c."constellationID",c."regionID",c."startDateTime",
                        c."corporationID",c."bounty",c."securityRating",c."title",c."balance",
                        c."createDateTime", NULLIF(co."allianceID", 0), s."shortName", m."startDate"
                   FROM "character_" c
                   LEFT JOIN "corporation" co ON co."corporationID" = c."corporationID"
                   LEFT JOIN "alliance_shortnames" s ON s."allianceID" = co."allianceID"
                   LEFT JOIN "allianceMembers" m ON m."corporationID" = c."corporationID"
                                                AND m."allianceID" = co."allianceID"
                  WHERE c."characterID" = $1 AND c."accountID" = $2"#;
    ctx.rowset(HEADER, sql, &[&(char_id as i32), &account(ctx)]).await
}

/// Queues the character for deletion and returns when that becomes possible
/// (charsel.Terminate shows the wait), or None if it is not the caller's.
async fn prepare_for_delete(ctx: &Ctx<'_>, char_id: i64) -> Value {
    let when = crate::bulkdata::filetime_now() + BIOMASS_DELAY;
    let sql = r#"UPDATE "character_" SET "deletePrepareDateTime" = $3
                  WHERE "characterID" = $1 AND "accountID" = $2
              RETURNING "deletePrepareDateTime""#;
    match ctx.query_one(sql, &[&(char_id as i32), &account(ctx), &when]).await {
        Some(_) => {
            info!("character {char_id} queued for deletion");
            Value::Long(when)
        }
        None => Value::None,
    }
}

/// Deletes a queued character whose wait is over, with everything that belongs
/// only to it, in one statement. Shared history (kill logs, the market journal,
/// votes) stays. Returns None, or a (message, args) error the client shows.
async fn delete_character(ctx: &Ctx<'_>, char_id: i64) -> Value {
    match delete_character_checked(ctx,char_id).await {
        Ok(())=>Value::None,
        Err(reason)=>Value::Tuple(vec![Value::bytes("CustomError"),Value::Dict(vec![(Value::bytes("error"),Value::text(reason))])]),
    }
}

/// Contract creation/acceptance and biomass share this lock. Unlike a wallet
/// row lock it can be acquired before contract rows without changing their
/// existing contract-to-wallet lock order.
pub(crate) async fn lock_character_lifecycle(tx:&tokio_postgres::Transaction<'_>,char_id:i32)->Result<bool,tokio_postgres::Error> {
    const NAMESPACE:i32=0x434852;
    tx.query_one("SELECT pg_advisory_xact_lock($1,$2)",&[&NAMESPACE,&char_id]).await?;
    Ok(tx.query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1"#,&[&char_id]).await?.is_some())
}

async fn delete_character_checked(ctx:&Ctx<'_>,char_id:i64)->Result<(),String> {
    let invalid="That character is not on this account or is not ready for deletion.";
    if char_id<=0 || char_id>i32::MAX as i64 {return Err(invalid.into());}
    let failed=|e:tokio_postgres::Error| {tracing::error!("deleting character {char_id}: {}",crate::logging::chain(&e));"The character could not be deleted.".to_string()};
    let mut client=crate::db::connect(ctx.db_url).await.map_err(failed)?;
    let tx=client.transaction().await.map_err(failed)?;
    if !lock_character_lifecycle(&tx,char_id as i32).await.map_err(failed)? {return Err(invalid.into());}
    let now=crate::bulkdata::filetime_now();
    let eligible=tx.query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "accountID"=$2 AND "deletePrepareDateTime" IS NOT NULL AND "deletePrepareDateTime" <= $3 FOR UPDATE"#,&[&(char_id as i32),&account(ctx),&now]).await.map_err(failed)?;
    if eligible.is_none() {return Err(invalid.into());}
    // This account-level RPC has no selected character for the connection's
    // arrival guard. Preserve the durable final-snapshot/loss obligation until
    // recovery resolves it; deleting its hull would cascade away the marker.
    // A derelict loss does not block normal play, but biomass would also erase
    // that unoccupied hull and its outstanding loss/insurance obligation.
    let unsettled_owned_loss=ctx.world.lock().pending_ship_losses.values().any(|loss|loss.char_id==char_id);
    if unsettled_owned_loss || crate::travel::blocked_character_in_memory(ctx.world,char_id)
        || crate::svc::jumps::portal::blocked_character_in_memory(ctx.world,char_id)
        || crate::conn::logout::blocked_character_in_memory(ctx.world,char_id)
        || tx.query_one(r#"SELECT EXISTS(SELECT 1 FROM "pendingShipLosses" WHERE "characterID"=$1)"#,
        &[&(char_id as i32)]).await.map_err(failed)?.get::<_,bool>(0)
        || crate::losses::pending::blocked_character(&tx,char_id).await.map_err(failed)? {
        return Err("This character's station arrival or ship loss must finish before deletion.".into());
    }
    {
        let world=ctx.world.lock();
        if !world.clients_of(char_id).is_empty() {return Err("Log out of that character before deleting it.".into());}
        if world.retained_logouts.values().any(|(pilot,_)|*pilot==char_id) {return Err("Your ship's active cycle or pending travel must finish before deleting this character.".into());}
        if world.trades.values().any(|trade|trade.traders.contains(&char_id)) {return Err("Finish or cancel that character's trade before deleting it.".into());}
    }
    if tx.query_opt(r#"SELECT 1 FROM corporation WHERE "ceoID"=$1 LIMIT 1"#,&[&(char_id as i32)]).await.map_err(failed)?.is_some() {
        return Err("Appoint another corporation CEO before deleting this character.".into());
    }
    // Read these rows without taking locks: settlement locks contract then
    // wallet, whereas deletion already owns the character wallet row.
    let obligations=r#"SELECT 1 FROM contracts c WHERE NOT c.deleted AND (
        (NOT c."forCorp" AND c."issuerID"=$1 AND c.status IN (0,1,3))
        OR (NOT c."acceptorCorp" AND c."acceptorID"=$1 AND c.type IN (3,4,5) AND c.status IN (1,3))
        OR (c.type=2 AND c.status IN (0,2) AND EXISTS (
            SELECT 1 FROM contract_bids b WHERE b.id=(SELECT latest.id FROM contract_bids latest WHERE latest."contractID"=c."contractID" ORDER BY latest.id DESC LIMIT 1)
                AND b."bidderID"=$1 AND NOT b."forCorp"))
        OR (c.type IN (3,4) AND c.status IN (1,3) AND EXISTS (
            SELECT 1 FROM contract_items ci JOIN entity e ON e."itemID"=ci."itemID"
                WHERE ci."contractID"=c."contractID" AND ci."inCrate" AND e."ownerID"=$1))
    ) LIMIT 1"#;
    if tx.query_opt(obligations,&[&(char_id as i32)]).await.map_err(failed)?.is_some() {
        return Err("Settle or cancel this character's outstanding contracts before deleting it.".into());
    }
    let sql = r#"
        WITH target AS (
            SELECT "characterID" AS id, "corporationID" AS corp FROM "character_"
             WHERE "characterID" = $1 AND "accountID" = $2
               AND "deletePrepareDateTime" IS NOT NULL AND "deletePrepareDateTime" <= $3 FOR UPDATE),
        items AS (
            DELETE FROM entity WHERE "ownerID" IN (SELECT id FROM target)
                                  OR "itemID"  IN (SELECT id FROM target)
            RETURNING "itemID"),
        attributes AS (DELETE FROM entity_attributes WHERE "itemID" IN (SELECT "itemID" FROM items)),
        owner      AS (DELETE FROM "eveStaticOwners"     WHERE "ownerID"     IN (SELECT id FROM target)),
        names      AS (DELETE FROM "eveNames"            WHERE "itemID"      IN (SELECT id FROM target)),
        bookmarks  AS (DELETE FROM bookmarks             WHERE "ownerID"     IN (SELECT id FROM target)),
        notes      AS (DELETE FROM "chrNotes"            WHERE "characterID" IN (SELECT id FROM target)),
        ownernotes AS (DELETE FROM "chrOwnerNote"        WHERE "ownerID"     IN (SELECT id FROM target)),
        standings  AS (DELETE FROM "chrStandings"        WHERE "characterID" IN (SELECT id FROM target)),
        npcstand   AS (DELETE FROM "chrNPCStandings"     WHERE "characterID" IN (SELECT id FROM target)),
        roles      AS (DELETE FROM "chrCorporationRoles" WHERE "characterID" IN (SELECT id FROM target)),
        employment AS (DELETE FROM "chrEmployment"       WHERE "characterID" IN (SELECT id FROM target)),
        missions   AS (DELETE FROM "chrMissionState"     WHERE "characterID" IN (SELECT id FROM target)),
        offers     AS (DELETE FROM "chrOffers"           WHERE "characterID" IN (SELECT id FROM target)),
        loyalty    AS (DELETE FROM "chrAgentLoyalty"     WHERE "characterID" IN (SELECT id FROM target)),
        research   AS (DELETE FROM "chrAgentResearch"    WHERE "characterID" IN (SELECT id FROM target)),
        shares     AS (DELETE FROM "crpCharShares"       WHERE "characterID" IN (SELECT id FROM target)),
        applies    AS (DELETE FROM "crpApplications"     WHERE "characterID" IN (SELECT id FROM target)),
        orders     AS (DELETE FROM market_orders         WHERE "charID"      IN (SELECT id FROM target)
                                                           AND COALESCE("walletOwnerID","charID")="charID"),
        jumps      AS (DELETE FROM "chrJumpClones"       WHERE "ownerID"     IN (SELECT id FROM target)),
        points     AS (DELETE FROM "chrLoyaltyPoints"    WHERE "characterID" IN (SELECT id FROM target)),
        visits     AS (DELETE FROM "chrSystemVisits"     WHERE "characterID" IN (SELECT id FROM target)),
        tutorials  AS (DELETE FROM "chrTutorialState"    WHERE "characterID" IN (SELECT id FROM target)),
        insurance  AS (DELETE FROM "insuranceContracts"  WHERE "ownerID"     IN (SELECT id FROM target)),
        channels   AS (DELETE FROM "channelChars"        WHERE "charID"      IN (SELECT id FROM target)),
        members    AS (UPDATE corporation SET "memberCount" = GREATEST("memberCount" - 1, 0)
                        WHERE "corporationID" IN (SELECT corp FROM target))
        DELETE FROM "character_" WHERE "characterID" IN (SELECT id FROM target)
        RETURNING "characterName",(SELECT COALESCE(array_agg("itemID"),ARRAY[]::integer[]) FROM items)"#;
    let Some(deleted)=tx.query_opt(sql, &[&(char_id as i32), &account(ctx), &now]).await.map_err(failed)? else {return Err(invalid.into());};
    let removed:std::collections::HashSet<i64>=deleted.get::<_,Vec<i32>>(1).into_iter().map(i64::from).collect();
    tx.commit().await.map_err(failed)?;
    // SQL rechecks ownership after waiting for a claimed hull's row lock.
    // Only its actual DELETE result may remove live balls: a pre-delete owner
    // snapshot could otherwise erase a ship that another pilot just acquired.
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
    use crate::{session::Session,test_support::Database};

    async fn pilot(db:&Database,account:i32)->Session {
        let id=db.id().await;
        db.db.execute(r#"INSERT INTO "character_" ("characterID","accountID","characterName",balance,"deletePrepareDateTime") VALUES ($1,$2,$3,10000000,1)"#,&[&id,&account,&format!("Deletion fixture {id}")]).await.unwrap();
        Session {char_id:Some(id as i64),user_id:Some(account as i64),station_id:Some(60000004),..Default::default()}
    }
    async fn contract(db:&Database,issuer:i32,kind:i32,status:i32,acceptor:i32)->i32 {
        let now=crate::bulkdata::filetime_now();
        db.db.query_one(r#"INSERT INTO contracts (type,"issuerID","issuerCorpID","forCorp",availability,"assigneeID","acceptorID","dateIssued","dateExpired","numDays","startStationID","startSolarSystemID","startRegionID","endStationID","endSolarSystemID","endRegionID",price,reward,collateral,title,description,"sourceLocation","sourceFlag",status)
            VALUES ($1,$2,1000002,false,0,0,$3,$4,$5,1,60000004,30000142,10000002,0,0,0,0,0,0,'Deletion fixture','',60000004,4,$6) RETURNING "contractID""#,&[&kind,&issuer,&acceptor,&now,&(now+864_000_000_000),&status]).await.unwrap().get(0)
    }
    async fn escrow_item(db:&Database,contract:i32,owner:i32)->i32 {
        let item=db.db.query_one(r#"INSERT INTO entity ("typeID","ownerID","locationID",flag,quantity) VALUES (34,$1,$2,0,3) RETURNING "itemID""#,&[&owner,&contract]).await.unwrap().get::<_,i32>(0);
        db.db.execute(r#"INSERT INTO contract_items ("contractID","itemID","typeID",quantity,"inCrate") VALUES ($1,$2,34,3,true)"#,&[&contract,&item]).await.unwrap();
        item
    }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn deletion_preserves_pending_derelict_losses_in_memory_and_after_restart() {
        let db=Database::load().await;let account=db.id().await;
        let target=pilot(&db,account).await;let me=target.char_id.unwrap() as i32;
        let hull=db.id().await;let system=30004971i32;
        db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton)
            VALUES($1,587,$2,$3,0,1,1)"#,&[&hull,&me,&system]).await.unwrap();
        let loss=crate::losses::Loss {client_id:0,char_id:me as i64,ship_id:hull as i64,ship_type:587,system:system as i64,
            pos:crate::ballpark::Vec3::ZERO,arrival_custody:None,capsule:false,unpiloted:true,final_blow:None,attackers:vec![],items:vec![]};
        let caller=Session {user_id:Some(account as i64),..Default::default()};let ctx=db.ctx(&caller);
        db.world.lock().pending_ship_losses.insert(hull as i64,loss.clone());
        assert!(!crate::losses::journal::blocked_character_in_memory(&db.world,me as i64),"a derelict loss does not immobilize its owner's current ship");
        assert!(delete_character_checked(&ctx,me as i64).await.is_err(),"biomass must still retain its owner's outstanding hull loss");
        assert!(db.db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&hull]).await.unwrap().is_some());
        let mut writer=crate::db::connect(&db.url).await.unwrap();let tx=writer.transaction().await.unwrap();
        assert!(lock_character_lifecycle(&tx,me).await.unwrap());
        tx.query_one(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#,&[&me]).await.unwrap();
        assert!(crate::losses::journal::stage(&tx,&loss).await.unwrap());tx.commit().await.unwrap();
        db.world.lock().pending_ship_losses.clear();
        assert!(!crate::losses::pending::blocked_character(&db.db,me as i64).await.unwrap(),"normal RPC quarantine remains occupied-only");
        assert!(delete_character_checked(&ctx,me as i64).await.is_err(),"restart cannot erase a durable derelict loss");
        assert!(!db.db.query_one(r#"SELECT occupied FROM "pendingShipLosses" WHERE "shipID"=$1"#,&[&hull]).await.unwrap().get::<_,bool>(0));
        assert!(db.db.query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1"#,&[&me]).await.unwrap().is_some());
        db.db.execute(r#"DELETE FROM "pendingShipLosses" WHERE "shipID"=$1"#,&[&hull]).await.unwrap();
        delete_character_checked(&ctx,me as i64).await.unwrap();
        assert!(db.db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&hull]).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn deletion_preserves_pending_arrival_even_when_marker_commits_while_deletion_waits() {
        let db=Database::load().await;let account=db.id().await;
        let target=pilot(&db,account).await;let me=target.char_id.unwrap() as i32;
        let hull=db.id().await;let station=60014659i32;let source=30004971i32;
        db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton)
            VALUES($1,587,$2,$3,4,1,1)"#,&[&hull,&me,&station]).await.unwrap();
        let caller=Session {user_id:Some(account as i64),..Default::default()};
        let ctx=db.ctx(&caller);
        let mut arrival_db=crate::db::connect(&db.url).await.unwrap();
        let arrival=arrival_db.transaction().await.unwrap();
        assert!(lock_character_lifecycle(&arrival,me).await.unwrap());
        arrival.execute(r#"UPDATE "character_" SET "shipID"=$2,"stationID"=$3 WHERE "characterID"=$1"#,&[&me,&hull,&station]).await.unwrap();
        let marker=crate::losses::pending::begin(&arrival,me as i64,hull as i64,station as i64,source as i64).await.unwrap();
        let blocker:i32=arrival.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
        let release=async {
            tokio::time::timeout(std::time::Duration::from_secs(10),async {loop {
                if db.db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE $1=ANY(pg_blocking_pids(pid)))",&[&blocker]).await.unwrap().get::<_,bool>(0) {break;}
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }}).await.expect("deletion waits for the accepted arrival transaction");
            arrival.commit().await.unwrap();
        };
        let (deleted,())=tokio::join!(delete_character_checked(&ctx,me as i64),release);
        assert!(deleted.unwrap_err().contains("station arrival"));
        assert!(crate::losses::pending::blocked_character(&db.db,me as i64).await.unwrap());
        assert!(db.db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&hull]).await.unwrap().is_some());
        // Both an unknown final snapshot and an unreadable recorded loss stay
        // quarantined; biomass must not choose a survival or death outcome.
        db.db.execute(r#"UPDATE "pendingStationArrivals" SET loss='{}' WHERE "shipID"=$1 AND generation=$2"#,&[&hull,&marker.generation]).await.unwrap();
        assert!(delete_character_checked(&ctx,me as i64).await.unwrap_err().contains("station arrival"));
        let foreign=Session {user_id:Some(account as i64+1),..Default::default()};
        assert!(delete_character_checked(&db.ctx(&foreign),me as i64).await.unwrap_err().contains("not on this account"));
        db.db.execute(r#"DELETE FROM "pendingStationArrivals" WHERE "shipID"=$1"#,&[&hull]).await.unwrap();
        delete_character_checked(&ctx,me as i64).await.unwrap();
        assert!(db.db.query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1"#,&[&me]).await.unwrap().is_none());
        assert!(db.db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&hull]).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn deletion_removes_empty_hulls_but_keeps_a_hull_claimed_while_sql_waits() {
        use crate::ballpark::{Park,Pilot,Ship,ShipStats,Vec3};
        let db=Database::load().await;
        let account=db.id().await;
        let target=pilot(&db,account).await;
        let other=pilot(&db,account).await.char_id.unwrap() as i32;
        let owner=target.char_id.unwrap() as i32;
        let ghost=db.id().await;
        let claimed=db.id().await;
        let foreign=db.id().await;
        let observer=db.id().await;
        let system=30000142i32;
        let mut park=Park::new(system as i64,vec![],crate::destiny::tick_now());
        for (id,property_owner,unpiloted) in [(ghost,owner,true),(claimed,owner,true),(foreign,other,true),(observer,other,false)] {
            db.db.execute(r#"INSERT INTO entity("itemID","typeID","ownerID","locationID",flag,quantity,singleton)
                VALUES($1,587,$2,$3,0,1,1)"#,&[&id,&property_owner,&system]).await.unwrap();
            let mut ship=Ship::new(id as i64,if unpiloted {0} else {9},ShipStats::new(37.0,1.1e6,3.1,320.0,2.0),
                Pilot{type_id:587,group_id:25,name:"Biomass hull".into(),char_id:property_owner as i64,
                    corp_id:1000002,alliance_id:None,security:0.0,bounty:0.0},Vec3::ZERO,Vec3::ZERO);
            ship.unpiloted=unpiloted;
            park.ships.insert(id as i64,ship);
        }
        let kind=crate::npc::NpcType::with(3634,53,1.0,1.0,&[]);
        let mut fighter=crate::npc::entity_fighter(&kind,10000.0,1,100.0,crate::combat::now_ms());
        fighter.locks.insert(ghost as i64,crate::combat::Lock::Locked);
        fighter.locks.insert(claimed as i64,crate::combat::Lock::Locked);
        park.ships.get_mut(&(observer as i64)).unwrap().fight=Some(fighter);
        let mut receiver={
            let mut world=db.world.lock();
            let rx=world.connect(9);
            world.set_presence(9,Some(crate::world::Presence{char_id:other as i64,corp_id:1000002,
                solar_system_id:system as i64,docked:false,station_id:None,role:0}));
            world.sees(9,system as i64);
            world.parks.insert(system as i64,park);
            rx
        };
        let caller=Session{user_id:Some(account as i64),..Default::default()};
        let ctx=db.ctx(&caller);
        let wrong=Session{user_id:Some(account as i64+1),..Default::default()};
        assert!(delete_character_checked(&db.ctx(&wrong),owner as i64).await.is_err());
        assert!(db.world.lock().parks[&(system as i64)].ships.contains_key(&(ghost as i64)));
        assert!(receiver.try_recv().is_err(),"refused deletion cannot remove live ships");

        let mut claiming_db=crate::db::connect(&db.url).await.unwrap();
        let claim=claiming_db.transaction().await.unwrap();
        let blocker:i32=claim.query_one("SELECT pg_backend_pid()",&[]).await.unwrap().get(0);
        claim.execute(r#"UPDATE entity SET "ownerID"=$2 WHERE "itemID"=$1"#,&[&claimed,&other]).await.unwrap();
        let release=async {
            tokio::time::timeout(std::time::Duration::from_secs(10),async {
                loop {
                    let blocked:bool=db.db.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity a WHERE $1=ANY(pg_blocking_pids(a.pid)))",&[&blocker]).await.unwrap().get(0);
                    if blocked {break;}
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }).await.expect("biomass deletion waits on the claiming transaction");
            assert!(db.world.lock().parks[&(system as i64)].ships.contains_key(&(ghost as i64)),"no live mutation before commit");
            claim.commit().await.unwrap();
        };
        let (deleted,())=tokio::join!(delete_character_checked(&ctx,owner as i64),release);
        deleted.unwrap();
        assert!(db.db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&ghost]).await.unwrap().is_none());
        assert_eq!(db.db.query_one(r#"SELECT "ownerID" FROM entity WHERE "itemID"=$1"#,&[&claimed]).await.unwrap().get::<_,i32>(0),other);
        {
            let world=db.world.lock();
            let park=&world.parks[&(system as i64)];
            assert!(!park.ships.contains_key(&(ghost as i64)));
            assert!(park.ships.contains_key(&(claimed as i64)),"stale live ownership must not erase a hull that SQL retained");
            assert!(park.ships.contains_key(&(foreign as i64)));
            let locks=&park.ships[&(observer as i64)].fight.as_ref().unwrap().locks;
            assert!(!locks.contains_key(&(ghost as i64)),"normal departure bookkeeping clears locks");
            assert!(locks.contains_key(&(claimed as i64)));
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
        assert_eq!(removed,vec![ghost as i64],"observers lose only the hull actually deleted");
    }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn deletion_preserves_ceos_contract_debtors_and_winning_bidders() {
        let db=Database::load().await;let account=db.id().await;let target=pilot(&db,account).await;
        let me=target.char_id.unwrap() as i32;let caller=Session {user_id:Some(account as i64),..Default::default()};let ctx=db.ctx(&caller);
        let corp=db.id().await;
        db.db.execute(r#"INSERT INTO corporation ("corporationID","corporationName","ceoID") VALUES ($1,$2,$3)"#,&[&corp,&format!("CEO fixture {corp}"),&me]).await.unwrap();
        assert!(!matches!(delete_character(&ctx,me as i64).await,Value::None));
        db.db.execute(r#"UPDATE corporation SET "ceoID"=0 WHERE "corporationID"=$1"#,&[&corp]).await.unwrap();
        let offered=contract(&db,me,1,0,0).await;
        assert!(!matches!(delete_character(&ctx,me as i64).await,Value::None));
        db.db.execute("UPDATE contracts SET status=4 WHERE \"contractID\"=$1",&[&offered]).await.unwrap();
        let borrowed=contract(&db,1,4,1,me).await;
        let item=escrow_item(&db,borrowed,me).await;
        assert!(!matches!(delete_character(&ctx,me as i64).await,Value::None));
        db.db.execute(r#"UPDATE contracts SET "acceptorID"=2 WHERE "contractID"=$1"#,&[&borrowed]).await.unwrap();
        // Borrowed goods still belong to a live obligation even when passed on.
        assert!(!matches!(delete_character(&ctx,me as i64).await,Value::None));
        assert!(db.db.query_opt(r#"SELECT 1 FROM entity WHERE "itemID"=$1"#,&[&item]).await.unwrap().is_some());
        db.db.execute("UPDATE contracts SET status=7 WHERE \"contractID\"=$1",&[&borrowed]).await.unwrap();
        let auction=contract(&db,1,2,0,0).await;
        db.db.execute(r#"INSERT INTO contract_bids ("contractID","bidderID",amount,"dateBid","forCorp") VALUES ($1,$2,1000,1,false)"#,&[&auction,&me]).await.unwrap();
        assert!(!matches!(delete_character(&ctx,me as i64).await,Value::None));
        db.db.execute("UPDATE contracts SET status=2 WHERE \"contractID\"=$1",&[&auction]).await.unwrap();
        assert!(!matches!(delete_character(&ctx,me as i64).await,Value::None));
        db.db.execute(r#"INSERT INTO contract_bids ("contractID","bidderID",amount,"dateBid","forCorp") VALUES ($1,2,2000,2,false)"#,&[&auction]).await.unwrap();
        // An already-paid auction issuer and a corporation's former acting
        // issuer have no personal obligation left to block deletion.
        contract(&db,me,2,2,2).await;
        let corp_contract=contract(&db,me,1,0,0).await;
        db.db.execute(r#"UPDATE contracts SET "forCorp"=true,"issuerCorpID"=$2 WHERE "contractID"=$1"#,&[&corp_contract,&corp]).await.unwrap();
        let foreign=Session {user_id:Some((account+1) as i64),..Default::default()};
        assert!(!matches!(delete_character(&db.ctx(&foreign),me as i64).await,Value::None));
        db.db.execute(r#"UPDATE "character_" SET "deletePrepareDateTime"=$2 WHERE "characterID"=$1"#,&[&me,&(crate::bulkdata::filetime_now()+864_000_000_000)]).await.unwrap();
        assert!(!matches!(delete_character(&ctx,me as i64).await,Value::None));
        db.db.execute(r#"UPDATE "character_" SET "deletePrepareDateTime"=1 WHERE "characterID"=$1"#,&[&me]).await.unwrap();
        assert!(matches!(delete_character(&ctx,me as i64).await,Value::None));
        assert!(db.db.query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1"#,&[&me]).await.unwrap().is_none());
    }

    #[tokio::test]
    #[ignore="requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn deletion_and_zero_value_loan_acceptance_cannot_orphan_goods() {
        let db=Database::load().await;let account=db.id().await;
        let issuer=pilot(&db,account).await;
        for _ in 0..4 {
            let borrower=pilot(&db,account).await;let me=borrower.char_id.unwrap() as i32;
            let contract=contract(&db,issuer.char_id.unwrap() as i32,4,0,0).await;
            let item=escrow_item(&db,contract,1).await;
            let caller=Session {user_id:Some(account as i64),..Default::default()};
            let deletion=db.ctx(&caller);let acceptance=db.ctx(&borrower);
            let call=Call {service:"contractMgr".into(),method:"AcceptContract".into(),args:vec![Value::Int(contract as i64),Value::Bool(false)],kwargs:vec![]};
            let (deleted,accepted)=tokio::join!(delete_character(&deletion,me as i64),crate::svc::contracts::contract_mgr(&call,&acceptance));
            let owner:i32=db.db.query_one(r#"SELECT "ownerID" FROM entity WHERE "itemID"=$1"#,&[&item]).await.unwrap().get(0);
            let exists=db.db.query_opt(r#"SELECT 1 FROM "character_" WHERE "characterID"=$1"#,&[&me]).await.unwrap().is_some();
            if matches!(accepted,Some(Outcome::Value(_))) {assert!(exists);assert_eq!(owner,me);assert!(!matches!(deleted,Value::None));}
            else {assert!(!exists);assert_eq!(owner,1);assert!(matches!(deleted,Value::None));}
        }
    }
}
