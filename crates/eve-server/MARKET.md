# Market implementation for build 45017

The original client contracts are in `client/script/ui/services/marketsvc.py`,
`client/script/ui/shared/market/quote.py`, `orders.py`, and
`client/script/ui/shared/neocom/wallet.py` in the decompiled client tree.

- `StartupCheck` verifies database availability and a selected regional session.
- Orders and price history use native `blue.DBRow` values in `dbutil.RowList`.
  The buy/sell picker rejects ordinary `util.Row` values when building its list
  of matching orders. Order records expose `accountKey` for corporation orders.
- `GetOrders` returns `objectCaching.CachedMethodCallResult` with a regional
  cache key, an Adler-32 content version, and an always-check policy. Without
  that version, the client keeps its previous order list after a refresh.
- `OnOwnOrderChanged` carries `(order, reason, isCorp)`. Wallet and inventory
  notifications go to the affected owners; corporation changes reach members.

## Trading

Purchases, sales, matching, inventory, escrow, fees, tax, transaction records,
and daily regional history commit in one transaction. Matching serializes with
repricing, cancellation, and expiration. Immediate orders fill completely or
roll back. Listed orders retain their remainder; minimum volumes and geographic
ranges apply. Remote purchases deliver at the selling station, including when
the buyer placed a ranged order from another station.

Repricing preserves the order ID and rematches crossing orders. Cancellation
and expiration return the remaining escrow or goods once. Expiration runs before
market reads/writes and on the server's minute heartbeat. Starter NPC orders
have permanent stock; player orders expire normally.

Trade, Retail, Wholesale, and Tycoon control open-order limits. Marketing,
Procurement, Visibility, and Daytrading control remote operations. Margin
Trading reduces the initial escrow; remaining funding comes from the buyer's
wallet when a sale fills it. Unfundable bids are skipped. Broker Relations,
Accounting, and NPC standings affect charges. Repricing enforces the client's
five-minute delay at the RPC boundary.

Corporation orders retain their wallet owner and division independently of the
issuing character's later membership. All seven wallet divisions are supported:
the existing corporation balance is account 1000, and `corporation_wallets`
stores accounts 1001â€“1006. Selection persists through `SetAccountKey`; transfer,
escrow, refund, tax, journal, and transaction records retain the account key.
Division access and corporation trade roles are checked on the server.

## Starter supply

`svc/market_seed.sql` runs at startup and is safe to repeat. Starter stations
use the same school/corporation/home-system lookup as character creation. Every
type with a market group is stocked at every school station. On the imported
database this is **5,567 types at 15 stations**, with both NPC asks and bids:
**167,010 starter orders**. The selector includes all schools in the data,
including schools not offered to ordinary player characters.

Existing NPC prices are preserved. Missing offers use an existing NPC ask,
then the type's base price, then 1,000 ISK per portion. New buy offers pay half
the ask. Stock is one billion units per order and does not deplete when traded
by the updated server. Existing player orders and other stations are retained.
Types without a market group (internal objects and other non-market types) are
not made into artificial market categories.

## Verification and rollout

Run `cargo test --workspace --offline` and, against a disposable imported
database, set `TRINITY_TEST_DATABASE` and run:

```
cargo test -p eve-server --bin eve-server svc::trade:: --offline -- --include-ignored --test-threads=1
```

Database tests cover stock coverage, NPC buying/selling, native response shapes,
frame sizes, player matching, repricing, refunds, expiration, concurrency,
permissions, margin funding, corporation deliveries, notifications, division
wallets, and transaction filters. `crates/eve-server/examples/market-smoke.calls` supplies read-only
protocol checks for `fakeclient`. Protocol checks do not replace a live client
GUI test.

Rebuild/restart the Docker service to activate source changes:

```
docker compose up -d --build server
```

The startup migrations create the wallet-division storage and reconcile starter
stock. The live database's starter stock was also seeded and audited directly
during this change; that does not update an already-running server executable.

