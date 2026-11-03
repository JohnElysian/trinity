//! Trinity contracts: escrowed goods and money, with all writes committed together.
//! Wire columns and method signatures follow client contracts.py and journal.py.

use super::{
    rows::{self, key_val, row, rowset},
    wallet::{post, Entry, Transfer},
};
use crate::{
    items::{Item, ItemTxn, NewItem},
    services::{Call, Ctx, Outcome},
};
use eve_marshal::Value;
use std::collections::{HashMap, HashSet};
use tokio_postgres::{types::ToSql, Client, Row};

const DAY: i64 = 864_000_000_000;
const CONTRACT_MANAGER: i64 = 72_057_594_037_927_936;
const DELIVERIES_ROLES: i64 = 256 | 4_503_599_627_370_496 | 18_014_398_509_481_984 | 1;
const SYSTEM: i64 = 1;
const HEADER: &[&str] = &[
    "contractID",
    "type",
    "issuerID",
    "issuerCorpID",
    "forCorp",
    "availability",
    "assigneeID",
    "acceptorID",
    "dateIssued",
    "dateExpired",
    "dateAccepted",
    "numDays",
    "dateCompleted",
    "startStationID",
    "startSolarSystemID",
    "startRegionID",
    "endStationID",
    "endSolarSystemID",
    "endRegionID",
    "price",
    "reward",
    "collateral",
    "title",
    "description",
    "volume",
    "status",
    "issuerAllianceID",
    "issuerWalletKey",
    "acceptorWalletKey",
];
const ITEM_HEADER: &[&str] = &[
    "itemID",
    "typeID",
    "quantity",
    "inCrate",
    "parentID",
    "copy",
    "materialLevel",
    "productivityLevel",
    "bpRuns",
];
const BID_HEADER: &[&str] = &["bidderID", "amount", "dateBid"];

pub async fn ensure(db: &Client) -> std::result::Result<(), tokio_postgres::Error> {
    db.batch_execute(r#"
      CREATE TABLE IF NOT EXISTS contracts (
        "contractID" integer PRIMARY KEY DEFAULT nextval('dyn_id_seq'),
        "type" integer NOT NULL, "issuerID" integer NOT NULL, "issuerCorpID" integer NOT NULL,
        "forCorp" boolean NOT NULL, "availability" integer NOT NULL, "assigneeID" integer NOT NULL,
        "acceptorID" integer NOT NULL DEFAULT 0, "dateIssued" bigint NOT NULL, "dateExpired" bigint NOT NULL,
        "dateAccepted" bigint NOT NULL DEFAULT 0, "numDays" integer NOT NULL, "dateCompleted" bigint NOT NULL DEFAULT 0,
        "startStationID" integer NOT NULL, "startSolarSystemID" integer NOT NULL, "startRegionID" integer NOT NULL,
        "endStationID" integer NOT NULL, "endSolarSystemID" integer NOT NULL, "endRegionID" integer NOT NULL,
        price double precision NOT NULL, reward double precision NOT NULL, collateral double precision NOT NULL,
        title text NOT NULL, description text NOT NULL, volume double precision NOT NULL DEFAULT 0,
        status integer NOT NULL DEFAULT 0, "issuerAllianceID" integer NOT NULL DEFAULT 0,
        "issuerWalletKey" integer NOT NULL DEFAULT 1000, "acceptorWalletKey" integer NOT NULL DEFAULT 1000,
        "acceptorCorp" boolean NOT NULL DEFAULT false, "crateID" integer NOT NULL DEFAULT 0,
        "sourceLocation" integer NOT NULL, "sourceFlag" integer NOT NULL, deleted boolean NOT NULL DEFAULT false);
      CREATE INDEX IF NOT EXISTS contracts_issuer ON contracts ("issuerID", status);
      CREATE INDEX IF NOT EXISTS contracts_corporation ON contracts ("issuerCorpID", status);
      CREATE INDEX IF NOT EXISTS contracts_assignee ON contracts ("assigneeID", status);
      ALTER TABLE contracts ADD COLUMN IF NOT EXISTS deposit double precision NOT NULL DEFAULT 0;
      ALTER TABLE contracts ADD COLUMN IF NOT EXISTS "salesTax" double precision NOT NULL DEFAULT 0;
      ALTER TABLE contracts ADD COLUMN IF NOT EXISTS "taxRate" double precision NOT NULL DEFAULT 0;
      ALTER TABLE contracts ADD COLUMN IF NOT EXISTS "taxCap" double precision NOT NULL DEFAULT 0;
      CREATE TABLE IF NOT EXISTS contract_items (
        id bigserial PRIMARY KEY, "contractID" integer NOT NULL REFERENCES contracts("contractID"),
        "itemID" integer NOT NULL, "typeID" integer NOT NULL, quantity integer NOT NULL,
        "inCrate" boolean NOT NULL, "parentID" integer NOT NULL DEFAULT 0,
        copy integer, "materialLevel" integer, "productivityLevel" integer, "bpRuns" integer);
      CREATE INDEX IF NOT EXISTS contract_items_contract ON contract_items ("contractID");
      CREATE TABLE IF NOT EXISTS contract_bids (
        id bigserial PRIMARY KEY, "contractID" integer NOT NULL REFERENCES contracts("contractID"),
        "bidderID" integer NOT NULL, amount double precision NOT NULL, "dateBid" bigint NOT NULL,
        "forCorp" boolean NOT NULL);
      CREATE INDEX IF NOT EXISTS contract_bids_contract ON contract_bids ("contractID", id DESC);
      CREATE TABLE IF NOT EXISTS contract_notifications (
        "contractID" integer NOT NULL REFERENCES contracts("contractID"), "ownerID" integer NOT NULL,
        PRIMARY KEY ("contractID", "ownerID"));
    "#).await
}

async fn counters(call: &Call, ctx: &Ctx<'_>) -> Result<Value> {
    let me = owner(ctx, false)? as i32;
    let corp = ctx.session.corp_id.unwrap_or(0) as i32;
    let rs=ctx.db.query(r#"SELECT c.*, ($2>=100000000 AND (c."assigneeID"=$2 OR EXISTS (SELECT 1 FROM "character_" a WHERE a."characterID"=c."assigneeID" AND a."corporationID"=$2))) AS internal FROM contracts c WHERE NOT deleted AND ("issuerID"=$1 OR "issuerCorpID"=$2 OR "assigneeID" IN ($1,$2) OR "acceptorID" IN ($1,$2))"#,&[&me,&corp]).await?;
    let now = crate::bulkdata::filetime_now();
    let outstanding = |who: i32| {
        rs.iter()
            .filter(|r| issuer(r) == who as i64 && number(r, "status") == 0)
            .count() as i64
    };
    let assigned = |who: i32| {
        rs.iter()
            .filter(|r| {
                number(r, "assigneeID") == who as i64
                    && number(r, "status") == 0
                    && r.get::<_, i64>("dateExpired") > now
            })
            .count() as i64
    };
    let progress = |who: i32| {
        rs.iter()
            .filter(|r| number(r, "acceptorID") == who as i64 && number(r, "status") == 1)
            .count() as i64
    };
    let mut attention = [0i64; 2];
    let mut bidding = [0i64; 2];
    for (i, who) in [me, corp].iter().enumerate() {
        attention[i]=ctx.db.query_one(&format!(r#"SELECT count(*) FROM contracts c WHERE NOT deleted AND (CASE WHEN "forCorp" THEN "issuerCorpID" ELSE "issuerID" END=$1 OR "acceptorID"=$1 OR EXISTS (SELECT 1 FROM contract_bids b WHERE b."contractID"=c."contractID" AND b."bidderID"=$1)) AND ((c.type=2 AND c.status=0 AND EXISTS (SELECT 1 FROM contract_bids mine WHERE mine."contractID"=c."contractID" AND mine."bidderID"=$1) AND (SELECT latest."bidderID" FROM contract_bids latest WHERE latest."contractID"=c."contractID" ORDER BY latest.id DESC LIMIT 1)<>$1) OR status=6 OR (status IN (0,2,3) AND "dateExpired"<=$2) OR (status=1 AND "dateAccepted"+"numDays"::bigint*{DAY}<=$2)) AND NOT EXISTS (SELECT 1 FROM contract_notifications n WHERE n."contractID"=c."contractID" AND n."ownerID"=$1)"#),&[who,&now]).await?.get(0);
        bidding[i]=ctx.db.query_one(r#"SELECT count(DISTINCT c."contractID") FROM contracts c JOIN contract_bids b USING ("contractID") WHERE NOT c.deleted AND c.status=0 AND c."dateExpired">$2 AND b."bidderID"=$1"#,&[who,&now]).await?.get(0);
    }
    let non_corp = rs
        .iter()
        .filter(|r| {
            !r.get::<_, bool>("forCorp")
                && number(r, "issuerID") == me as i64
                && !r.get::<_, bool>("internal")
                && number(r, "status") == 0
        })
        .count() as i64;
    let for_corp = rs
        .iter()
        .filter(|r| {
            r.get::<_, bool>("forCorp")
                && number(r, "issuerID") == me as i64
                && !r.get::<_, bool>("internal")
                && number(r, "status") == 0
        })
        .count() as i64;
    let values = match call.method.as_str() {
        "NumRequiringAttention" => vec![("n", attention[0]), ("ncorp", attention[1])],
        "NumOutstandingContracts" => vec![
            ("myCharTotal", outstanding(me)),
            ("myCorpTotal", outstanding(corp)),
            ("nonCorpForMyChar", non_corp),
            ("nonCorpForMyCorp", for_corp),
        ],
        _ => vec![
            ("numOutstandingContracts", outstanding(corp)),
            ("numOutstandingContractsNonCorp", non_corp),
            ("numOutstandingContractsForCorp", for_corp),
            ("numRequiresAttention", attention[0]),
            ("numRequiresAttentionCorp", attention[1]),
            ("numAssignedTo", assigned(me)),
            ("numAssignedToCorp", assigned(corp)),
            ("numBiddingOn", bidding[0]),
            ("numBiddingOnCorp", bidding[1]),
            ("numInProgress", progress(me)),
            ("numInProgressCorp", progress(corp)),
        ],
    };
    Ok(key_val(
        values
            .into_iter()
            .map(|(k, v)| (k, Value::Int(v)))
            .collect(),
    ))
}

async fn inventory(call: &Call, ctx: &Ctx<'_>) -> Result<Value> {
    let station = call.arg_id(0);
    let container = call.method != "GetItemsInStation";
    let corp = flag(call, "forCorp", if container { 2 } else { 1 });
    let who = owner(ctx, corp)?;
    let deliveries = corp && ctx.session.corp_role() & DELIVERIES_ROLES != 0;
    let mut location = station;
    if corp {
        location = ctx
            .db
            .query_opt(
                r#"SELECT "itemID" FROM "crpOffices" WHERE "stationID"=$1 AND "corporationID"=$2"#,
                &[&(station as i32), &(who as i32)],
            )
            .await?
            .map(|r| r.get::<_, i32>(0) as i64)
            .unwrap_or(station);
    }
    if container {
        let id = call.arg_id(1);
        let r = ctx
            .db
            .query_opt(
                r#"SELECT "ownerID","locationID","flag" FROM entity WHERE "itemID"=$1"#,
                &[&(id as i32)],
            )
            .await?;
        if !r.is_some_and(|r| {
            r.get::<_, i32>(0) as i64 == who
                && (r.get::<_, i32>(1) as i64 == location
                    || (deliveries
                        && r.get::<_, i32>(1) as i64 == station
                        && r.get::<_, i32>(2) == 62))
                && (!corp
                    || super::corphangar::may_query(ctx, r.get::<_, i32>(2) as i64)
                    || (deliveries && r.get::<_, i32>(2) == 62))
        }) {
            return refuse("That container is not in your hangar at this station.");
        }
        location = id;
    }
    if call.method == "HasFittedCharges" {
        let r=ctx.db.query_opt(r#"SELECT e."typeID" FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID" JOIN "invGroups" g ON g."groupID"=t."groupID" WHERE e."locationID"=$1 AND g."categoryID"=8 AND e.flag BETWEEN 11 AND 34 LIMIT 1"#,&[&(location as i32)]).await?;
        return Ok(r
            .map(|r| Value::Int(r.get::<_, i32>(0) as i64))
            .unwrap_or(Value::None));
    }
    let sql = format!(
        r#"{} e."ownerID"=$1 AND (e."locationID"=$2 OR ($3 AND e."locationID"=$4 AND e.flag=62)) ORDER BY e."itemID""#,
        super::inventory::INVITEM_SELECT
    );
    let rs = ctx
        .db
        .query(
            &sql,
            &[
                &(who as i32),
                &(location as i32),
                &(deliveries && !container),
                &(station as i32),
            ],
        )
        .await?;
    let rs: Vec<Row> = rs
        .into_iter()
        .filter(|r| {
            container
                || (!corp && r.get::<_, i32>(4) == 4)
                || (corp && super::corphangar::may_query(ctx, r.get::<_, i32>(4) as i64))
                || (deliveries && r.get::<_, i32>(4) == 62)
        })
        .collect();
    Ok(rowset(super::inventory::INVITEM_HEADER, rows::lines(&rs)))
}

async fn write(call: &Call, ctx: &Ctx<'_>) -> Result<Value> {
    let actor = owner(ctx, false)?;
    let mut db = crate::db::connect(ctx.db_url).await?;
    let mut tx = ItemTxn::begin(&mut db).await?;
    if !super::login::lock_character_lifecycle(tx.tx(), actor as i32).await? {
        return refuse("That character no longer exists.");
    }
    let mut fx = Effects::default();
    let result = match call.method.as_str() {
        "CreateContract" => create(call, ctx, &mut tx, &mut fx).await?,
        "SplitStack" => split(call, ctx, &mut tx).await?,
        "DeleteNotification" => {
            let who = owner(ctx, flag(call, "forCorp", 1))?;
            tx.tx().execute(r#"INSERT INTO contract_notifications ("contractID","ownerID") SELECT "contractID",$2 FROM contracts WHERE "contractID"=$1 ON CONFLICT DO NOTHING"#,&[&(call.arg_id(0) as i32),&(who as i32)]).await?;
            Value::None
        }
        _ => {
            let r = tx
                .tx()
                .query_opt(
                    r#"SELECT * FROM contracts WHERE "contractID"=$1 AND NOT deleted FOR UPDATE"#,
                    &[&(call.arg_id(0) as i32)],
                )
                .await?
                .ok_or_else(|| Failure::Text("That contract no longer exists.".into()))?;
            fx.owners.insert(issuer(&r));
            match call.method.as_str() {
                "AcceptContract" => accept(call, ctx, &mut tx, &mut fx, &r).await?,
                "PlaceBid" => bid(call, ctx, &mut tx, &mut fx, &r).await?,
                "FinishAuction" => finish_auction(call, ctx, &mut tx, &mut fx, &r).await?,
                _ => complete(call, ctx, &mut tx, &mut fx, &r).await?,
            }
            Value::Bool(true)
        }
    };
    let changes = tx.commit().await?;
    let w = ctx.world.lock();
    let mut clients = vec![ctx.client_id];
    for who in &fx.owners {
        clients.extend(w.clients_of(*who));
        clients.extend(w.corp_members(*who));
    }
    clients.sort_unstable();
    clients.dedup();
    for change in changes {
        w.notify(&clients, "OnItemChange", change);
    }
    for (who, balance) in fx.balances {
        let mut clients = w.clients_of(who);
        clients.extend(w.corp_members(who));
        w.notify(
            &clients,
            "OnAccountChange",
            Value::Tuple(vec![
                Value::bytes("cash"),
                Value::Int(who),
                Value::Double(balance),
            ]),
        );
    }
    if let Some((event, id, station)) = fx.event {
        let location = key_val(vec![
            ("locationID", Value::Int(station)),
            ("groupID", Value::Int(15)),
        ]);
        w.notify(
            &clients,
            event,
            Value::Tuple(vec![Value::Int(id), location]),
        );
    }
    Ok(result)
}

#[derive(Debug)]
struct NewContract {
    kind: i32,
    availability: i32,
    assignee: i32,
    expiry: i64,
    days: i32,
    start: i32,
    end: i32,
    price: f64,
    reward: f64,
    collateral: f64,
    title: String,
    description: String,
    items: Vec<(i64, i64)>,
    requests: Vec<(i64, i64)>,
    flag: i64,
    corp: bool,
}
fn pairs(value: Option<&Value>) -> Result<Vec<(i64, i64)>> {
    let Some(value) = value else {
        return Ok(vec![]);
    };
    let entries = value
        .seq()
        .ok_or_else(|| Failure::Text("Invalid item list.".into()))?;
    if entries.len() > 100 {
        return refuse("A contract can contain at most 100 items.");
    }
    let mut seen = HashSet::new();
    let mut out = vec![];
    for v in entries {
        let p = v
            .seq()
            .ok_or_else(|| Failure::Text("Invalid item list.".into()))?;
        let id = p.first().and_then(Value::as_i64).unwrap_or(0);
        let qty = p.get(1).and_then(Value::as_i64).unwrap_or(0);
        if p.len() != 2
            || id <= 0
            || id > i32::MAX as i64
            || qty <= 0
            || qty > i32::MAX as i64
            || !seen.insert(id)
        {
            return refuse("Contract items must have distinct IDs and positive quantities.");
        }
        out.push((id, qty));
    }
    out.sort_unstable();
    Ok(out)
}
impl NewContract {
    fn parse(call: &Call) -> Result<Self> {
        for index in [0, 1, 2, 4, 5, 6] {
            if !call
                .arg_i64(index)
                .is_some_and(|n| (0..=i32::MAX as i64).contains(&n))
            {
                return refuse("Invalid contract identifier or duration.");
            }
        }
        let n = Self {
            kind: call.arg_id(0) as i32,
            availability: call.arg_id(1) as i32,
            assignee: call.arg_id(2) as i32,
            expiry: call.arg_id(3),
            days: call.arg_id(4) as i32,
            start: call.arg_id(5) as i32,
            end: call.arg_id(6) as i32,
            price: call.args.get(7).and_then(Value::as_f64).unwrap_or(f64::NAN),
            reward: call.args.get(8).and_then(Value::as_f64).unwrap_or(f64::NAN),
            collateral: call.args.get(9).and_then(Value::as_f64).unwrap_or(f64::NAN),
            title: call.arg_text(10).unwrap_or_default(),
            description: call.arg_text(11).unwrap_or_default(),
            items: pairs(arg(call, "itemList", 12))?,
            flag: arg(call, "flag", 13).and_then(Value::as_i64).unwrap_or(4),
            requests: pairs(arg(call, "requestItemTypeList", 14))?,
            corp: flag(call, "forCorp", 15),
        };
        if !(1..=5).contains(&n.kind)
            || ![0, 1].contains(&n.availability)
            || n.assignee < 0
            || (n.availability == 1) != (n.assignee > 0)
        {
            return refuse("Invalid contract type or availability.");
        }
        if ![1440, 4320, 10080, 20160].contains(&n.expiry)
            || !(0..=365).contains(&n.days)
            || ([3, 4, 5].contains(&n.kind) && n.days == 0)
        {
            return refuse("Invalid contract expiry or duration.");
        }
        if [n.price, n.reward, n.collateral]
            .iter()
            .any(|v| !v.is_finite() || !(0.0..=1e12).contains(v))
        {
            return refuse("Contract amounts must be between 0 and one trillion ISK.");
        }
        if n.title.chars().count() > 50 || n.description.chars().count() > 4000 {
            return refuse("The contract title or description is too long.");
        }
        if n.kind != 1 && !n.requests.is_empty() {
            return refuse("Only item exchanges can request items.");
        }
        if ([2, 3].contains(&n.kind) || (n.kind == 4 && n.reward == 0.0)) && n.items.is_empty() {
            return refuse("Select at least one item for this contract.");
        }
        if n.kind == 2 && (n.reward > 0.0 || (n.collateral > 0.0 && n.collateral < n.price)) {
            return refuse("The auction buyout must cover its starting price.");
        }
        if n.kind == 5 && (!n.items.is_empty() || n.price != 0.0 || n.start != 0) {
            return refuse("Freeform contracts may contain a reward and collateral, but no items.");
        }
        Ok(n)
    }
}

async fn station(tx: &ItemTxn<'_>, id: i32) -> Result<(i32, i32)> {
    let r = tx
        .tx()
        .query_opt(
            r#"SELECT "solarSystemID","regionID" FROM "staStations" WHERE "stationID"=$1"#,
            &[&id],
        )
        .await?
        .ok_or_else(|| Failure::Text("Select a valid station.".into()))?;
    Ok((r.get(0), r.get(1)))
}
async fn source(
    tx: &ItemTxn<'_>,
    ctx: &Ctx<'_>,
    station: i64,
    who: i64,
    corp: bool,
    flag: i64,
) -> Result<i64> {
    if !corp {
        if flag != 4 {
            return refuse("Items must be in your personal hangar.");
        }
        return Ok(station);
    }
    if flag == 62 {
        if ctx.session.corp_role() & DELIVERIES_ROLES == 0 {
            return refuse("You cannot take items from corporation deliveries.");
        }
        return Ok(station);
    }
    if !super::corphangar::may_take(ctx, flag) {
        return refuse("You cannot take items from that corporation division.");
    }
    tx.tx()
        .query_opt(
            r#"SELECT "itemID" FROM "crpOffices" WHERE "stationID"=$1 AND "corporationID"=$2"#,
            &[&(station as i32), &(who as i32)],
        )
        .await?
        .map(|r| r.get::<_, i32>(0) as i64)
        .ok_or_else(|| Failure::Text("Your corporation has no office at that station.".into()))
}
async fn portion(tx: &mut ItemTxn<'_>, item: &Item, qty: i64) -> Result<i64> {
    if qty < 1 || qty > item.quantity || (item.singleton && qty != item.quantity) {
        return refuse("The selected quantity is no longer available.");
    }
    if qty == item.quantity {
        return Ok(item.id);
    }
    tx.set_quantity(item.id, item.quantity - qty).await?;
    Ok(tx
        .create(NewItem {
            type_id: item.type_id,
            owner_id: item.owner_id,
            location_id: item.location_id,
            flag: item.flag,
            quantity: qty,
            singleton: false,
        })
        .await?)
}

async fn create(
    call: &Call,
    ctx: &Ctx<'_>,
    tx: &mut ItemTxn<'_>,
    fx: &mut Effects,
) -> Result<Value> {
    let n = NewContract::parse(call)?;
    let who = owner(ctx, n.corp)?;
    // Serialise creation for one issuer so simultaneous calls cannot exceed its cap.
    tx.tx()
        .query_one("SELECT pg_advisory_xact_lock($1)", &[&who])
        .await?;
    let count:i64=tx.tx().query_one(r#"SELECT count(*) FROM contracts WHERE NOT deleted AND status=0 AND CASE WHEN "forCorp" THEN "issuerCorpID" ELSE "issuerID" END=$1"#,&[&(who as i32)]).await?.get(0);
    if count >= 500 {
        return refuse("You have reached the outstanding contract limit.");
    }
    let me = ctx.session.char_id.unwrap_or(0);
    let skills = super::skills::levels(tx.tx(), me, &[25235, 25233, 3446, 16622]).await?;
    let corp = ctx.session.corp_id.unwrap_or(0);
    let internal = n.assignee > 0
        && corp >= 100_000_000
        && (n.assignee as i64 == corp
            || tx
                .tx()
                .query_opt(
                    r#"SELECT 1 FROM "character_" WHERE "characterID"=$1 AND "corporationID"=$2"#,
                    &[&n.assignee, &(corp as i32)],
                )
                .await?
                .is_some());
    if !internal {
        let active:i64=tx.tx().query_one(r#"SELECT count(*) FROM contracts c WHERE NOT c.deleted AND c.status=0 AND c."issuerID"=$1 AND c."forCorp"=$2 AND NOT (c."assigneeID"=$3 AND $3>=100000000) AND NOT EXISTS (SELECT 1 FROM "character_" a WHERE a."characterID"=c."assigneeID" AND a."corporationID"=$3 AND $3>=100000000)"#,&[&(me as i32),&n.corp,&(corp as i32)]).await?.get(0);
        let level = skills[&if n.corp { 25233 } else { 25235 }].clamp(0, 5);
        let limit = if n.corp {
            10 + 10 * level
        } else {
            1 + 4 * level
        };
        if active >= limit {
            return refuse("Train Contracting or Corporation Contracting to create more outstanding contracts.");
        }
    }
    let fees = fees(
        n.kind,
        n.availability,
        n.price.max(n.reward),
        n.expiry,
        skills[&3446],
        skills[&16622],
    );
    let (start_system, start_region) = if n.kind == 5 {
        (
            ctx.session.solar_system_id2.unwrap_or(0) as i32,
            ctx.session.region_id.unwrap_or(0) as i32,
        )
    } else {
        station(tx, n.start).await?
    };
    let (end_system, end_region) = if n.end > 0 {
        station(tx, n.end).await?
    } else {
        (0, 0)
    };
    if n.kind == 3 && n.end == 0 {
        return refuse("Choose a delivery station.");
    }
    if n.assignee > 0 {
        let exists=tx.tx().query_opt(r#"SELECT "ownerID" FROM "eveStaticOwners" WHERE "ownerID"=$1 UNION SELECT "characterID" FROM "character_" WHERE "characterID"=$1 UNION SELECT "corporationID" FROM corporation WHERE "corporationID"=$1 UNION SELECT "allianceID" FROM alliances WHERE "allianceID"=$1"#,&[&n.assignee]).await?;
        if exists.is_none() {
            return refuse("That assignee does not exist.");
        }
    }
    let source = if n.kind == 5 {
        0
    } else {
        source(tx, ctx, n.start as i64, who, n.corp, n.flag).await?
    };
    let now = crate::bulkdata::filetime_now();
    let expires = now + n.expiry * 600_000_000;
    let r=tx.tx().query_one(r#"INSERT INTO contracts (type,"issuerID","issuerCorpID","forCorp",availability,"assigneeID","dateIssued","dateExpired","numDays","startStationID","startSolarSystemID","startRegionID","endStationID","endSolarSystemID","endRegionID",price,reward,collateral,title,description,"issuerAllianceID","sourceLocation","sourceFlag") VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23) RETURNING "contractID""#,&[&n.kind,&(ctx.session.char_id.unwrap_or(0) as i32),&(ctx.session.corp_id.unwrap_or(0) as i32),&n.corp,&n.availability,&n.assignee,&now,&expires,&n.days,&n.start,&start_system,&start_region,&n.end,&end_system,&end_region,&n.price,&n.reward,&n.collateral,&n.title,&n.description,&(ctx.session.alliance_id.unwrap_or(0) as i32),&(source as i32),&(n.flag as i32)]).await?;
    let id = r.get::<_, i32>(0) as i64;
    let mut volume = 0.0;
    for (item_id, qty) in n.items {
        let item = tx
            .item(item_id)
            .await?
            .ok_or_else(|| Failure::Text("An offered item no longer exists.".into()))?;
        if item.owner_id != who
            || item.location_id != source
            || item.flag != n.flag
            || ctx.session.ship_id == Some(item_id)
        {
            return refuse("An offered item is not available in this hangar.");
        }
        let item_id = portion(tx, &item, qty).await?;
        volume += item.volume * qty as f64;
        snapshot(tx, id, item_id, 0).await?;
        let children = descendants(tx, item_id).await?;
        for child in children {
            let item = tx
                .item(child)
                .await?
                .ok_or_else(|| Failure::Text("An item in that container is missing.".into()))?;
            if item.owner_id != who {
                return refuse("That container holds goods belonging to another owner.");
            }
            snapshot(tx, id, child, item.location_id).await?;
            tx.set_owner(child, SYSTEM).await?;
        }
        tx.set_owner(item_id, SYSTEM).await?;
        tx.move_to(item_id, id, 4).await?;
    }
    if n.kind == 3 && volume > 120_000.0 {
        return refuse("Courier packages cannot exceed 120,000 cubic metres.");
    }
    for (ty, qty) in n.requests {
        if tx
            .tx()
            .query_opt(
                r#"SELECT "typeID" FROM "invTypes" WHERE "typeID"=$1"#,
                &[&(ty as i32)],
            )
            .await?
            .is_none()
        {
            return refuse("A requested item type does not exist.");
        }
        tx.tx().execute(r#"INSERT INTO contract_items ("contractID","itemID","typeID",quantity,"inCrate") VALUES ($1,0,$2,$3,false)"#,&[&(id as i32),&(ty as i32),&(qty as i32)]).await?;
    }
    tx.tx()
        .execute(
            r#"UPDATE contracts SET volume=$2 WHERE "contractID"=$1"#,
            &[&(id as i32), &volume],
        )
        .await?;
    money(tx, fx, who, -n.reward, id, 83).await?;
    money(tx, fx, who, -fees.broker, id, 72).await?;
    money(tx, fx, who, -fees.deposit, id, 74).await?;
    tx.tx().execute(r#"UPDATE contracts SET deposit=$2,"salesTax"=$3,"taxRate"=$4,"taxCap"=$5 WHERE "contractID"=$1"#,&[&(id as i32),&fees.deposit,&fees.tax,&fees.rate,&fees.cap]).await?;
    fx.owners.insert(who);
    if n.assignee > 0 {
        fx.owners.insert(n.assignee as i64);
        fx.event = Some(("OnContractAssigned", id, n.start as i64));
    }
    Ok(Value::Int(id))
}

struct Fees {
    broker: f64,
    deposit: f64,
    tax: f64,
    rate: f64,
    cap: f64,
}
// contractscommon.CalcContractFees, including its truncation before the duration multiplier.
fn fees(kind: i32, private: i32, base: f64, minutes: i64, broker: i64, accounting: i64) -> Fees {
    if private != 0 {
        return Fees {
            broker: 10_000.0,
            deposit: 0.0,
            tax: 0.0,
            rate: 0.0,
            cap: 0.0,
        };
    }
    let multiplier = if kind == 3 { 0.1 } else { 1.0 };
    let rate = (0.01 - accounting.clamp(0, 5) as f64 * 0.001) * multiplier;
    let cap = 100_000_000.0 * (1.0 - accounting.clamp(0, 5) as f64 * 0.1);
    let broker_rate = (0.004 - broker.clamp(0, 5) as f64 * 0.0002) * multiplier;
    let broker_cap = 10_000_000.0 * (1.0 - broker.clamp(0, 5) as f64 * 0.05);
    Fees {
        broker: (base * broker_rate).clamp(10_000.0, broker_cap).floor()
            * (1.0 + (minutes / 1440 - 1) as f64 * 0.05),
        deposit: (base * 0.01 * multiplier)
            .clamp(if kind == 3 { 10_000.0 } else { 100_000.0 }, 10_000_000.0)
            .floor(),
        tax: if kind == 3 {
            0.0
        } else {
            (base * rate).clamp(10_000.0, cap).floor()
        },
        rate: if kind == 3 { 0.0 } else { rate },
        cap,
    }
}
async fn settle_fees(
    tx: &ItemTxn<'_>,
    fx: &mut Effects,
    r: &Row,
    auction: Option<f64>,
) -> Result<()> {
    let id = number(r, "contractID");
    let tax = if let Some(amount) = auction {
        if r.get::<_, f64>("taxRate") > 0.0 {
            (amount * r.get::<_, f64>("taxRate"))
                .clamp(10_000.0, r.get("taxCap"))
                .floor()
        } else {
            0.0
        }
    } else {
        r.get("salesTax")
    };
    money(tx, fx, issuer(r), r.get("deposit"), id, 82).await?;
    money(tx, fx, issuer(r), -tax, id, 73).await?;
    tx.tx()
        .execute(
            r#"UPDATE contracts SET deposit=0,"salesTax"=0 WHERE "contractID"=$1"#,
            &[&(id as i32)],
        )
        .await?;
    Ok(())
}

async fn snapshot(tx: &ItemTxn<'_>, contract: i64, item: i64, parent: i64) -> Result<()> {
    tx.tx().execute(r#"INSERT INTO contract_items ("contractID","itemID","typeID",quantity,"inCrate","parentID",copy,"materialLevel","productivityLevel","bpRuns") SELECT $1,e."itemID",e."typeID",e.quantity,true,$3,b.copy,b."materialLevel",b."productivityLevel",b."licensedProductionRunsRemaining" FROM entity e LEFT JOIN "invBlueprints" b ON b."itemID"=e."itemID" WHERE e."itemID"=$2"#,&[&(contract as i32),&(item as i32),&(parent as i32)]).await?;
    Ok(())
}
async fn descendants(tx: &ItemTxn<'_>, item: i64) -> Result<Vec<i64>> {
    Ok(tx.tx().query(r#"WITH RECURSIVE children AS (SELECT "itemID" FROM entity WHERE "locationID"=$1 UNION SELECT e."itemID" FROM entity e JOIN children c ON e."locationID"=c."itemID") SELECT "itemID" FROM children ORDER BY "itemID""#,&[&(item as i32)]).await?.iter().map(|r|r.get::<_,i32>(0) as i64).collect())
}
async fn give_goods(
    tx: &mut ItemTxn<'_>,
    id: i64,
    who: i64,
    location: i64,
    flag: i64,
) -> Result<()> {
    let rs=tx.tx().query(r#"SELECT "itemID","parentID" FROM contract_items WHERE "contractID"=$1 AND "inCrate" ORDER BY "itemID""#,&[&(id as i32)]).await?;
    for r in rs {
        let item = r.get::<_, i32>(0) as i64;
        tx.set_owner(item, who).await?;
        if r.get::<_, i32>(1) == 0 {
            tx.move_to(item, location, flag).await?;
        }
    }
    Ok(())
}
async fn split(call: &Call, ctx: &Ctx<'_>, tx: &mut ItemTxn<'_>) -> Result<Value> {
    let corp = flag(call, "forCorp", 3);
    let who = owner(ctx, corp)?;
    let flag = call.arg_i64(4).unwrap_or(4);
    let location = source(tx, ctx, call.arg_id(0), who, corp, flag).await?;
    let item = tx
        .item(call.arg_id(1))
        .await?
        .ok_or_else(|| Failure::Text("That item no longer exists.".into()))?;
    if item.owner_id != who
        || item.location_id != location
        || item.flag != flag
        || item.singleton
        || call.arg_id(2) >= item.quantity
    {
        return refuse("That stack cannot be split.");
    }
    Ok(Value::Int(portion(tx, &item, call.arg_id(2)).await?))
}

fn may_accept(ctx: &Ctx<'_>, r: &Row, who: i64) -> Result<()> {
    if number(r, "status") != 0 || r.get::<_, i64>("dateExpired") <= crate::bulkdata::filetime_now()
    {
        return refuse("This contract is no longer available.");
    }
    if issuer(r) == who {
        return refuse("You cannot accept your own contract.");
    }
    let assigned = number(r, "assigneeID");
    if number(r, "availability") != 0
        && assigned != who
        && Some(assigned) != ctx.session.corp_id
        && Some(assigned) != ctx.session.alliance_id
    {
        return refuse("This private contract is assigned to somebody else.");
    }
    Ok(())
}
async fn status(tx: &ItemTxn<'_>, id: i64, state: i32) -> Result<()> {
    let completed = if state >= 4 {
        crate::bulkdata::filetime_now()
    } else {
        0
    };
    tx.tx()
        .execute(
            r#"UPDATE contracts SET status=$2,"dateCompleted"=$3 WHERE "contractID"=$1"#,
            &[&(id as i32), &state, &completed],
        )
        .await?;
    Ok(())
}
async fn accept(
    call: &Call,
    ctx: &Ctx<'_>,
    tx: &mut ItemTxn<'_>,
    fx: &mut Effects,
    r: &Row,
) -> Result<()> {
    let corp = flag(call, "forCorp", 1);
    let who = owner(ctx, corp)?;
    may_accept(ctx, r, who)?;
    let id = number(r, "contractID");
    let seller = issuer(r);
    let kind = number(r, "type");
    let start = number(r, "startStationID");
    if kind == 2 {
        return refuse("Place a bid to accept an auction.");
    }
    if kind == 1 {
        let requests=tx.tx().query(r#"SELECT "typeID",quantity FROM contract_items WHERE "contractID"=$1 AND NOT "inCrate" ORDER BY "typeID""#,&[&(id as i32)]).await?;
        if corp && !requests.is_empty() && ctx.session.corp_role() & DELIVERIES_ROLES == 0 {
            return refuse("You cannot take the requested items from corporation deliveries.");
        }
        for req in requests {
            let ty = req.get::<_, i32>(0);
            let mut remaining = req.get::<_, i32>(1) as i64;
            // Requested goods come from the accepting owner's station hangar/deliveries.
            let rs=tx.tx().query(r#"SELECT "itemID" FROM entity WHERE "ownerID"=$1 AND "locationID"=$2 AND flag=$3 AND "typeID"=$4 AND singleton=0 ORDER BY "itemID" FOR UPDATE"#,&[&(who as i32),&(start as i32),&(delivery(corp) as i32),&ty]).await?;
            for row in rs {
                if remaining == 0 {
                    break;
                }
                let item = tx.item(row.get::<_, i32>(0) as i64).await?.ok_or_else(|| {
                    Failure::Text("A requested item is no longer available.".into())
                })?;
                let qty = remaining.min(item.quantity);
                let item_id = portion(tx, &item, qty).await?;
                tx.set_owner(item_id, seller).await?;
                tx.move_to(item_id, start, delivery(r.get("forCorp")))
                    .await?;
                remaining -= qty;
            }
            if remaining > 0 {
                return refuse(
                    "The requested goods are not available in your hangar at the pickup station.",
                );
            }
        }
    }
    let price = r.get::<_, f64>("price");
    let collateral = r.get::<_, f64>("collateral");
    if kind == 1 || kind == 4 {
        money(tx, fx, who, -price, id, 71).await?;
        money(tx, fx, seller, price, id, 71).await?;
    }
    if [3, 4, 5].contains(&kind) {
        money(tx, fx, who, -collateral, id, 65).await?;
    }
    if kind == 1 || kind == 4 {
        money(tx, fx, who, r.get("reward"), id, 68).await?;
    }
    if kind == 1 {
        settle_fees(tx, fx, r, None).await?;
    }
    if kind == 3 {
        let crate_id = tx
            .create(NewItem {
                type_id: super::missions::TYPE_PLASTIC_WRAP,
                owner_id: who,
                location_id: start,
                flag: delivery(corp),
                quantity: 1,
                singleton: true,
            })
            .await?;
        give_goods(tx, id, who, crate_id, 5).await?;
        tx.tx()
            .execute(
                r#"UPDATE contracts SET "crateID"=$2 WHERE "contractID"=$1"#,
                &[&(id as i32), &(crate_id as i32)],
            )
            .await?;
    } else if kind != 5 {
        give_goods(tx, id, who, start, delivery(corp)).await?;
    }
    tx.tx().execute(r#"UPDATE contracts SET "acceptorID"=$2,"acceptorCorp"=$3,"dateAccepted"=$4 WHERE "contractID"=$1"#,&[&(id as i32),&(who as i32),&corp,&crate::bulkdata::filetime_now()]).await?;
    status(tx, id, if kind == 1 { 4 } else { 1 }).await?;
    fx.owners.insert(who);
    fx.event = Some(("OnContractAccepted", id, start));
    Ok(())
}

fn next_bid(price: f64, current: Option<f64>, buyout: f64) -> f64 {
    let bid = price.max(current.unwrap_or(0.0) + (0.1 * price).floor().max(1000.0));
    if buyout > 0.0 {
        bid.min(buyout)
    } else {
        bid
    }
}
async fn bid(
    call: &Call,
    ctx: &Ctx<'_>,
    tx: &mut ItemTxn<'_>,
    fx: &mut Effects,
    r: &Row,
) -> Result<()> {
    let corp = flag(call, "forCorp", 2);
    let who = owner(ctx, corp)?;
    may_accept(ctx, r, who)?;
    if number(r, "type") != 2 {
        return refuse("Only auctions accept bids.");
    }
    let id = number(r, "contractID");
    let amount = call.args.get(1).and_then(Value::as_f64).unwrap_or(f64::NAN);
    let previous=tx.tx().query_opt(r#"SELECT "bidderID",amount FROM contract_bids WHERE "contractID"=$1 ORDER BY id DESC LIMIT 1"#,&[&(id as i32)]).await?;
    let buyout = r.get::<_, f64>("collateral");
    let minimum = next_bid(r.get("price"), previous.as_ref().map(|r| r.get(1)), buyout);
    if !amount.is_finite() || amount < minimum || amount > 1e12 || (buyout > 0.0 && amount > buyout)
    {
        return refuse("The bid is below the minimum or above the buyout price. Refresh the contract and try again.");
    }
    if let Some(old) = previous {
        let old_owner = old.get::<_, i32>(0) as i64;
        money(tx, fx, old_owner, old.get(1), id, 64).await?;
        fx.owners.insert(old_owner);
        fx.event = Some(("OnContractOutbid", id, number(r, "startStationID")));
    }
    money(tx, fx, who, -amount, id, 63).await?;
    let now = crate::bulkdata::filetime_now();
    tx.tx().execute(r#"INSERT INTO contract_bids ("contractID","bidderID",amount,"dateBid","forCorp") VALUES ($1,$2,$3,$4,$5)"#,&[&(id as i32),&(who as i32),&amount,&now,&corp]).await?;
    tx.tx()
        .execute(
            r#"DELETE FROM contract_notifications WHERE "contractID"=$1"#,
            &[&(id as i32)],
        )
        .await?;
    if buyout > 0.0 && amount >= buyout {
        tx.tx()
            .execute(
                r#"UPDATE contracts SET "dateExpired"=$2 WHERE "contractID"=$1"#,
                &[&(id as i32), &now],
            )
            .await?;
        fx.event = Some(("OnContractBuyout", id, number(r, "startStationID")));
    }
    Ok(())
}
async fn finish_auction(
    call: &Call,
    ctx: &Ctx<'_>,
    tx: &mut ItemTxn<'_>,
    fx: &mut Effects,
    r: &Row,
) -> Result<()> {
    let id = number(r, "contractID");
    let state = number(r, "status");
    if number(r, "type") != 2
        || ![0, 2, 3].contains(&state)
        || r.get::<_, i64>("dateExpired") > crate::bulkdata::filetime_now()
    {
        return refuse("This auction is not ready to settle.");
    }
    let b=tx.tx().query_opt(r#"SELECT "bidderID",amount,"forCorp" FROM contract_bids WHERE "contractID"=$1 ORDER BY id DESC LIMIT 1"#,&[&(id as i32)]).await?.ok_or_else(||Failure::Text("This auction has no bids; the issuer may cancel it to reclaim the items.".into()))?;
    let winner = b.get::<_, i32>(0) as i64;
    let is_issuer = flag(call, "isIssuer", 1);
    if is_issuer {
        if owner(ctx, r.get("forCorp"))? != issuer(r) || state == 2 {
            return refuse("You cannot collect this auction's money.");
        }
        money(tx, fx, issuer(r), b.get(1), id, 67).await?;
        settle_fees(tx, fx, r, Some(b.get(1))).await?;
    } else {
        if owner(ctx, b.get("forCorp"))? != winner || state == 3 {
            return refuse("Only the winning bidder can collect these items.");
        }
        give_goods(
            tx,
            id,
            winner,
            number(r, "startStationID"),
            delivery(b.get("forCorp")),
        )
        .await?;
    }
    tx.tx().execute(r#"UPDATE contracts SET "acceptorID"=$2,"acceptorCorp"=$3,"dateAccepted"=$4 WHERE "contractID"=$1"#,&[&(id as i32),&(winner as i32),&b.get::<_,bool>("forCorp"),&crate::bulkdata::filetime_now()]).await?;
    status(
        tx,
        id,
        if state != 0 {
            4
        } else if is_issuer {
            2
        } else {
            3
        },
    )
    .await?;
    fx.owners.insert(winner);
    fx.event = Some(("OnContractCompleted", id, number(r, "startStationID")));
    Ok(())
}

async fn complete(
    call: &Call,
    ctx: &Ctx<'_>,
    tx: &mut ItemTxn<'_>,
    fx: &mut Effects,
    r: &Row,
) -> Result<()> {
    let id = number(r, "contractID");
    let seller = issuer(r);
    let state = number(r, "status");
    let kind = number(r, "type");
    let me = ctx.session.char_id.unwrap_or(0);
    let is_seller = if r.get("forCorp") {
        ctx.session.corp_id == Some(seller) && owner(ctx, true).is_ok()
    } else {
        me == seller
    };
    let buyer = number(r, "acceptorID");
    let is_buyer = if r.get("acceptorCorp") {
        ctx.session.corp_id == Some(buyer) && owner(ctx, true).is_ok()
    } else {
        me == buyer
    };
    let deleting = call.method == "DeleteContract";
    let requested = if deleting { 5 } else { call.arg_id(1) };
    let start = number(r, "startStationID");
    if deleting || requested == 6 {
        if ![0, 6].contains(&state) {
            return refuse("An accepted or completed contract cannot be cancelled.");
        }
        if deleting && !is_seller {
            return refuse("Only the issuer may delete this contract.");
        }
        if requested == 6 {
            let assignee = number(r, "assigneeID");
            if state != 0
                || !(assignee == me
                    || (ctx.session.corp_id == Some(assignee) && owner(ctx, true).is_ok()))
            {
                return refuse(
                    "Only the assigned character or corporation may reject this contract.",
                );
            }
        }
        let bids: i64 = tx
            .tx()
            .query_one(
                r#"SELECT count(*) FROM contract_bids WHERE "contractID"=$1"#,
                &[&(id as i32)],
            )
            .await?
            .get(0);
        if bids > 0 {
            return refuse("An auction with bids cannot be cancelled or rejected.");
        }
        if state == 0 {
            give_goods(
                tx,
                id,
                seller,
                number(r, "sourceLocation"),
                number(r, "sourceFlag"),
            )
            .await?;
            money(tx, fx, seller, r.get("reward"), id, 66).await?;
        }
        status(tx, id, requested as i32).await?;
        if deleting {
            tx.tx()
                .execute(
                    r#"UPDATE contracts SET deleted=true WHERE "contractID"=$1"#,
                    &[&(id as i32)],
                )
                .await?;
        }
        fx.event = Some(("OnContractRejected", id, start));
        return Ok(());
    }
    if ![1, 3].contains(&state) || ![3, 4, 5].contains(&kind) || !(is_seller || is_buyer) {
        return refuse("You cannot complete or fail this contract.");
    }
    if requested == 7 {
        let overdue = r.get::<_, i64>("dateAccepted") + number(r, "numDays") * DAY
            <= crate::bulkdata::filetime_now();
        if is_seller && !is_buyer && !overdue && !(kind == 5 && state == 3) {
            return refuse("The contractor still has time to complete this contract.");
        }
        if kind != 4 {
            money(tx, fx, seller, r.get::<_, f64>("reward"), id, 66).await?;
        }
        money(tx, fx, seller, r.get::<_, f64>("collateral"), id, 70).await?;
        status(tx, id, 7).await?;
        fx.event = Some(("OnContractFailed", id, start));
        return Ok(());
    }
    if requested != 4 {
        return refuse("Invalid contract completion status.");
    }
    if kind == 5 && is_buyer && !is_seller {
        if state != 1 {
            return refuse("The issuer must confirm that the work is complete.");
        }
        status(tx, id, 3).await?;
        fx.event = Some(("OnContractCompleted", id, start));
        return Ok(());
    }
    if kind != 5 && !is_buyer {
        return refuse("Only the accepting contractor can deliver these goods.");
    }
    if kind == 3 {
        let crate_id = number(r, "crateID");
        let dest = number(r, "endStationID");
        let package = tx.item(crate_id).await?.ok_or_else(|| {
            Failure::Text("The courier package is missing or has been destroyed.".into())
        })?;
        let in_ship = ctx.session.station_id == Some(dest)
            && ctx.session.ship_id == Some(package.location_id)
            && package.flag == 5;
        if package.owner_id != buyer || (package.location_id != dest && !in_ship) {
            return refuse("Deliver the courier package to the destination station first.");
        }
        // Every original item and its quantity must still be sealed in the package.
        let rs=tx.tx().query(r#"SELECT "itemID","parentID",quantity FROM contract_items WHERE "contractID"=$1 AND "inCrate" ORDER BY "itemID""#,&[&(id as i32)]).await?;
        let expected: HashSet<i64> = rs.iter().map(|row| row.get::<_, i32>(0) as i64).collect();
        let contents: HashSet<i64> = descendants(tx, crate_id).await?.into_iter().collect();
        if contents != expected {
            return refuse("The courier package has been opened or damaged.");
        }
        for row in rs {
            let item = tx.item(row.get::<_, i32>(0) as i64).await?.ok_or_else(|| {
                Failure::Text("The courier package has been opened or damaged.".into())
            })?;
            let parent = row.get::<_, i32>(1) as i64;
            let expected = if parent == 0 { crate_id } else { parent };
            if item.owner_id != buyer
                || item.location_id != expected
                || item.quantity != row.get::<_, i32>(2) as i64
            {
                return refuse("The courier package has been opened or damaged.");
            }
        }
        give_goods(tx, id, seller, dest, delivery(r.get("forCorp"))).await?;
        tx.destroy(crate_id).await?;
    }
    if kind == 4 {
        let rs=tx.tx().query(r#"SELECT "itemID",quantity,"parentID" FROM contract_items WHERE "contractID"=$1 AND "inCrate" ORDER BY "itemID""#,&[&(id as i32)]).await?;
        let expected: HashSet<i64> = rs.iter().map(|row| row.get::<_, i32>(0) as i64).collect();
        for root in rs.iter().filter(|row| row.get::<_, i32>(2) == 0) {
            let contents = descendants(tx, root.get::<_, i32>(0) as i64).await?;
            if contents.iter().any(|item| !expected.contains(item)) {
                return refuse(
                    "Remove your additional cargo and fittings before returning the loaned items.",
                );
            }
        }
        for row in rs {
            let item = tx
                .item(row.get::<_, i32>(0) as i64)
                .await?
                .ok_or_else(|| Failure::Text("A loaned item has not been returned.".into()))?;
            let parent = row.get::<_, i32>(2) as i64;
            if item.owner_id != buyer
                || item.quantity < row.get::<_, i32>(1) as i64
                || item.location_id != if parent == 0 { start } else { parent }
                || ctx.session.ship_id == Some(item.id)
            {
                return refuse("Return the loaned items to the original station hangar first.");
            }
            let promised = row.get::<_, i32>(1) as i64;
            if item.quantity > promised {
                let remainder = portion(tx, &item, item.quantity - promised).await?;
                tx.move_to(remainder, start, delivery(r.get("acceptorCorp")))
                    .await?;
            }
        }
        give_goods(
            tx,
            id,
            seller,
            number(r, "sourceLocation"),
            number(r, "sourceFlag"),
        )
        .await?;
    }
    if kind == 4 {
        money(tx, fx, buyer, -r.get::<_, f64>("reward"), id, 71).await?;
        money(tx, fx, seller, r.get("reward"), id, 71).await?;
    } else {
        money(tx, fx, buyer, r.get("reward"), id, 68).await?;
    }
    money(tx, fx, buyer, r.get("collateral"), id, 69).await?;
    settle_fees(tx, fx, r, None).await?;
    status(tx, id, 4).await?;
    fx.owners.insert(buyer);
    fx.event = Some(("OnContractCompleted", id, start));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{session::Session, test_support::Database};

    fn call(method: &str, args: Vec<Value>) -> Call {
        Call {
            service: "contractMgr".into(),
            method: method.into(),
            args,
            kwargs: vec![],
        }
    }
    fn create_call(kind: i64, item: i64, assignee: i64) -> Call {
        let mut c = call(
            "CreateContract",
            vec![
                Value::Int(kind),
                Value::Int(i64::from(assignee > 0)),
                Value::Int(assignee),
                Value::Int(1440),
                Value::Int(if kind >= 3 { 1 } else { 0 }),
                Value::Int(if kind == 5 { 0 } else { 60000004 }),
                Value::Int(if kind == 3 { 60000007 } else { 0 }),
                Value::Double(if kind == 1 || kind == 2 { 1000.0 } else { 0.0 }),
                Value::Double(if kind >= 3 { 100.0 } else { 0.0 }),
                Value::Double(if kind >= 3 { 500.0 } else { 0.0 }),
                Value::bytes("Test contract"),
                Value::bytes("Integration test"),
            ],
        );
        c.kwargs.push((
            Value::bytes("itemList"),
            Value::List(if item > 0 {
                vec![Value::List(vec![Value::Int(item), Value::Int(3)])]
            } else {
                vec![]
            }),
        ));
        c
    }
    async fn pilot(db: &Database) -> Session {
        let id = db.id().await;
        db.db.execute(r#"INSERT INTO "character_" ("characterID","characterName",balance,"corporationID","stationID") VALUES ($1,$2,10000000,1000002,60000004)"#,&[&id,&format!("Contracts fixture {id}")]).await.unwrap();
        Session {
            char_id: Some(id as i64),
            corp_id: Some(1000002),
            station_id: Some(60000004),
            solar_system_id2: Some(30000142),
            region_id: Some(10000002),
            ..Default::default()
        }
    }
    async fn mineral(db: &Database, s: &Session) -> i64 {
        db.db.query_one(r#"INSERT INTO entity ("typeID","ownerID","locationID",flag,quantity) VALUES (34,$1,60000004,4,10) RETURNING "itemID""#,&[&(s.char_id.unwrap() as i32)]).await.unwrap().get::<_,i32>(0) as i64
    }
    async fn balance(db: &Database, s: &Session) -> f64 {
        db.db
            .query_one(
                r#"SELECT balance FROM "character_" WHERE "characterID"=$1"#,
                &[&(s.char_id.unwrap() as i32)],
            )
            .await
            .unwrap()
            .get(0)
    }
    async fn state(db: &Database, id: i64) -> i32 {
        db.db
            .query_one(
                r#"SELECT status FROM contracts WHERE "contractID"=$1"#,
                &[&(id as i32)],
            )
            .await
            .unwrap()
            .get(0)
    }
    fn count_rows(v: &Value) -> usize {
        field(v, "lines").unwrap().seq().unwrap().len()
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn database_exchange_escrow_atomicity_privacy_and_cancellation() {
        let db = Database::load().await;
        let a = pilot(&db).await;
        let b = pilot(&db).await;
        let stranger = pilot(&db).await;
        let item = mineral(&db, &a).await;
        let id = write(&create_call(1, item, b.char_id.unwrap()), &db.ctx(&a))
            .await
            .unwrap()
            .as_i64()
            .unwrap();
        let detail = detail(&db.ctx(&b), id).await.unwrap();
        assert_eq!(count_rows(field(&detail, "items").unwrap()), 1);
        assert_eq!(
            super::detail(&db.ctx(&stranger), id).await.unwrap(),
            Value::None
        );
        assert!(write(
            &call("AcceptContract", vec![Value::Int(id), Value::Bool(false)]),
            &db.ctx(&stranger)
        )
        .await
        .is_err());
        assert_eq!(state(&db, id).await, 0);
        db.db
            .execute(
                r#"UPDATE "character_" SET balance=0 WHERE "characterID"=$1"#,
                &[&(b.char_id.unwrap() as i32)],
            )
            .await
            .unwrap();
        assert!(write(
            &call("AcceptContract", vec![Value::Int(id), Value::Bool(false)]),
            &db.ctx(&b)
        )
        .await
        .is_err());
        assert_eq!(state(&db, id).await, 0);
        db.db
            .execute(
                r#"UPDATE "character_" SET balance=10000000 WHERE "characterID"=$1"#,
                &[&(b.char_id.unwrap() as i32)],
            )
            .await
            .unwrap();
        let before = balance(&db, &a).await;
        write(
            &call("AcceptContract", vec![Value::Int(id), Value::Bool(false)]),
            &db.ctx(&b),
        )
        .await
        .unwrap();
        assert_eq!(state(&db, id).await, 4);
        assert_eq!(balance(&db, &a).await, before + 1000.0);
        assert_eq!(balance(&db, &b).await, 9_999_000.0);
        assert!(write(
            &call("AcceptContract", vec![Value::Int(id), Value::Bool(false)]),
            &db.ctx(&b)
        )
        .await
        .is_err());
        let delivered:i64=db.db.query_one(r#"SELECT sum(quantity)::bigint FROM entity WHERE "ownerID"=$1 AND "typeID"=34 AND "locationID"=60000004"#,&[&(b.char_id.unwrap() as i32)]).await.unwrap().get(0);
        assert_eq!(delivered, 3);
        let cancel = write(&create_call(1, item, b.char_id.unwrap()), &db.ctx(&a))
            .await
            .unwrap()
            .as_i64()
            .unwrap();
        write(
            &call("DeleteContract", vec![Value::Int(cancel)]),
            &db.ctx(&a),
        )
        .await
        .unwrap();
        assert_eq!(state(&db, cancel).await, 5);
        let returned:i64=db.db.query_one(r#"SELECT sum(quantity)::bigint FROM entity WHERE "ownerID"=$1 AND "typeID"=34 AND "locationID"=60000004"#,&[&(a.char_id.unwrap() as i32)]).await.unwrap().get(0);
        assert_eq!(returned, 7);
        for method in [
            "NumRequiringAttention",
            "NumOutstandingContracts",
            "CollectMyPageInfo",
        ] {
            counters(&call(method, vec![]), &db.ctx(&a)).await.unwrap();
        }
        for method in [
            "GetContractList",
            "GetMyBids",
            "GetMyExpiredContractList",
            "GetMyCurrentContractList",
        ] {
            list(&call(method, vec![]), &db.ctx(&a)).await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE disposable imported database"]
    async fn database_auctions_courier_loans_and_freeform_settle_once() {
        let db = Database::load().await;
        let a = pilot(&db).await;
        let mut b = pilot(&db).await;
        let c = pilot(&db).await;
        let auction = write(&create_call(2, mineral(&db, &a).await, 0), &db.ctx(&a))
            .await
            .unwrap()
            .as_i64()
            .unwrap();
        write(
            &call(
                "PlaceBid",
                vec![
                    Value::Int(auction),
                    Value::Double(1000.0),
                    Value::Bool(false),
                ],
            ),
            &db.ctx(&b),
        )
        .await
        .unwrap();
        write(
            &call(
                "PlaceBid",
                vec![
                    Value::Int(auction),
                    Value::Double(2000.0),
                    Value::Bool(false),
                ],
            ),
            &db.ctx(&c),
        )
        .await
        .unwrap();
        assert_eq!(balance(&db, &b).await, 10_000_000.0);
        assert_eq!(balance(&db, &c).await, 9_998_000.0);
        let attention = counters(&call("NumRequiringAttention", vec![]), &db.ctx(&b))
            .await
            .unwrap();
        assert_eq!(field(&attention, "n").and_then(Value::as_i64), Some(1));
        let outstanding = list(
            &call("GetMyExpiredContractList", vec![Value::Bool(false)]),
            &db.ctx(&b),
        )
        .await
        .unwrap();
        assert_eq!(count_rows(field(&outstanding, "contracts").unwrap()), 1);
        db.db
            .execute(
                r#"UPDATE contracts SET "dateExpired"=0 WHERE "contractID"=$1"#,
                &[&(auction as i32)],
            )
            .await
            .unwrap();
        write(
            &call(
                "FinishAuction",
                vec![Value::Int(auction), Value::Bool(false)],
            ),
            &db.ctx(&c),
        )
        .await
        .unwrap();
        assert_eq!(state(&db, auction).await, 3);
        assert!(write(
            &call(
                "FinishAuction",
                vec![Value::Int(auction), Value::Bool(false)]
            ),
            &db.ctx(&c)
        )
        .await
        .is_err());
        write(
            &call(
                "FinishAuction",
                vec![Value::Int(auction), Value::Bool(true)],
            ),
            &db.ctx(&a),
        )
        .await
        .unwrap();
        assert_eq!(state(&db, auction).await, 4);
        for kind in [3, 4, 5] {
            let item = if kind == 5 { 0 } else { mineral(&db, &a).await };
            let id = write(&create_call(kind, item, b.char_id.unwrap()), &db.ctx(&a))
                .await
                .unwrap()
                .as_i64()
                .unwrap();
            let before = balance(&db, &b).await;
            write(
                &call("AcceptContract", vec![Value::Int(id), Value::Bool(false)]),
                &db.ctx(&b),
            )
            .await
            .unwrap();
            assert_eq!(
                balance(&db, &b).await,
                before - if kind == 4 { 400.0 } else { 500.0 }
            );
            if kind == 3 {
                assert!(write(
                    &call("CompleteContract", vec![Value::Int(id), Value::Int(4)]),
                    &db.ctx(&b)
                )
                .await
                .is_err());
                db.db.execute(r#"UPDATE entity SET "locationID"=60000007 WHERE "itemID"=(SELECT "crateID" FROM contracts WHERE "contractID"=$1)"#,&[&(id as i32)]).await.unwrap();
                b.station_id = Some(60000007);
                let extra = db.db.query_one(r#"INSERT INTO entity ("typeID","ownerID","locationID",flag,quantity) SELECT 34,$2,"crateID",5,1 FROM contracts WHERE "contractID"=$1 RETURNING "itemID""#,&[&(id as i32),&(b.char_id.unwrap() as i32)]).await.unwrap().get::<_,i32>(0);
                assert!(write(
                    &call("CompleteContract", vec![Value::Int(id), Value::Int(4)]),
                    &db.ctx(&b)
                )
                .await
                .is_err());
                assert_eq!(state(&db, id).await, 1);
                db.db
                    .execute(
                        r#"UPDATE entity SET "locationID"=60000007,flag=4 WHERE "itemID"=$1"#,
                        &[&extra],
                    )
                    .await
                    .unwrap();
            }
            if kind == 4 {
                db.db.execute(r#"UPDATE entity SET quantity=quantity+2 WHERE "itemID" IN (SELECT "itemID" FROM contract_items WHERE "contractID"=$1 AND "inCrate")"#,&[&(id as i32)]).await.unwrap();
            }
            write(
                &call("CompleteContract", vec![Value::Int(id), Value::Int(4)]),
                &db.ctx(&b),
            )
            .await
            .unwrap();
            if kind == 5 {
                assert_eq!(state(&db, id).await, 3);
                write(
                    &call("CompleteContract", vec![Value::Int(id), Value::Int(4)]),
                    &db.ctx(&a),
                )
                .await
                .unwrap();
            }
            assert_eq!(state(&db, id).await, 4);
            if kind == 4 {
                let returned=db.db.query_one(r#"SELECT e.quantity,e."ownerID" FROM entity e JOIN contract_items ci USING ("itemID") WHERE ci."contractID"=$1 AND ci."inCrate""#,&[&(id as i32)]).await.unwrap();
                assert_eq!(returned.get::<_, i32>(0), 3);
                assert_eq!(returned.get::<_, i32>(1), a.char_id.unwrap() as i32);
                let remaining:i64=db.db.query_one(r#"SELECT COALESCE(sum(quantity),0)::bigint FROM entity WHERE "ownerID"=$1 AND "typeID"=34 AND "locationID"=60000004"#,&[&(b.char_id.unwrap() as i32)]).await.unwrap().get(0);
                assert_eq!(remaining, 2);
            }
            assert_eq!(
                balance(&db, &b).await,
                before + if kind == 4 { 0.0 } else { 100.0 }
            );
            assert!(write(
                &call("CompleteContract", vec![Value::Int(id), Value::Int(4)]),
                &db.ctx(&b)
            )
            .await
            .is_err());
        }
    }
    #[test]
    fn item_lists_reject_duplicates_invalid_quantities_and_overflow() {
        let list = |pairs: &[(i64, i64)]| {
            Value::List(
                pairs
                    .iter()
                    .map(|(id, q)| Value::List(vec![Value::Int(*id), Value::Int(*q)]))
                    .collect(),
            )
        };
        assert_eq!(
            pairs(Some(&list(&[(7, 3), (2, 1)]))).unwrap(),
            vec![(2, 1), (7, 3)]
        );
        for bad in [
            vec![(1, 1), (1, 2)],
            vec![(1, 0)],
            vec![(1, -1)],
            vec![(i32::MAX as i64 + 1, 1)],
            vec![(1, i32::MAX as i64 + 1)],
        ] {
            assert!(pairs(Some(&list(&bad))).is_err());
        }
    }
    #[test]
    fn auction_increment_matches_client_and_caps_at_buyout() {
        assert_eq!(next_bid(10_000.0, None, 0.0), 10_000.0);
        assert_eq!(next_bid(10_000.0, Some(10_000.0), 0.0), 11_000.0);
        assert_eq!(next_bid(10_000.0, Some(10_000.0), 10_500.0), 10_500.0);
        assert_eq!(next_bid(100.0, None, 0.0), 1000.0);
    }
    #[test]
    fn fees_follow_client_minima_skills_and_duration() {
        let private = fees(1, 1, 1e9, 1440, 0, 0);
        assert_eq!(private.broker, 10_000.0);
        assert_eq!(private.deposit + private.tax, 0.0);
        let small = fees(1, 0, 1000.0, 1440, 0, 0);
        assert_eq!(small.broker, 10_000.0);
        assert_eq!(small.deposit, 100_000.0);
        assert_eq!(small.tax, 10_000.0);
        let courier = fees(3, 0, 1000.0, 4320, 0, 0);
        assert_eq!(courier.deposit, 10_000.0);
        assert_eq!(courier.tax, 0.0);
        assert_eq!(courier.broker, 11_000.0);
        let skilled = fees(2, 0, 10_000_000.0, 1440, 5, 5);
        assert_eq!(skilled.broker, 30_000.0);
        assert_eq!(skilled.tax, 50_000.0);
    }
    #[test]
    fn column_positions_match_the_clients_sort_comparator() {
        for (i, name) in [
            (0, "contractID"),
            (1, "type"),
            (2, "issuerID"),
            (3, "issuerCorpID"),
            (4, "forCorp"),
            (9, "dateExpired"),
            (13, "startStationID"),
            (14, "startSolarSystemID"),
            (19, "price"),
            (20, "reward"),
            (22, "title"),
        ] {
            assert_eq!(HEADER[i], name);
        }
    }
    #[test]
    fn search_filters_read_keyval_instances() {
        let filter = key_val(vec![("type", Value::Int(3))]);
        assert_eq!(field(&filter, "type").and_then(Value::as_i64), Some(3));
    }
    #[test]
    fn malformed_and_non_finite_contracts_are_rejected() {
        let mut call = Call {
            service: "contractMgr".into(),
            method: "CreateContract".into(),
            args: vec![
                Value::Int(1),
                Value::Int(0),
                Value::Int(0),
                Value::Int(1440),
                Value::Int(0),
                Value::Int(60000001),
                Value::Int(0),
                Value::Double(0.0),
                Value::Double(0.0),
                Value::Double(0.0),
                Value::bytes("test"),
                Value::bytes(""),
            ],
            kwargs: vec![],
        };
        assert!(NewContract::parse(&call).is_ok());
        call.args[7] = Value::Double(f64::NAN);
        assert!(NewContract::parse(&call).is_err());
        call.args[7] = Value::Double(1.0);
        call.args[3] = Value::Int(i64::MAX);
        assert!(NewContract::parse(&call).is_err());
    }
}

#[derive(Debug)]
enum Failure {
    Text(String),
    Db(tokio_postgres::Error),
    Money { need: f64, have: f64 },
}
impl From<tokio_postgres::Error> for Failure {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::Db(e)
    }
}
impl From<Transfer> for Failure {
    fn from(e: Transfer) -> Self {
        match e {
            Transfer::Db(e) => Self::Db(e),
            Transfer::NotEnough { need, have } => Self::Money { need, have },
            Transfer::NoSuchOwner => Self::Text("That wallet no longer exists.".into()),
        }
    }
}
type Result<T> = std::result::Result<T, Failure>;
fn refuse<T>(text: &str) -> Result<T> {
    Err(Failure::Text(text.into()))
}
fn owner(ctx: &Ctx<'_>, corp: bool) -> Result<i64> {
    if corp
        && ctx.session.corp_role() & (CONTRACT_MANAGER | crate::session::CORP_ROLE_DIRECTOR) == 0
    {
        return refuse("You need the Contract Manager role to act for your corporation.");
    }
    if corp {
        ctx.session.corp_id
    } else {
        ctx.session.char_id
    }
    .ok_or_else(|| Failure::Text("Select a character first.".into()))
}
fn arg<'a>(call: &'a Call, name: &str, index: usize) -> Option<&'a Value> {
    call.kwarg(name).or(call.args.get(index))
}
fn flag(call: &Call, name: &str, index: usize) -> bool {
    arg(call, name, index).and_then(Value::as_i64).unwrap_or(0) != 0
}
fn field<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    match v {
        Value::Instance { state, .. } => field(state, name),
        _ => v.get_str(name),
    }
}
fn fields(header: &[&str], alias: &str) -> String {
    header
        .iter()
        .map(|h| format!("{alias}\"{h}\""))
        .collect::<Vec<_>>()
        .join(",")
}
fn number(r: &Row, name: &str) -> i64 {
    r.get::<_, i32>(name) as i64
}
fn issuer(r: &Row) -> i64 {
    number(
        r,
        if r.get("forCorp") {
            "issuerCorpID"
        } else {
            "issuerID"
        },
    )
}
fn delivery(corp: bool) -> i64 {
    if corp {
        62
    } else {
        4
    }
}

#[derive(Default)]
struct Effects {
    balances: HashMap<i64, f64>,
    owners: HashSet<i64>,
    event: Option<(&'static str, i64, i64)>,
}
async fn money(
    tx: &ItemTxn<'_>,
    fx: &mut Effects,
    who: i64,
    delta: f64,
    contract: i64,
    reference: i32,
) -> Result<()> {
    if delta == 0.0 {
        return Ok(());
    }
    let id = contract.to_string();
    let entry = Entry {
        ref_type: reference,
        payer: if delta < 0.0 { who } else { 0 },
        payee: if delta > 0.0 { who } else { 0 },
        arg_id1: &id,
        reason: "Contract",
    };
    fx.balances
        .insert(who, post(tx.tx(), who, delta, &entry).await?);
    fx.owners.insert(who);
    Ok(())
}

pub async fn contract_mgr(call: &Call, ctx: &Ctx<'_>) -> Option<Outcome> {
    let result = match call.method.as_str() {
        "GetContract" => detail(ctx, call.arg_id(0)).await,
        "GetContractList"
        | "GetContractListForOwner"
        | "GetMyExpiredContractList"
        | "GetMyCurrentContractList"
        | "GetMyBids" => list(call, ctx).await,
        "NumRequiringAttention" | "NumOutstandingContracts" | "CollectMyPageInfo" => {
            counters(call, ctx).await
        }
        "GetItemsInStation" | "GetItemsInContainer" | "HasFittedCharges" => {
            inventory(call, ctx).await
        }
        "CreateContract" | "AcceptContract" | "DeleteContract" | "CompleteContract"
        | "PlaceBid" | "FinishAuction" | "DeleteNotification" | "SplitStack" => {
            write(call, ctx).await
        }
        _ => return None,
    };
    Some(match result {
        Ok(v) => Outcome::Value(v),
        Err(Failure::Text(t)) => Outcome::custom(t),
        Err(Failure::Money { need, have }) => Outcome::not_enough_money(need, have),
        Err(Failure::Db(e)) => {
            tracing::error!("contract {}: {}", call.method, crate::logging::chain(&e));
            Outcome::custom("The contract could not be processed. Please try again.")
        }
    })
}

fn visible(ctx: &Ctx<'_>) -> String {
    format!(
        r#"(c.availability=0 OR c."assigneeID" IN ({},{},{}) OR c."issuerID"={} OR (c."forCorp" AND c."issuerCorpID"={}) OR c."acceptorID" IN ({},{}))"#,
        ctx.session.char_id.unwrap_or(-1),
        ctx.session.corp_id.unwrap_or(-1),
        ctx.session.alliance_id.unwrap_or(-1),
        ctx.session.char_id.unwrap_or(-1),
        ctx.session.corp_id.unwrap_or(-1),
        ctx.session.char_id.unwrap_or(-1),
        ctx.session.corp_id.unwrap_or(-1)
    )
}
async fn item_rows(db: &Client, id: i64) -> Result<Value> {
    Ok(rowset(ITEM_HEADER, rows::lines(&db.query(&format!("SELECT {} FROM contract_items WHERE \"contractID\"=$1 ORDER BY \"inCrate\" DESC, id", fields(ITEM_HEADER, "")), &[&(id as i32)]).await?)))
}
async fn bid_rows(db: &Client, id: i64) -> Result<Value> {
    Ok(rowset(
        BID_HEADER,
        rows::lines(
            &db.query(
                &format!(
                    "SELECT {} FROM contract_bids WHERE \"contractID\"=$1 ORDER BY id DESC",
                    fields(BID_HEADER, "")
                ),
                &[&(id as i32)],
            )
            .await?,
        ),
    ))
}
async fn detail(ctx: &Ctx<'_>, id: i64) -> Result<Value> {
    let sql = format!(
        "SELECT {} FROM contracts c WHERE c.\"contractID\"=$1 AND NOT c.deleted AND {}",
        fields(HEADER, "c."),
        visible(ctx)
    );
    let Some(r) = ctx.db.query_opt(&sql, &[&(id as i32)]).await? else {
        return Ok(Value::None);
    };
    Ok(key_val(vec![
        ("contract", row(HEADER, rows::line(&r))),
        ("items", item_rows(ctx.db, id).await?),
        ("bids", bid_rows(ctx.db, id).await?),
    ]))
}

struct Query {
    sql: String,
    params: Vec<Box<dyn ToSql + Sync + Send>>,
}
impl Query {
    fn bind(&mut self, value: impl ToSql + Sync + Send + 'static) -> String {
        self.params.push(Box::new(value));
        format!("${}", self.params.len())
    }
    fn equal(&mut self, column: &str, value: i64) {
        let p = self.bind(value as i32);
        self.sql.push_str(&format!(" AND {column}={p}"));
    }
}
async fn list(call: &Call, ctx: &Ctx<'_>) -> Result<Value> {
    let me = owner(ctx, false)?;
    let mut q = Query {
        sql: format!(
            "SELECT {} FROM contracts c WHERE NOT c.deleted AND {}",
            fields(HEADER, "c."),
            visible(ctx)
        ),
        params: vec![],
    };
    let now = crate::bulkdata::filetime_now();
    match call.method.as_str() {
        "GetContractList" => {
            let p = q.bind(now);
            q.sql
                .push_str(&format!(" AND c.status=0 AND c.\"dateExpired\">{p}"));
            if let Some(filter) = call.args.first() {
                for (name, column) in [
                    ("type", "type"),
                    ("assigneeID", "assigneeID"),
                    ("stationID", "startStationID"),
                    ("solarSystemID", "startSolarSystemID"),
                    ("regionID", "startRegionID"),
                ] {
                    if let Some(n) = field(filter, name).and_then(Value::as_i64) {
                        if n > 0 || name == "assigneeID" {
                            q.equal(&format!("c.\"{column}\""), n);
                        }
                    }
                }
                for (name, op) in [("priceMin", ">="), ("priceMax", "<=")] {
                    if let Some(n) = field(filter, name)
                        .and_then(Value::as_f64)
                        .filter(|n| n.is_finite() && *n > 0.0)
                    {
                        let p = q.bind(n);
                        q.sql.push_str(&format!(" AND c.price{op}{p}"));
                    }
                }
                if let Some(s) = field(filter, "string")
                    .and_then(Value::as_string)
                    .filter(|s| !s.is_empty())
                {
                    let p = q.bind(s.to_lowercase());
                    q.sql.push_str(&format!(
                        " AND strpos(lower(c.title || ' ' || c.description),{p})>0"
                    ));
                }
                for (name, exclude) in [("issuedByIDs", false), ("notIssuedByIDs", true)] {
                    if let Some(s) = field(filter, name).and_then(Value::as_string) {
                        let ids: Vec<i32> =
                            s.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                        if !ids.is_empty() {
                            let p = q.bind(ids);
                            q.sql.push_str(&format!(" AND {}(CASE WHEN c.\"forCorp\" THEN c.\"issuerCorpID\" ELSE c.\"issuerID\" END = ANY({p}))", if exclude {"NOT "} else {""}));
                        }
                    }
                }
                for (name, column) in [
                    ("itemTypeID", "i.\"typeID\""),
                    ("itemGroupID", "t.\"groupID\""),
                    ("itemCategoryID", "g.\"categoryID\""),
                ] {
                    if let Some(n) = field(filter, name)
                        .and_then(Value::as_i64)
                        .filter(|n| *n > 0)
                    {
                        let p = q.bind(n as i32);
                        q.sql.push_str(&format!(" AND EXISTS (SELECT 1 FROM contract_items i JOIN \"invTypes\" t ON t.\"typeID\"=i.\"typeID\" JOIN \"invGroups\" g ON g.\"groupID\"=t.\"groupID\" WHERE i.\"contractID\"=c.\"contractID\" AND {column}={p})"));
                    }
                }
            }
        }
        "GetContractListForOwner" => {
            q.equal(
                if flag(call, "isAccepted", 3) {
                    "c.\"acceptorID\""
                } else {
                    "CASE WHEN c.\"forCorp\" THEN c.\"issuerCorpID\" ELSE c.\"issuerID\" END"
                },
                call.arg_id(0),
            );
            if let Some(s) = call.arg_i64(1).filter(|s| *s >= 0) {
                q.equal("c.status", s);
            }
            if let Some(t) = call.arg_i64(2).filter(|t| *t > 0) {
                q.equal("c.type", t);
            }
        }
        method => {
            let who = owner(
                ctx,
                flag(
                    call,
                    "forCorp",
                    if method == "GetMyCurrentContractList" {
                        1
                    } else {
                        0
                    },
                ),
            )?;
            let p = q.bind(who as i32);
            match method {
                "GetMyBids" => q.sql.push_str(&format!(" AND EXISTS (SELECT 1 FROM contract_bids b WHERE b.\"contractID\"=c.\"contractID\" AND b.\"bidderID\"={p})")),
                "GetMyCurrentContractList" => { q.sql.push_str(" AND c.status IN (0,1,2,3)"); q.sql.push_str(&format!(" AND {}={p}",if flag(call,"isAccepted",0) {"c.\"acceptorID\""} else {"CASE WHEN c.\"forCorp\" THEN c.\"issuerCorpID\" ELSE c.\"issuerID\" END"})); }
                _ => {
                    let time=q.bind(now);
                    q.sql.push_str(&format!(" AND ((CASE WHEN c.\"forCorp\" THEN c.\"issuerCorpID\" ELSE c.\"issuerID\" END={p}) OR c.\"acceptorID\"={p} OR EXISTS (SELECT 1 FROM contract_bids b WHERE b.\"contractID\"=c.\"contractID\" AND b.\"bidderID\"={p})) AND ((c.type=2 AND c.status=0 AND EXISTS (SELECT 1 FROM contract_bids mine WHERE mine.\"contractID\"=c.\"contractID\" AND mine.\"bidderID\"={p}) AND (SELECT latest.\"bidderID\" FROM contract_bids latest WHERE latest.\"contractID\"=c.\"contractID\" ORDER BY latest.id DESC LIMIT 1)<>{p}) OR c.status=6 OR (c.status IN (0,2,3) AND c.\"dateExpired\"<={time}) OR (c.status=1 AND c.\"dateAccepted\"+c.\"numDays\"::bigint*{DAY}<={time})) AND NOT EXISTS (SELECT 1 FROM contract_notifications n WHERE n.\"contractID\"=c.\"contractID\" AND n.\"ownerID\"={p})"));
                }
            }
        }
    }
    if let Some(start) = call.kwarg("startContractID").and_then(Value::as_i64) {
        let p = q.bind(start as i32);
        q.sql.push_str(&format!(" AND c.\"contractID\"<={p}"));
    }
    let limit = call
        .kwarg("num")
        .and_then(Value::as_i64)
        .unwrap_or(100)
        .clamp(1, 500);
    q.sql
        .push_str(&format!(" ORDER BY c.\"contractID\" DESC LIMIT {limit}"));
    let params: Vec<&(dyn ToSql + Sync)> = q
        .params
        .iter()
        .map(|p| &**p as &(dyn ToSql + Sync))
        .collect();
    let contracts = ctx.db.query(&q.sql, &params).await?;
    let mut items = vec![];
    let mut bids = vec![];
    for r in &contracts {
        let id = number(r, "contractID");
        items.push((Value::Int(id), item_rows(ctx.db, id).await?));
        bids.push((Value::Int(id), bid_rows(ctx.db, id).await?));
    }
    let _ = me;
    Ok(key_val(vec![
        ("contracts", rowset(HEADER, rows::lines(&contracts))),
        ("items", Value::Dict(items)),
        ("bids", Value::Dict(bids)),
    ]))
}
