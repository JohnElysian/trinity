//! Tables and columns added after the dumps in `db/` were first cut, made on
//! start where an older database lacks them (the dumps hold them too, for a
//! fresh one).

use tokio_postgres::Client;

const STATEMENTS: &[&str] = &[
    // authentication.SetLanguageID persists the account preference; the
    // original wwwroot/language.py updates autUsers.languageID and the session.
    r#"ALTER TABLE "account" ADD COLUMN IF NOT EXISTS "languageID" text"#,
    // Jump clones (svc::clones): a body waiting in a station or a ship's
    // clone vat bay; its implants are items located on it.
    r#"CREATE TABLE IF NOT EXISTS "chrJumpClones" (
           "jumpCloneID" integer PRIMARY KEY DEFAULT nextval('dyn_id_seq'),
           "ownerID" integer NOT NULL,
           "locationID" integer NOT NULL,
           "createdAt" bigint NOT NULL DEFAULT 0)"#,
    r#"CREATE INDEX IF NOT EXISTS "chrJumpClones_owner" ON "chrJumpClones" ("ownerID")"#,
    r#"CREATE INDEX IF NOT EXISTS "chrJumpClones_location" ON "chrJumpClones" ("locationID")"#,
    r#"COMMENT ON TABLE "chrJumpClones" IS 'MADE: jump clones, in stations or ships with a clone vat bay; implants are entity rows located on the clone (svc::clones).'"#,
    r#"ALTER TABLE "character_" ADD COLUMN IF NOT EXISTS "lastCloneJump" bigint NOT NULL DEFAULT 0"#,
    // Outposts being built (svc::outposts), and how their owners run stations.
    r#"CREATE TABLE IF NOT EXISTS "outpostConstruction" (
           "platformID" integer PRIMARY KEY,
           "ownerID" integer NOT NULL,
           "solarSystemID" integer NOT NULL,
           "builderID" integer NOT NULL DEFAULT 0,
           "startedAt" bigint NOT NULL,
           "doneAt" bigint NOT NULL)"#,
    r#"COMMENT ON TABLE "outpostConstruction" IS 'MADE: outpost platforms under construction, and when each becomes a station (svc::outposts).'"#,
    r#"CREATE TABLE IF NOT EXISTS "stationServiceRules" (
           "stationID" integer NOT NULL,
           "serviceID" integer NOT NULL,
           "minimumStanding" double precision NOT NULL DEFAULT -10,
           "minimumCharSecurity" double precision NOT NULL DEFAULT -10,
           "maximumCharSecurity" double precision NOT NULL DEFAULT 10,
           "minimumCorpSecurity" double precision NOT NULL DEFAULT -10,
           "maximumCorpSecurity" double precision NOT NULL DEFAULT 10,
           "discountPerGoodStandingPoint" double precision NOT NULL DEFAULT 0,
           "surchargePerBadStandingPoint" double precision NOT NULL DEFAULT 0,
           PRIMARY KEY ("stationID", "serviceID"))"#,
    r#"COMMENT ON TABLE "stationServiceRules" IS 'MADE: who a player-owned station lets use each service, and at what cost (svc::outposts).'"#,
    r#"ALTER TABLE "staStations" ADD COLUMN IF NOT EXISTS "description" text"#,
    // Trinity's bulk categories and groups carry a published flag (its
    // static data has it; RMR's did not).
    r#"ALTER TABLE "invCategories" ADD COLUMN IF NOT EXISTS "published" smallint NOT NULL DEFAULT 1"#,
    r#"ALTER TABLE "invGroups" ADD COLUMN IF NOT EXISTS "published" smallint NOT NULL DEFAULT 1"#,
    // Trinity's character creator: careers and specialities replace the
    // school/department/field path, skills come with levels, and every step
    // has a short description. tools/trinity_static.py fills these; on a
    // database it has not refreshed they exist but are empty.
    r#"ALTER TABLE "chrRaces" ADD COLUMN IF NOT EXISTS "shortDescription" text"#,
    r#"ALTER TABLE "chrBloodlines" ADD COLUMN IF NOT EXISTS "shortDescription" text"#,
    r#"ALTER TABLE "chrBloodlines" ADD COLUMN IF NOT EXISTS "shortMaleDescription" text"#,
    r#"ALTER TABLE "chrBloodlines" ADD COLUMN IF NOT EXISTS "shortFemaleDescription" text"#,
    r#"ALTER TABLE "chrAncestries" ADD COLUMN IF NOT EXISTS "shortDescription" text"#,
    r#"ALTER TABLE "chrAncestries" ADD COLUMN IF NOT EXISTS "typeID2" integer"#,
    r#"ALTER TABLE "chrAncestries" ADD COLUMN IF NOT EXISTS "typeQuantity2" integer"#,
    r#"ALTER TABLE "chrAttributes" ADD COLUMN IF NOT EXISTS "shortDescription" text"#,
    r#"ALTER TABLE "chrSchools" ADD COLUMN IF NOT EXISTS "careerID" integer"#,
    r#"CREATE TABLE IF NOT EXISTS "chrCareers" (
           "raceID" integer, "careerID" integer PRIMARY KEY, "careerName" text, description text,
           "shortDescription" text, "graphicID" integer, "schoolID" integer)"#,
    r#"CREATE TABLE IF NOT EXISTS "chrCareerSpecialities" (
           "careerID" integer, "specialityID" integer PRIMARY KEY, "specialityName" text, description text,
           "shortDescription" text, "graphicID" integer, "departmentID" integer)"#,
    r#"CREATE TABLE IF NOT EXISTS "chrCareerSkills" ("careerID" integer, "skillTypeID" integer, levels integer)"#,
    r#"CREATE TABLE IF NOT EXISTS "chrCareerSpecialitySkills" ("specialityID" integer, "skillTypeID" integer, levels integer)"#,
    r#"CREATE TABLE IF NOT EXISTS "chrRaceSkills" ("raceID" integer, "skillTypeID" integer, levels integer)"#,
    // What a character was made from, for Trinity's character sheet
    // (charactersheet.py reads careerID, schoolID, ancestryID and
    // careerSpecialityID from charmgr.GetPublicInfo).
    r#"ALTER TABLE "character_" ADD COLUMN IF NOT EXISTS "ancestryID" integer"#,
    r#"ALTER TABLE "character_" ADD COLUMN IF NOT EXISTS "schoolID" integer"#,
    r#"ALTER TABLE "character_" ADD COLUMN IF NOT EXISTS "careerID" integer"#,
    r#"ALTER TABLE "character_" ADD COLUMN IF NOT EXISTS "careerSpecialityID" integer"#,
    // Characters made before those columns: the school their corporation
    // runs, its first career and speciality, the bloodline's first ancestry.
    r#"UPDATE "character_" c SET "schoolID" = s."schoolID"
         FROM "chrSchools" s WHERE s."corporationID" = c."corporationID" AND c."schoolID" IS NULL"#,
    r#"UPDATE "character_" c SET "careerID" = (SELECT min(k."careerID") FROM "chrCareers" k WHERE k."schoolID" = c."schoolID")
        WHERE c."careerID" IS NULL AND c."schoolID" IS NOT NULL"#,
    r#"UPDATE "character_" c SET "careerSpecialityID" = (SELECT min(p."specialityID") FROM "chrCareerSpecialities" p WHERE p."careerID" = c."careerID")
        WHERE c."careerSpecialityID" IS NULL AND c."careerID" IS NOT NULL"#,
    r#"UPDATE "character_" c SET "ancestryID" = (SELECT min(a."ancestryID") FROM "chrAncestries" a WHERE a."bloodlineID" = c."bloodlineID")
        WHERE c."ancestryID" IS NULL AND c."accountID" IS NOT NULL"#,
    // Trinity's loyalty points: one balance per pilot and NPC corporation
    // (svc::lp), earned from that corporation's agents.
    r#"CREATE TABLE IF NOT EXISTS "chrLoyaltyPoints" (
           "characterID" integer NOT NULL,
           "corporationID" integer NOT NULL,
           "loyaltyPoints" integer NOT NULL DEFAULT 0 CHECK ("loyaltyPoints" >= 0),
           PRIMARY KEY ("characterID", "corporationID"))"#,
    r#"COMMENT ON TABLE "chrLoyaltyPoints" IS 'MADE: loyalty points each pilot holds with each NPC corporation (svc::lp).'"#,
    // Trinity's solar system rows carry their sovereignty level (neocom.py).
    r#"ALTER TABLE "mapSolarSystems" ADD COLUMN IF NOT EXISTS "sovereigntyLevel" integer"#,
    // Deadspace complexes (complexes): generated pockets and who has been in them.
    r#"CREATE TABLE IF NOT EXISTS "deadspaceComplexes" (
           "complexID" integer PRIMARY KEY DEFAULT nextval('dyn_id_seq'),
           "solarSystemID" integer NOT NULL,
           "level" smallint NOT NULL,
           "seed" bigint NOT NULL,
           "x" double precision NOT NULL, "y" double precision NOT NULL, "z" double precision NOT NULL,
           "rooms" smallint NOT NULL,
           "cleared" smallint NOT NULL DEFAULT 0,
           "spawnedAt" bigint NOT NULL,
           "expires" bigint NOT NULL)"#,
    r#"COMMENT ON TABLE "deadspaceComplexes" IS 'MADE: generated deadspace complexes: where each lies, how hard it is, how far it has been cleared (complexes).'"#,
    // Moon scans (svc::scan): what a probe found at a moon, for its owner.
    r#"CREATE TABLE IF NOT EXISTS "moonScans" (
           "charID" integer NOT NULL,
           "moonID" integer NOT NULL,
           "scannedAt" bigint NOT NULL,
           PRIMARY KEY ("charID", "moonID"))"#,
    r#"COMMENT ON TABLE "moonScans" IS 'MADE: moons each character has surveyed with a probe (svc::scan).'"#,
    // Prices the data leaves at nothing, which insurance and clones are
    // priced by: a ship at what the NPC market pays for it, a clone a quarter
    // of an ISK for every skill point it keeps (the Alpha clone stays free).
    r#"UPDATE "invTypes" t SET "basePrice" = o.bid
         FROM (SELECT "typeID", max("price") AS bid FROM market_orders WHERE "bid" = 1 AND "charID" < 100000000 GROUP BY 1) o
        WHERE t."typeID" = o."typeID" AND COALESCE(t."basePrice", 0) = 0
          AND t."groupID" IN (SELECT "groupID" FROM "invGroups" WHERE "categoryID" = 6)
          AND EXISTS (SELECT 1 FROM "invTypes" x JOIN "invGroups" g ON g."groupID" = x."groupID" WHERE g."categoryID" = 6 AND COALESCE(x."basePrice", 0) = 0 AND x."marketGroupID" IS NOT NULL)"#,
    r#"UPDATE "invTypes" t SET "basePrice" = round(COALESCE(a."valueFloat", a."valueInt") * 0.25)
         FROM "dgmTypeAttributes" a
        WHERE a."typeID" = t."typeID" AND a."attributeID" = 419 AND t."groupID" = 23 AND t."typeID" <> 164 AND COALESCE(t."basePrice", 0) = 0"#,
];

pub async fn ensure(db: &Client) -> Result<(), tokio_postgres::Error> {
    for sql in STATEMENTS {
        db.execute(*sql, &[]).await?;
    }
    crate::svc::recruitment::ensure(db).await?;
    crate::svc::contracts::ensure(db).await?;
    crate::svc::trade::ensure(db).await?;
    crate::svc::petition::ensure(db).await?;
    crate::svc::bridges::ensure(db).await?;
    crate::svc::chat::ensure(db).await?;
    crate::losses::ensure(db).await?;
    crate::travel::ensure(db).await?;
    crate::svc::jumps::portal::ensure(db).await?;
    crate::losses::pending::ensure(db).await?;
    crate::losses::journal::ensure(db).await?;
    crate::static_entities::ensure(db).await?;
    crate::svc::map::ensure(db).await?;
    crate::svc::tutorial::ensure(db).await?;
    crate::drones::ensure(db).await?;
    crate::space_state::ensure(db).await?;
    crate::cloak::durability::ensure(db).await?;
    crate::svc::maintenance::ensure(db).await?;
    crate::svc::boosters::ensure(db).await?;
    crate::svc::outposts::ensure_access_locks(db).await?;
    crate::svc::outposts::ensure_management_schema(db).await?;
    crate::svc::corp::ensure_office_schema(db).await?;
    crate::conn::logout::ensure(db).await?;
    Ok(())
}
