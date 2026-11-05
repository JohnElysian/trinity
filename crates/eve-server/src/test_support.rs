//! Shared test fixture: a store holding a fresh world (made once, copied for
//! each test, so tests run side by side without touching each other).
use std::sync::{Arc, OnceLock};

use crate::store::{Db, Store};
use crate::{bulkdata::BulkData, dogma::Catalog, services::Ctx, session::Session, world::World};

/// A fresh world as the server starts it: the dumps, the NPC market, and
/// the start-up fix-ups.
fn template() -> &'static Db {
    static WORLD: OnceLock<Db> = OnceLock::new();
    WORLD.get_or_init(|| {
        let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
        let tables = crate::sde::get().tables();
        let store = Store::in_memory(crate::startup::fresh(tables).expect("make a fresh world"));
        crate::startup::ensure(&store, tables).expect("start-up fix-ups");
        store.into_db()
    })
}

/// A store of its own holding a fresh world.
pub fn store() -> Store {
    Store::in_memory(template().clone())
}

pub struct Database {
    pub store: Arc<Store>,
    pub dogma: Catalog,
    pub bulk: BulkData,
    pub world: World,
}

impl Database {
    pub async fn load() -> Self {
        Self {
            store: Arc::new(store()),
            dogma: Catalog::new(),
            bulk: BulkData { entries: Default::default(), node_id: 1 },
            world: World::default(),
        }
    }

    pub fn ctx<'a>(&'a self, session: &'a Session) -> Ctx<'a> {
        Ctx {
            store: &self.store,
            dogma: &self.dogma,
            bulk: &self.bulk,
            world: &self.world,
            node_id: 1,
            client_id: 1,
            session,
        }
    }

    /// A fresh item ID (`dyn_id_seq`), for test rows that need one.
    pub async fn id(&self) -> i32 {
        self.store.write(|tx| tx.nextval("dyn_id_seq")).expect("dyn_id_seq") as i32
    }
}
