//! What the bot simulation sets right in the store on start. Its tables
//! (`bots`, `botStock`, `botRelations`, `botRegionDensity`, `botGroups`) are
//! part of the store's schema.

use crate::store::{StoreError, Tx};

/// Kill reports share one sequence: losses.rs and the bots both take from
/// it, so it starts past every kill already logged.
pub fn ensure(tx: &mut Tx<'_>) -> Result<(), StoreError> {
    let last = tx.table("killLog").rows().filter_map(|r| r.int("killID")).max().unwrap_or(0).max(1);
    tx.setval("killLog_killID_seq", last);
    Ok(())
}
