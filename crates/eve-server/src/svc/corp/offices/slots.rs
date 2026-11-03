//! Stable original rentalInfo slot numbers shared by renting and management.
use tokio_postgres::{Client,Error,Transaction};

pub(crate) async fn ensure_schema(db:&Client)->Result<(),Error> {
    db.batch_execute(r#"
        ALTER TABLE "crpOffices" ADD COLUMN IF NOT EXISTS "rentalSlot" integer;
        CREATE UNIQUE INDEX IF NOT EXISTS "crpOffices_rentalSlot" ON "crpOffices" ("stationID","rentalSlot") WHERE "rentalSlot" IS NOT NULL;
    "#).await
}

/// Caller holds the station FOR UPDATE. Do not rewrite historical rental
/// prices, dates, visibility or slot identities when reconciling older offices.
/// An old office without billing history gets no invented historical dates.
pub(crate) async fn ensure_slots_locked(tx:&Transaction<'_>,station:i32)->Result<(),Error> {
    let slots=tx.query_opt(r#"SELECT COALESCE(t."officeSlots",0) FROM "staStations" s
        JOIN "staStationTypes" t ON t."stationTypeID"=s."stationTypeID" WHERE s."stationID"=$1 FOR SHARE OF t"#,&[&station]).await?
        .map_or(0,|row|row.get::<_,i32>(0).max(0));
    tx.execute(r#"INSERT INTO "rentalInfo" ("stationID","slotNumber","typeID","publiclyAvailable")
        SELECT $1,n,26,1 FROM generate_series(1,$2::integer) n ON CONFLICT ("stationID","slotNumber") DO NOTHING"#,&[&station,&slots]).await?;
    let offices=tx.query(r#"SELECT "corporationID","officeFolderID" FROM "crpOffices"
        WHERE "stationID"=$1 AND "rentalSlot" IS NULL ORDER BY "officeFolderID" FOR UPDATE"#,&[&station]).await?;
    for office in offices {
        let corporation:i32=office.get(0);let folder:i32=office.get(1);
        let slot=tx.query_opt(r#"SELECT r."slotNumber" FROM "rentalInfo" r
            WHERE r."stationID"=$1 AND r."typeID"=26 AND (r."renterID"=$2 OR (r."renterID"=0 AND r."slotNumber" BETWEEN 1 AND $3))
              AND NOT EXISTS(SELECT 1 FROM "crpOffices" o WHERE o."stationID"=r."stationID" AND o."rentalSlot"=r."slotNumber")
            ORDER BY (r."renterID"=$2) DESC,r."slotNumber" LIMIT 1 FOR UPDATE OF r"#,&[&station,&corporation,&slots]).await?;
        let Some(slot)=slot.map(|row|row.get::<_,i32>(0)) else {continue;};
        tx.execute(r#"UPDATE "rentalInfo" SET "renterID"=$3 WHERE "stationID"=$1 AND "slotNumber"=$2 AND "renterID"=0"#,&[&station,&slot,&corporation]).await?;
        tx.execute(r#"UPDATE "crpOffices" SET "rentalSlot"=$4 WHERE "stationID"=$1 AND "corporationID"=$2 AND "officeFolderID"=$3"#,&[&station,&corporation,&folder,&slot]).await?;
    }
    Ok(())
}
