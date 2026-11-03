//! Shared opt-in database test fixture. Use only a disposable imported database.
use crate::{bulkdata::BulkData, dogma::Catalog, services::Ctx, session::Session, world::World};

pub struct Database {
    pub db: tokio_postgres::Client,
    pub url: String,
    pub dogma: Catalog,
    pub bulk: BulkData,
    pub world: World,
}

impl Database {
    pub async fn load() -> Self {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("warn")
            .with_test_writer()
            .try_init();
        let url = std::env::var("TRINITY_TEST_DATABASE")
            .expect("set TRINITY_TEST_DATABASE to a disposable database imported from db/*.sql.gz");
        let db = crate::db::connect(&url)
            .await
            .expect("connect test database");
        crate::schema::ensure(&db)
            .await
            .expect("apply schema migrations");
        let dogma = Catalog::load(&db).await.expect("load test dogma");
        Self {
            db,
            url,
            dogma,
            bulk: BulkData {
                entries: Default::default(),
                node_id: 1,
            },
            world: World::default(),
        }
    }

    pub fn ctx<'a>(&'a self, session: &'a Session) -> Ctx<'a> {
        Ctx {
            db: &self.db,
            db_url: &self.url,
            dogma: &self.dogma,
            bulk: &self.bulk,
            world: &self.world,
            node_id: 1,
            client_id: 1,
            session,
        }
    }

    pub async fn id(&self) -> i32 {
        self.db
            .query_one("SELECT nextval('dyn_id_seq')::integer", &[])
            .await
            .unwrap()
            .get(0)
    }
}
