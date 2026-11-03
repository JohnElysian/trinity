//! Materialize identities added by the Trinity SDE after the older entity dump.
//! Existing items/characters are preserved, including player station ownership.
use tokio_postgres::Client;

pub async fn ensure(db: &Client) -> Result<(), tokio_postgres::Error> {
    // Inventory lookups use entity, while the ballpark uses mapDenormalize.
    // Without these rows new systems/stations exist on the map but have no item.
    for table in ["entityStatic", "entity"] {
        db.execute(
            &format!(
                r#"INSERT INTO "{table}"
            ("itemID","itemName","typeID","ownerID","locationID",singleton,quantity,x,y,z)
            SELECT s."solarSystemID",s."solarSystemName",5,1,0,1,1,s.x,s.y,s.z
            FROM "mapSolarSystems" s WHERE NOT EXISTS
                (SELECT 1 FROM "{table}" e WHERE e."itemID"=s."solarSystemID")
            ON CONFLICT DO NOTHING"#
            ),
            &[],
        )
        .await?;
        db.execute(&format!(r#"INSERT INTO "{table}"
            ("itemID","itemName","typeID","ownerID","locationID",singleton,quantity,x,y,z)
            SELECT s."stationID",s."stationName",s."stationTypeID",s."corporationID",s."solarSystemID",1,1,s.x,s.y,s.z
            FROM "staStations" s WHERE NOT EXISTS
                (SELECT 1 FROM "{table}" e WHERE e."itemID"=s."stationID")
            ON CONFLICT DO NOTHING"#), &[]).await?;
    }
    // The export supplies names/types/affiliations and bloodlines, but no NPC
    // portrait appearance. Do not overwrite older character records with defaults.
    for table in ["characterStatic", "character_"] {
        db.execute(&format!(r#"INSERT INTO "{table}"
            ("characterID","characterName","corporationID","typeID","bloodlineID","raceID",
             "stationID","solarSystemID","constellationID","regionID")
            SELECT a."agentID",o."ownerName",COALESCE(a."corporationID",0),o."typeID",
                COALESCE(b."bloodlineID",0),COALESCE(b."raceID",0),COALESCE(a."stationID",0),
                COALESCE(s."solarSystemID",0),COALESCE(s."constellationID",0),COALESCE(s."regionID",0)
            FROM "agtAgents" a JOIN "eveStaticOwners" o ON o."ownerID"=a."agentID"
            LEFT JOIN "bloodlineTypes" bt ON bt."typeID"=o."typeID"
            LEFT JOIN "chrBloodlines" b ON b."bloodlineID"=bt."bloodlineID"
            LEFT JOIN "staStations" s ON s."stationID"=a."stationID"
            WHERE NOT EXISTS (SELECT 1 FROM "{table}" c WHERE c."characterID"=a."agentID")
            ON CONFLICT DO NOTHING"#), &[]).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    #[ignore = "requires TRINITY_TEST_DATABASE pointing at a disposable imported database"]
    async fn trinity_identities_are_present_and_existing_items_are_preserved() {
        let fixture = crate::test_support::Database::load().await;
        let db = &fixture.db;
        for query in [
            r#"SELECT count(*) FROM "staStations" s LEFT JOIN entity e ON e."itemID"=s."stationID" WHERE e."itemID" IS NULL"#,
            r#"SELECT count(*) FROM "mapSolarSystems" s LEFT JOIN entity e ON e."itemID"=s."solarSystemID" WHERE e."itemID" IS NULL"#,
            r#"SELECT count(*) FROM "agtAgents" a JOIN "eveStaticOwners" o ON o."ownerID"=a."agentID" LEFT JOIN "character_" c ON c."characterID"=a."agentID" WHERE c."characterID" IS NULL"#,
        ] {
            assert_eq!(db.query_one(query, &[]).await.unwrap().get::<_, i64>(0), 0);
        }
        let before = db
            .query_one(r#"SELECT count(*),sum("ownerID") FROM entity"#, &[])
            .await
            .unwrap();
        super::ensure(db).await.unwrap();
        let after = db
            .query_one(r#"SELECT count(*),sum("ownerID") FROM entity"#, &[])
            .await
            .unwrap();
        assert_eq!(before.get::<_, i64>(0), after.get::<_, i64>(0));
        assert_eq!(before.get::<_, i64>(1), after.get::<_, i64>(1));
    }
}
