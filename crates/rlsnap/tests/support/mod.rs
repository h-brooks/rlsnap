//! Shared integration test support: spins up a fresh, uniquely-named
//! database per test on the shared Postgres server, bootstraps the
//! Supabase-style roles, writes a scratch `rlsnap.toml` + working directory,
//! and drives `rlsnap::run` (the single library seam).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio_postgres::{Client, NoTls};

/// Serializes `std::env::set_var` calls across concurrently-running tests.
/// Each test uses a unique variable name, so this only needs to prevent a
/// torn write to the process environment table itself, not logical
/// collisions between tests.
static ENV_LOCK: Mutex<()> = Mutex::new(());

pub fn superuser_url() -> String {
    std::env::var("PG_TEST_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54390/postgres".to_string())
}

fn unique_suffix() -> u64 {
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

/// A throwaway database, dropped when this guard is dropped (or explicitly
/// via `close()`).
pub struct TestDb {
    name: String,
    admin_url: String,
    closed: bool,
}

impl TestDb {
    pub async fn create() -> TestDb {
        let admin_url = superuser_url();
        let name = format!("rlsnap_test_{}_{}", std::process::id(), unique_suffix());

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

    pub fn url(&self) -> String {
        replace_db_name(&self.admin_url, &self.name)
    }

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

/// A scratch working directory (holding `rlsnap.toml`) plus the uniquely
/// named env var that the config's `[targets.test]` reads its URL from.
pub struct TestHarness {
    pub db: TestDb,
    pub dir: PathBuf,
    #[allow(dead_code)]
    pub url_env_name: String,
}

impl TestHarness {
    /// Create a fresh database, load `fixture_sql`, and write an
    /// `rlsnap.toml` built from `config_body` (everything except the
    /// `[targets.test]` table, which this appends with a unique
    /// `url_env`). `mode` is `"behavioural"` or `"catalog"`.
    pub async fn new(fixture_sql: &str, config_body: &str, mode: &str, max_rows: u32) -> Self {
        let db = TestDb::create().await;
        db.load_fixture(fixture_sql).await;

        let suffix = unique_suffix();
        let url_env_name = format!("RLSNAP_TEST_URL_{suffix}");
        {
            let _guard = ENV_LOCK.lock().unwrap();
            // SAFETY: this test binary runs many tests concurrently as
            // threads within one process, but each test uses a distinct
            // variable name, so no two tests observe or race on the same
            // key; the lock only prevents a torn write to the environment
            // table itself.
            unsafe {
                std::env::set_var(&url_env_name, db.url());
            }
        }

        let dir = std::env::temp_dir().join(format!("rlsnap_test_dir_{suffix}"));
        std::fs::create_dir_all(&dir).unwrap();
        let toml = format!(
            "{config_body}\n\n[targets.test]\nurl_env = \"{url_env_name}\"\nmode = \"{mode}\"\nmax_rows = {max_rows}\n"
        );
        std::fs::write(dir.join("rlsnap.toml"), toml).unwrap();

        TestHarness {
            db,
            dir,
            url_env_name,
        }
    }

    /// Run `rlsnap::run` against this harness's working directory.
    pub async fn run(&self, args: &[&str]) -> anyhow::Result<i32> {
        let args: Vec<String> = std::iter::once("rlsnap".to_string())
            .chain(args.iter().map(|s| s.to_string()))
            .collect();
        rlsnap::run(&args, &self.dir).await
    }

    #[allow(dead_code)]
    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub async fn close(self) {
        self.db.close().await;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[allow(dead_code)]
pub fn nothing() {}
