//! Shared test support: spins up a fresh, uniquely-named database per test
//! on the shared Postgres server, bootstraps the Supabase-style roles, and
//! drops it on drop.

use tokio_postgres::{Client, NoTls};

pub fn superuser_url() -> String {
    std::env::var("PG_TEST_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54390/postgres".to_string())
}

/// A throwaway database, dropped when this guard is dropped.
pub struct TestDb {
    name: String,
    admin_url: String,
    closed: bool,
}

impl TestDb {
    /// Create a fresh database with a unique name, bootstrap the Supabase
    /// `anon` / `authenticated` / `service_role` NOLOGIN roles if missing,
    /// and grant them to the connecting role.
    pub async fn create() -> TestDb {
        let admin_url = superuser_url();
        let name = format!("pgcore_test_{}_{}", std::process::id(), unique_suffix());

        let (admin_client, conn) = tokio_postgres::connect(&admin_url, NoTls)
            .await
            .expect("connect to superuser db");
        tokio::spawn(async move {
            let _ = conn.await;
        });

        admin_client
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .await
            .expect("create test database");

        let db = TestDb {
            name,
            admin_url,
            closed: false,
        };
        db.bootstrap_roles().await;
        db
    }

    /// Synchronously drop the database now (terminating any other backends
    /// connected to it first) instead of relying on the best-effort `Drop`
    /// impl. Prefer this at the end of a test.
    pub async fn close(mut self) {
        self.drop_now().await;
        self.closed = true;
    }

    async fn drop_now(&self) {
        if let Ok((client, conn)) = tokio_postgres::connect(&self.admin_url, NoTls).await {
            tokio::spawn(async move {
                let _ = conn.await;
            });
            let name = &self.name;
            let _ = client
                .batch_execute(&format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = '{name}' AND pid <> pg_backend_pid();"
                ))
                .await;
            let _ = client
                .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\""))
                .await;
        }
    }

    /// Connection URL for this test database.
    pub fn url(&self) -> String {
        replace_db_name(&self.admin_url, &self.name)
    }

    /// Connect to this test database (spawns the connection task).
    pub async fn connect(&self) -> Client {
        let (client, conn) = tokio_postgres::connect(&self.url(), NoTls)
            .await
            .expect("connect to test database");
        tokio::spawn(async move {
            let _ = conn.await;
        });
        client
    }

    async fn bootstrap_roles(&self) {
        let (client, conn) = tokio_postgres::connect(&self.url(), NoTls)
            .await
            .expect("connect to test database for bootstrap");
        tokio::spawn(async move {
            let _ = conn.await;
        });

        let connecting_role: String = client
            .query_one("SELECT current_user", &[])
            .await
            .unwrap()
            .get(0);

        for role in ["anon", "authenticated", "service_role"] {
            // Roles are cluster-wide, not per-database, and tests run
            // concurrently against the same cluster, so guard the create
            // with an exception handler rather than a check-then-create
            // (which races between concurrent tests).
            client
                .batch_execute(&format!(
                    "DO $$ BEGIN \
                        CREATE ROLE {role} NOLOGIN; \
                     EXCEPTION WHEN duplicate_object THEN NULL; \
                     END $$;"
                ))
                .await
                .unwrap_or_else(|e| panic!("bootstrap role {role}: {e}"));
            client
                .batch_execute(&format!("GRANT {role} TO \"{connecting_role}\""))
                .await
                .unwrap_or_else(|e| panic!("grant role {role}: {e}"));
        }
    }

    /// Run fixture SQL (may contain multiple statements) against this
    /// database as the connecting (owner) role.
    pub async fn load_fixture(&self, sql: &str) {
        let client = self.connect().await;
        client
            .batch_execute(sql)
            .await
            .unwrap_or_else(|e| panic!("load fixture: {e}"));
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        let admin_url = self.admin_url.clone();
        let name = self.name.clone();
        // Best-effort fallback for a test that didn't call `close()`. Tests
        // run under `#[tokio::test]`, so a Handle is normally available.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Ok((client, conn)) = tokio_postgres::connect(&admin_url, NoTls).await {
                    tokio::spawn(async move {
                        let _ = conn.await;
                    });
                    let _ = client
                        .batch_execute(&format!(
                            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                             WHERE datname = '{name}' AND pid <> pg_backend_pid();"
                        ))
                        .await;
                    let _ = client
                        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\""))
                        .await;
                }
            });
        }
    }
}

fn unique_suffix() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    nanos.wrapping_add(count)
}

fn replace_db_name(url: &str, new_db: &str) -> String {
    let idx = url.rfind('/').expect("url has a path segment");
    let (base, rest) = url.split_at(idx);
    let query_idx = rest.find('?');
    match query_idx {
        Some(qi) => format!("{base}/{new_db}{}", &rest[qi..]),
        None => format!("{base}/{new_db}"),
    }
}

#[allow(dead_code)]
pub fn nothing() {}
