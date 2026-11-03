use super::*;

pub async fn ensure(db:&Client)->Result<(),tokio_postgres::Error> {
    db.batch_execute(r#"CREATE TABLE IF NOT EXISTS "travelReceipts" (
        generation bigserial PRIMARY KEY,"characterID" integer NOT NULL,"shipID" integer NOT NULL,"shipTypeID" integer NOT NULL,
        source text NOT NULL,destination text NOT NULL,state smallint NOT NULL DEFAULT 0 CHECK(state IN(0,1)),
        "createdAt" bigint NOT NULL,"completedAt" bigint,loss text);
        CREATE UNIQUE INDEX IF NOT EXISTS "travelReceipts_pending_ship" ON "travelReceipts"("shipID") WHERE state=0;
        CREATE UNIQUE INDEX IF NOT EXISTS "travelReceipts_pending_pilot" ON "travelReceipts"("characterID") WHERE state=0;"#).await
}

pub(super) async fn begin(tx:&Transaction<'_>,pilot:i64,ship:i64,source:Place,destination:Place)->Result<Marker,tokio_postgres::Error> {
    let valid=id(pilot) && id(ship) && source.valid() && destination.valid() && destination.station.is_none();
    let source_json=serde_json::to_string(&source).unwrap_or_default();
    let destination_json=serde_json::to_string(&destination).unwrap_or_default();
    let row=tx.query_one(r#"INSERT INTO "travelReceipts"("characterID","shipID","shipTypeID",source,destination,"createdAt")
        SELECT $2,e."itemID",e."typeID",$4,$5,$6 FROM entity e
        JOIN "invTypes" t ON t."typeID"=e."typeID" JOIN "invGroups" g ON g."groupID"=t."groupID"
        JOIN "character_" c ON c."characterID"=$2 AND c."shipID"=e."itemID" AND c."solarSystemID"=$3 AND COALESCE(c."stationID",0)=0
        WHERE e."itemID"=$1 AND e."ownerID"=$2 AND e."locationID"=$3 AND e.flag=0 AND e.quantity=1 AND e.singleton<>0
        AND g."categoryID"=6 AND $7 RETURNING generation,"shipTypeID""#,
        &[&(ship as i32),&(pilot as i32),&(destination.system as i32),&source_json,&destination_json,&crate::combat::now_ms(),&valid]).await?;
    Ok(Marker {generation:row.get(0),pilot,ship,ship_type:i64::from(row.get::<_,i32>(1)),source,destination})
}

pub(super) fn marker(row:&tokio_postgres::Row)->Option<Marker> {
    let source=row.get::<_,&str>(4);let destination=row.get::<_,&str>(5);
    if source.len()>4096 || destination.len()>4096 {return None;}
    let m=Marker {generation:row.get(0),pilot:i64::from(row.get::<_,i32>(1)),ship:i64::from(row.get::<_,i32>(2)),
        ship_type:i64::from(row.get::<_,i32>(3)),source:serde_json::from_str(source).ok()?,
        destination:serde_json::from_str(destination).ok()?};
    m.valid().then_some(m)
}

pub(super) struct Locked {pub resolution:Resolution,pub loss:Option<String>}
pub(super) async fn locked(tx:&Transaction<'_>,m:Marker)->Result<Locked,String> {
    let available=crate::svc::login::lock_character_lifecycle(tx,m.pilot as i32).await.map_err(failed)?;
    let pilot=tx.query_opt(r#"SELECT "shipID","solarSystemID",COALESCE("stationID",0) FROM "character_" WHERE "characterID"=$1 FOR UPDATE"#,
        &[&(m.pilot as i32)]).await.map_err(failed)?;
    let hull=tx.query_opt(r#"SELECT COALESCE("ownerID",0)::integer,COALESCE("typeID",0)::integer,COALESCE("locationID",0)::integer,
        COALESCE(flag,0)::integer,COALESCE(quantity,0)::integer,COALESCE(singleton,0)::integer FROM entity WHERE "itemID"=$1 FOR UPDATE"#,
        &[&(m.ship as i32)]).await.map_err(failed)?;
    let custody=|place:Place|available && pilot.as_ref().is_some_and(|row|row.get::<_,Option<i32>>(0)==Some(m.ship as i32)
        && row.get::<_,Option<i32>>(1)==Some(place.system as i32) && row.get::<_,i32>(2)==place.station.unwrap_or(0) as i32)
        && hull.as_ref().is_some_and(|row|row.get::<_,i32>(0)==m.pilot as i32 && row.get::<_,i32>(1)==m.ship_type as i32
            && row.get::<_,i32>(2)==place.station.unwrap_or(place.system) as i32 && row.get::<_,i32>(3)==if place.station.is_some(){4}else{0}
            && row.get::<_,i32>(4)==1 && row.get::<_,i32>(5)!=0);
    let row=tx.query_opt(r#"SELECT generation,"characterID","shipID","shipTypeID",source,destination,state,loss
        FROM "travelReceipts" WHERE generation=$1 FOR UPDATE"#,&[&m.generation]).await.map_err(failed)?;
    let Some(row)=row else {
        let newer=tx.query_one(r#"SELECT EXISTS(SELECT 1 FROM "travelReceipts" WHERE generation>$1 AND ("shipID"=$2 OR "characterID"=$3))"#,
            &[&m.generation,&(m.ship as i32),&(m.pilot as i32)]).await.map_err(failed)?.get::<_,bool>(0);
        return Ok(Locked {resolution:if !newer && custody(m.source){Resolution::RolledBack}else{Resolution::Unknown},loss:None});
    };
    if marker(&row)!=Some(m){return Ok(Locked {resolution:Resolution::Unknown,loss:None});}
    let state:i16=row.get(6);
    let resolution=if state==1 {Resolution::Finalized}else if state==0 && custody(m.destination){Resolution::Accepted}else{Resolution::Unknown};
    Ok(Locked {resolution,loss:row.get(7)})
}
