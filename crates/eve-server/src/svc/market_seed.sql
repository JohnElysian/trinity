-- Idempotent starter-station supply. These NPC orders provide permanent stock;
-- player orders and orders outside starter stations are not replaced.
BEGIN;
SELECT pg_advisory_xact_lock(45017, 2);
ALTER TABLE market_orders ADD COLUMN IF NOT EXISTS "starterSeed" boolean NOT NULL DEFAULT false;
CREATE UNIQUE INDEX IF NOT EXISTS market_starter_seed_unique
    ON market_orders ("stationID", "typeID", bid) WHERE "starterSeed";
CREATE INDEX IF NOT EXISTS market_open_matching
    ON market_orders ("regionID", "typeID", bid, price, issued) WHERE "orderState" = 0;
CREATE INDEX IF NOT EXISTS market_npc_prices ON market_orders ("typeID", price)
    WHERE bid = 0 AND "charID" < 100000000;
CREATE INDEX IF NOT EXISTS market_order_expiry
    ON market_orders ((issued + duration::bigint * 864000000000))
    WHERE "orderState" = 0 AND "charID" >= 100000000;

WITH school AS (
    SELECT min(st."stationID") AS "stationID"
    FROM "chrSchools" s
    JOIN "crpNPCCorporations" n ON n."corporationID" = s."corporationID"
    JOIN "staStations" st ON st."corporationID" = s."corporationID"
        AND st."solarSystemID" = n."solarSystemID"
    GROUP BY s."schoolID"
), existing AS (
    SELECT DISTINCT ON (o."stationID", o."typeID", o.bid) o."orderID"
    FROM market_orders o JOIN school s USING ("stationID")
    WHERE o."charID" < 100000000 AND o."orderState" = 0
        AND NOT EXISTS (SELECT 1 FROM market_orders seeded WHERE seeded."starterSeed"
            AND seeded."stationID" = o."stationID" AND seeded."typeID" = o."typeID" AND seeded.bid = o.bid)
    ORDER BY o."stationID", o."typeID", o.bid, o."orderID"
)
UPDATE market_orders SET "starterSeed" = true WHERE "orderID" IN (SELECT "orderID" FROM existing);

WITH school AS (
    SELECT min(st."stationID") AS "stationID"
    FROM "chrSchools" s
    JOIN "crpNPCCorporations" n ON n."corporationID" = s."corporationID"
    JOIN "staStations" st ON st."corporationID" = s."corporationID"
        AND st."solarSystemID" = n."solarSystemID"
    GROUP BY s."schoolID"
), prices AS (
    SELECT t."typeID", greatest(0.02, coalesce(
        (SELECT min(o.price) FROM market_orders o WHERE o."typeID" = t."typeID"
            AND o.bid = 0 AND o."charID" < 100000000 AND o.price > 0),
        nullif(t."basePrice", 0), 1000.0 / greatest(t."portionSize", 1))) AS price
    FROM "invTypes" t WHERE t."marketGroupID" IS NOT NULL
)
INSERT INTO market_orders ("typeID", "charID", "walletOwnerID", "regionID", "stationID",
    range, bid, price, "volEntered", "volRemaining", issued, "orderState", "minVolume",
    contraband, "accountID", duration, "isCorp", "solarSystemID", escrow, jumps, "starterSeed")
SELECT p."typeID", st."corporationID", st."corporationID", st."regionID", st."stationID",
    -1, side.bid, round((p.price * CASE WHEN side.bid = 1 THEN 0.5 ELSE 1 END)::numeric, 2)::float8,
    1000000000, 1000000000, (extract(epoch FROM now())::bigint + 11644473600) * 10000000,
    0, 1, 0, 1000, 365, 1, st."solarSystemID", 0, 0, true
FROM prices p CROSS JOIN school s JOIN "staStations" st USING ("stationID")
CROSS JOIN (VALUES (0::smallint), (1::smallint)) side(bid)
ON CONFLICT ("stationID", "typeID", bid) WHERE "starterSeed" DO UPDATE
SET "volEntered" = 1000000000, "volRemaining" = 1000000000,
    "orderState" = 0, issued = EXCLUDED.issued, duration = 365,
    "walletOwnerID" = EXCLUDED."walletOwnerID";
COMMIT;
