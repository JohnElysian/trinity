//! Corporation jump arrays. Wire shapes come from moonmining.py and
//! menusvc.dis::JumpPortalBridgeMenu/BridgePortals/JumpThroughPortal.
use super::{
    jumps,
    rows::{float, int},
};
use crate::{
    ballpark::Vec3,
    services::{Ctx, Outcome},
    session::CORP_ROLE_DIRECTOR,
};
use eve_marshal::Value;
use tokio_postgres::{Client, GenericClient};

const GROUP_ARRAY: i32 = 707;
const STARBASE_CONFIG: i64 = 1 << 53;

pub async fn ensure(db: &Client) -> Result<(), tokio_postgres::Error> {
    db.batch_execute(
        r#"CREATE TABLE IF NOT EXISTS "jumpBridgeLinks" (
        "sourceID" integer PRIMARY KEY REFERENCES entity("itemID") ON DELETE CASCADE,
        "destinationID" integer UNIQUE NOT NULL REFERENCES entity("itemID") ON DELETE CASCADE,
        CHECK ("sourceID" <> "destinationID"))"#,
    )
    .await
}

pub async fn towers(ctx: &Ctx<'_>) -> Value {
    let Some(corp) = ctx.session.corp_id else {
        return Value::List(Vec::new());
    };
    let rows = ctx
        .query(
            r#"SELECT e."typeID", e."itemID", e."locationID" FROM entity e
        JOIN "invTypes" t ON t."typeID" = e."typeID"
        JOIN "posStructures" p ON p."itemID" = e."itemID"
        JOIN "mapSolarSystems" s ON s."solarSystemID" = e."locationID"
        WHERE e."ownerID" = $1 AND t."groupID" = 365 ORDER BY e."locationID",e."itemID""#,
            &[&(corp as i32)],
        )
        .await;
    Value::List(
        rows.iter()
            .map(|row| Value::Tuple(super::rows::line(row)))
            .collect(),
    )
}

async fn arrays(
    db: &impl GenericClient,
    corp: i64,
) -> Result<Vec<tokio_postgres::Row>, tokio_postgres::Error> {
    db.query(
        r#"SELECT e."locationID",e."itemID", remote."locationID",remote."itemID",remote."typeID"
        FROM entity e JOIN "invTypes" t ON t."typeID"=e."typeID"
        JOIN "posStructures" p ON p."itemID"=e."itemID"
        JOIN "mapSolarSystems" s ON s."solarSystemID"=e."locationID"
        LEFT JOIN "jumpBridgeLinks" link ON link."sourceID"=e."itemID"
        LEFT JOIN entity remote ON remote."itemID"=link."destinationID"
        WHERE e."ownerID"=$1 AND t."groupID"=$2 ORDER BY e."locationID",e."itemID""#,
        &[&(corp as i32), &GROUP_ARRAY],
    )
    .await
}

pub async fn list(ctx: &Ctx<'_>) -> Value {
    let Some(corp) = ctx.session.corp_id else {
        return Value::List(Vec::new());
    };
    match arrays(ctx.db, corp).await {
        Ok(rows) => Value::List(
            rows.iter()
                .map(|row| {
                    let link = match (int(row, 2), int(row, 3), int(row, 4)) {
                        (Some(system), Some(structure), Some(type_id)) => vec![(
                            Value::Int(int(row, 1).unwrap()),
                            Value::Tuple(vec![
                                Value::Int(system),
                                Value::Int(structure),
                                Value::Int(type_id),
                            ]),
                        )],
                        _ => Vec::new(),
                    };
                    Value::Tuple(vec![Value::Int(int(row, 0).unwrap()), Value::Dict(link)])
                })
                .collect(),
        ),
        Err(error) => {
            tracing::error!("jump array listing: {}", crate::logging::chain(&error));
            Value::List(Vec::new())
        }
    }
}

pub async fn linkable(ctx: &Ctx<'_>) -> Value {
    let Some(corp) = ctx.session.corp_id else {
        return Value::List(Vec::new());
    };
    match arrays(ctx.db, corp).await {
        Ok(rows) => Value::List(
            rows.iter()
                .filter(|row| int(row, 3).is_none())
                .map(|row| {
                    Value::Tuple(vec![
                        Value::Int(int(row, 0).unwrap()),
                        Value::Int(int(row, 1).unwrap()),
                    ])
                })
                .collect(),
        ),
        Err(error) => {
            tracing::error!("linkable jump arrays: {}", crate::logging::chain(&error));
            Value::List(Vec::new())
        }
    }
}

fn may_configure(ctx: &Ctx<'_>) -> Result<(i32, i32), String> {
    if !ctx.session.has_corp_role(CORP_ROLE_DIRECTOR) && !ctx.session.has_corp_role(STARBASE_CONFIG)
    {
        return Err("You need the Starbase Configuration role.".into());
    }
    let corp = ctx.session.corp_id.ok_or("You have no corporation.")?;
    let system = ctx
        .session
        .solar_system_id
        .ok_or("You must be in space to configure a jump bridge.")?;
    Ok((corp as i32, system as i32))
}

pub async fn install(ctx: &Ctx<'_>, local: i64, remote_system: i64, remote: i64) -> Outcome {
    let result = install_inner(ctx, local, remote_system, remote).await;
    match result {
        Ok(()) => {
            refresh(ctx, local, remote).await;
            Outcome::Value(Value::None)
        }
        Err(reason) => Outcome::custom(reason),
    }
}

async fn install_inner(
    ctx: &Ctx<'_>,
    local: i64,
    remote_system: i64,
    remote: i64,
) -> Result<(), String> {
    let (corp, system) = may_configure(ctx)?;
    if local == remote || i64::from(system) == remote_system {
        return Err("A jump bridge must connect different systems.".into());
    }
    let mut db = crate::db::connect(ctx.db_url).await.map_err(db_error)?;
    let tx = db.transaction().await.map_err(db_error)?;
    let rows = tx
        .query(
            r#"SELECT e."itemID",e."ownerID",e."locationID",p.state,
            COALESCE(range.value::float8,0) FROM entity e
        JOIN "invTypes" t ON t."typeID"=e."typeID" JOIN "posStructures" p ON p."itemID"=e."itemID"
        LEFT JOIN "dgmTypeAttribs" range ON range."typeID"=e."typeID" AND range."attributeID"=867
        WHERE e."itemID"=ANY($1) AND t."groupID"=707 ORDER BY e."itemID" FOR UPDATE OF p"#,
            &[&vec![local as i32, remote as i32]],
        )
        .await
        .map_err(db_error)?;
    let a = rows
        .iter()
        .find(|row| int(row, 0) == Some(local))
        .ok_or("The local jump array does not exist.")?;
    let b = rows
        .iter()
        .find(|row| int(row, 0) == Some(remote))
        .ok_or("The destination jump array does not exist.")?;
    if int(a, 1) != Some(i64::from(corp)) || int(b, 1) != Some(i64::from(corp)) {
        return Err("Both jump arrays must belong to your corporation.".into());
    }
    if int(a, 2) != Some(i64::from(system)) || int(b, 2) != Some(remote_system) {
        return Err("Those jump arrays are not in the specified systems.".into());
    }
    if [a, b]
        .iter()
        .any(|row| !matches!(int(row, 3), Some(1 | 4 | 5)))
    {
        return Err("Both jump arrays must be anchored before linking them.".into());
    }
    let distance = jumps::light_years(ctx, i64::from(system), remote_system)
        .await
        .ok_or("Those systems could not be found.")?;
    if distance > float(a, 4).unwrap_or(0.0).min(float(b, 4).unwrap_or(0.0)) {
        return Err("The destination is beyond the jump arrays' range.".into());
    }
    let linked=tx.query_one(r#"SELECT count(*) FROM "jumpBridgeLinks" WHERE "sourceID"=ANY($1) OR "destinationID"=ANY($1)"#,
        &[&vec![local as i32,remote as i32]]).await.map_err(db_error)?.get::<_,i64>(0);
    if linked != 0 {
        return Err("Unlink the existing bridge before making another link.".into());
    }
    tx.execute(
        r#"INSERT INTO "jumpBridgeLinks" ("sourceID","destinationID") VALUES ($1,$2),($2,$1)"#,
        &[&(local as i32), &(remote as i32)],
    )
    .await
    .map_err(db_error)?;
    tx.commit().await.map_err(db_error)
}

pub async fn uninstall(ctx: &Ctx<'_>, local: i64) -> Outcome {
    let result:Result<i64,String>=async {
        let (corp,system)=may_configure(ctx)?;
        let mut db=crate::db::connect(ctx.db_url).await.map_err(db_error)?;
        let tx=db.transaction().await.map_err(db_error)?;
        let local_row=tx.query_opt(r#"SELECT e."itemID" FROM entity e JOIN "posStructures" p ON p."itemID"=e."itemID"
            WHERE e."itemID"=$1 AND e."ownerID"=$2 AND e."locationID"=$3 FOR UPDATE OF p"#,
            &[&(local as i32),&corp,&system]).await.map_err(db_error)?;
        if local_row.is_none() { return Err("That is not your corporation's local jump array.".into()); }
        let peers=tx.query(r#"DELETE FROM "jumpBridgeLinks" WHERE "sourceID"=$1 OR "destinationID"=$1 RETURNING "destinationID""#,
            &[&(local as i32)]).await.map_err(db_error)?;
        let remote=peers.iter().filter_map(|row|int(row,0)).find(|id|*id!=local).unwrap_or(0);
        tx.commit().await.map_err(db_error)?;
        Ok(remote)
    }.await;
    match result {
        Ok(remote) => {
            refresh(ctx, local, remote).await;
            Outcome::Value(Value::None)
        }
        Err(reason) => Outcome::custom(reason),
    }
}

async fn refresh(ctx: &Ctx<'_>, local: i64, remote: i64) {
    let rows = ctx
        .query(
            r#"SELECT e."itemID",e."locationID",other."itemID",other."locationID",other."typeID"
        FROM entity e LEFT JOIN "jumpBridgeLinks" l ON l."sourceID"=e."itemID"
        LEFT JOIN entity other ON other."itemID"=l."destinationID" WHERE e."itemID"=ANY($1)"#,
            &[&vec![local as i32, remote as i32]],
        )
        .await;
    let mut world = ctx.world.lock();
    for row in rows {
        let (id, system) = (int(&row, 0).unwrap(), int(&row, 1).unwrap());
        let entry = world.parks.get_mut(&system).and_then(|park| {
            let structure = park.structures.get_mut(&id)?;
            structure.bridge = match (int(&row, 2), int(&row, 3), int(&row, 4)) {
                (Some(id), Some(system), Some(type_id)) => Some((id, system, type_id)),
                _ => None,
            };
            Some(crate::pos::slim_change(park.tick, structure))
        });
        if let Some(entry) = entry {
            super::space::broadcast(&world, system, None, vec![entry]);
        }
    }
}

fn db_error(error: tokio_postgres::Error) -> String {
    tracing::error!("jump bridge: {}", crate::logging::chain(&error));
    "The jump bridge operation could not be completed.".into()
}

/// Validate both endpoints and quote the departure array's fuel. Arrival
/// commits the debit together with the pilot and ship's destination.
pub async fn prepare(ctx: &Ctx<'_>, local: i64, remote: i64, to: i64) -> Result<jumps::PreparedJump, String> {
    let (Some(system), Some(ship_id)) = (ctx.session.solar_system_id, ctx.session.ship_id) else {
        return Err("You are not in space.".into());
    };
    let mass = {
        let world = ctx.world.lock();
        let park = world.parks.get(&system).ok_or("You are not in space.")?;
        let ship = park.ships.get(&ship_id).ok_or("You are not in space.")?;
        if ship.warping() || ship.cloaked {
            return Err("You cannot jump while cloaked or warping.".into());
        }
        let array = park
            .structures
            .get(&local)
            .ok_or("The jump array is not here.")?;
        if (ship.pos - array.pos).len() - ship.stats.radius - array.radius > jumps::BRIDGE_RANGE {
            return Err("Move within 5 km of the jump array.".into());
        }
        if ship.pilot.corp_id != array.corp_id
            && (ship.pilot.alliance_id.is_none() || ship.pilot.alliance_id != array.alliance_id)
        {
            return Err(
                "You are not a member of this jump array's corporation or alliance.".into(),
            );
        }
        ship.stats.mass
    };
    let row=ctx.db.query_opt(r#"SELECT b.x,b.y,b.z,b."typeID",p.state,q.state
        FROM "jumpBridgeLinks" link JOIN entity a ON a."itemID"=link."sourceID"
        JOIN entity b ON b."itemID"=link."destinationID"
        JOIN "posStructures" p ON p."itemID"=a."itemID" JOIN "posStructures" q ON q."itemID"=b."itemID"
        WHERE a."itemID"=$1 AND b."itemID"=$2 AND a."locationID"=$3 AND b."locationID"=$4"#,&[&(local as i32),&(remote as i32),&(system as i32),&(to as i32)]).await.map_err(db_error)?
        .ok_or("That jump bridge link no longer exists.")?;
    if !matches!(int(&row, 4), Some(4 | 5)) || !matches!(int(&row, 5), Some(4 | 5)) {
        return Err("Both jump arrays must be online.".into());
    }
    let destination = Vec3::new(
        float(&row, 0).unwrap(),
        float(&row, 1).unwrap(),
        float(&row, 2).unwrap(),
    );
    let attributes = ctx.db
        .query(
            r#"SELECT "attributeID",value::float8 FROM "dgmTypeAttribs"
        WHERE "typeID"=$1 AND "attributeID" IN (866,868,1001)"#,
            &[&(int(&row, 3).unwrap() as i32)],
        )
        .await
        .map_err(db_error)?;
    let attr = |id: i64| {
        attributes
            .iter()
            .find(|row| int(row, 0) == Some(id))
            .and_then(|row| float(row, 1))
            .unwrap_or(0.0)
    };
    let fuel = attr(866) as i32;
    let distance = jumps::light_years(ctx, system, to)
        .await
        .ok_or("Those systems could not be found.")?;
    // menusvc's tooltip gives these four factors. The original server's
    // rounding expression is not in the dump; ceiling follows this server's
    // capital-drive payment rule and remains a historical-parity limitation.
    let quantity = (distance * attr(868) * attr(1001) * mass).ceil().max(1.0) as i64;
    if fuel <= 0 {
        return Err("The jump array's fuel type is missing.".into());
    }
    let fuel=jumps::FuelCost {cargo:local,type_id:i64::from(fuel),quantity,flag:None};
    fuel.check(ctx).await?;
    Ok(jumps::PreparedJump {at:jumps::landing(destination),fuel,capacitor:0.0,clients:vec![ctx.client_id]})
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ballpark::{Park, Pilot, Ship, ShipStats};

    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE with imported static data"]
    async fn bridge_links_survive_reload_and_validate_endpoint_and_fuel() {
        let database = crate::test_support::Database::load().await;
        let corp = database.id().await;
        let owner = database.id().await;
        let local = database.id().await;
        let remote = database.id().await;
        let fuel = database.id().await;
        let ship_id = database.id().await;
        for (id, location) in [(local, 30000142), (remote, 30000143)] {
            database.db.execute(r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",quantity,singleton)
                VALUES ($1,27897,$2,$3,1,1)"#, &[&id,&corp,&location]).await.unwrap();
            database
                .db
                .execute(
                    r#"INSERT INTO "posStructures" ("itemID",state) VALUES ($1,4)"#,
                    &[&id],
                )
                .await
                .unwrap();
        }
        database
            .db
            .execute(
                r#"INSERT INTO entity ("itemID","typeID","ownerID","locationID",quantity)
            VALUES ($1,16273,$2,$3,10)"#,
                &[&fuel, &corp, &local],
            )
            .await
            .unwrap();
        let mut session = crate::session::Session {
            char_id: Some(i64::from(owner)),
            corp_id: Some(i64::from(corp)),
            ship_id: Some(i64::from(ship_id)),
            solar_system_id: Some(30000142),
            ..Default::default()
        };
        assert!(matches!(
            install(
                &database.ctx(&session),
                i64::from(local),
                30000143,
                i64::from(remote)
            )
            .await,
            Outcome::UserError { .. }
        ));
        session.roles_at_all = STARBASE_CONFIG;
        let ctx = database.ctx(&session);
        assert!(matches!(
            install(&ctx, i64::from(local), 30000143, i64::from(remote)).await,
            Outcome::Value(Value::None)
        ));
        assert!(matches!(
            install(&ctx, i64::from(local), 30000143, i64::from(remote)).await,
            Outcome::UserError { .. }
        ));
        let values = list(&ctx).await;
        let values = values.seq().unwrap();
        assert_eq!(values.len(), 2);
        assert!(values.iter().all(
            |value| matches!(&value.seq().unwrap()[1],Value::Dict(entries) if entries.len()==1)
        ));
        assert_eq!(linkable(&ctx).await, Value::List(Vec::new()));
        let loaded = crate::pos::load(&database.db, 30000142).await.unwrap();
        let structure = loaded
            .into_iter()
            .find(|structure| structure.id == i64::from(local))
            .unwrap();
        assert_eq!(structure.bridge, Some((i64::from(remote), 30000143, 27897)));
        let mut park = Park::new(30000142, Vec::new(), 0);
        park.structures.insert(i64::from(local), structure);
        park.ships.insert(
            i64::from(ship_id),
            Ship::new(
                i64::from(ship_id),
                1,
                ShipStats::new(100.0, 1_000_000.0, 1.0, 300.0, 1.0),
                Pilot {
                    type_id: 587,
                    group_id: 25,
                    name: "Pilot".into(),
                    char_id: i64::from(owner),
                    corp_id: i64::from(corp),
                    alliance_id: None,
                    security: 0.0,
                    bounty: 0.0,
                },
                Vec3::new(1000.0, 0.0, 0.0),
                Vec3::ZERO,
            ),
        );
        database.world.lock().parks.insert(30000142, park);
        assert!(prepare(&ctx, i64::from(local), i64::from(remote), 30000144)
            .await
            .is_err());
        let quote=prepare(&ctx, i64::from(local), i64::from(remote), 30000143).await.unwrap();
        assert_eq!(quote.fuel.quantity,1);
        assert_eq!(quote.fuel.cargo,i64::from(local));
        assert_eq!(quote.fuel.flag,None);
        let left = database
            .db
            .query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#, &[&fuel])
            .await
            .unwrap()
            .get::<_, i32>(0);
        assert_eq!(left, 10,"preparation never pays before arrival");
        database
            .db
            .execute(
                r#"UPDATE "posStructures" SET state=1 WHERE "itemID"=$1"#,
                &[&remote],
            )
            .await
            .unwrap();
        assert!(prepare(&ctx, i64::from(local), i64::from(remote), 30000143)
            .await
            .is_err());
        let left = database
            .db
            .query_one(r#"SELECT quantity FROM entity WHERE "itemID"=$1"#, &[&fuel])
            .await
            .unwrap()
            .get::<_, i32>(0);
        assert_eq!(left, 10, "failed jumps do not consume fuel");
        assert!(matches!(
            uninstall(&ctx, i64::from(local)).await,
            Outcome::Value(Value::None)
        ));
        assert_eq!(linkable(&ctx).await.seq().unwrap().len(), 2);
        assert!(
            database.world.lock().parks[&30000142].structures[&i64::from(local)]
                .bridge
                .is_none()
        );
    }
}
