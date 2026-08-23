//! Shared test support: spins up a fresh, uniquely-named database per test
//! on the shared Postgres server, bootstraps the Supabase-style roles, and
//! drops it on close. Also helpers for writing a temp config file / temp
//! migration file and running the built `rehearse` binary.
//!
//! This module is compiled fresh into each integration test binary, and no
//! single test file uses every helper here, so dead-code warnings from the
//! unused subset in any one binary are expected and suppressed.
#![allow(dead_code)]

use std::path::PathBuf;

use tokio_postgres::{Client, NoTls};

pub fn superuser_url() -> String {
    std::env::var("PG_TEST_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@127.0.0.1:54390/postgres".to_string())
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

/// A throwaway database, dropped when `close()` is called (best-effort on
/// `Drop` otherwise).
pub struct TestDb {
    name: String,
    admin_url: String,
    closed: bool,
}

impl TestDb {
    pub async fn create() -> TestDb {
        let admin_url = superuser_url();
        let name = format!("rehearse_test_{}_{}", std::process::id(), unique_suffix());

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

/// A scratch directory for one test's temp files (config toml, migration
/// sql, snapshot json), removed on drop.
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    pub fn new() -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "rehearse_test_scratch_{}_{}",
            std::process::id(),
            unique_suffix()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch { dir }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    pub fn write(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, contents).expect("write scratch file");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A unique environment variable name to hand a test's database URL to the
/// config file, so parallel tests never collide on the same var.
pub fn unique_env_var_name() -> String {
    format!("REHEARSE_TEST_DB_URL_{}", unique_suffix())
}

/// Write a minimal single-target config file naming `target_name`, pointed
/// at `url_env`, with the given optional `lock_timeout_ms` override.
pub fn write_config(
    scratch: &Scratch,
    target_name: &str,
    url_env: &str,
    lock_timeout_ms: Option<u64>,
) -> PathBuf {
    let mut toml =
        format!("[targets.{target_name}]\nurl_env = \"{url_env}\"\nmode = \"behavioural\"\n");
    if let Some(ms) = lock_timeout_ms {
        toml.push_str(&format!("lock_timeout_ms = {ms}\n"));
    }
    scratch.write("rlsnap.toml", &toml)
}

pub struct BinResult {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Run the built `rehearse` binary with `args` (not including the program
/// name) and `envs` set on the child process only (never the test process
/// itself, so parallel tests never race on shared env vars), returning its
/// exit code and captured stdout/stderr.
pub fn run_bin(args: &[&str], envs: &[(&str, &str)]) -> BinResult {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rehearse"))
        .args(args)
        .envs(envs.iter().copied())
        .output()
        .expect("run rehearse binary");
    BinResult {
        status: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
    }
}
