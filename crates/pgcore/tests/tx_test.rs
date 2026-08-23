mod support;

use pgcore::RollbackTx;
use support::TestDb;

#[tokio::test]
async fn rollback_leaves_table_unchanged() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int primary key, name text);")
        .await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();
    tx.execute("INSERT INTO widgets (id, name) VALUES (1, 'a')", &[])
        .await
        .unwrap();
    tx.execute("INSERT INTO widgets (id, name) VALUES (2, 'b')", &[])
        .await
        .unwrap();
    let rows = tx.query("SELECT count(*) FROM widgets", &[]).await.unwrap();
    let count: i64 = rows[0].get(0);
    assert_eq!(count, 2, "rows should be visible inside the transaction");
    tx.finish().await.unwrap();

    let check = db.connect().await;
    let rows = check
        .query("SELECT count(*) FROM widgets", &[])
        .await
        .unwrap();
    let count: i64 = rows[0].get(0);
    assert_eq!(count, 0, "table must be unchanged after ROLLBACK");
    db.close().await;
}

#[tokio::test]
async fn drop_without_finish_still_rolls_back() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int primary key);")
        .await;

    {
        let client = db.connect().await;
        let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();
        tx.execute("INSERT INTO widgets (id) VALUES (1)", &[])
            .await
            .unwrap();
        // tx dropped here without calling finish()
    }

    // Give the best-effort drop-spawned rollback a moment to run.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let check = db.connect().await;
    let rows = check
        .query("SELECT count(*) FROM widgets", &[])
        .await
        .unwrap();
    let count: i64 = rows[0].get(0);
    assert_eq!(
        count, 0,
        "table must be unchanged after an un-finished drop"
    );
    db.close().await;
}

#[tokio::test]
async fn failed_statement_inside_savepoint_does_not_poison_tx() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int primary key, name text not null);")
        .await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();

    let sp = tx.savepoint("probe_1").await.unwrap();
    let err = tx
        .execute("INSERT INTO widgets (id, name) VALUES (1, NULL)", &[])
        .await
        .unwrap_err();
    assert!(err.as_db_error().is_some());
    sp.rollback_to().await.unwrap();

    // The outer transaction must still be usable after rolling back to the
    // savepoint that absorbed the failed statement.
    tx.execute("INSERT INTO widgets (id, name) VALUES (1, 'ok')", &[])
        .await
        .unwrap();
    let rows = tx.query("SELECT count(*) FROM widgets", &[]).await.unwrap();
    let count: i64 = rows[0].get(0);
    assert_eq!(count, 1);

    tx.finish().await.unwrap();
    db.close().await;
}

#[tokio::test]
async fn savepoint_release_keeps_effects_within_outer_rollback() {
    let db = TestDb::create().await;
    db.load_fixture("CREATE TABLE widgets (id int primary key);")
        .await;

    let client = db.connect().await;
    let tx = RollbackTx::begin(client, 5000, 5000).await.unwrap();

    let sp = tx.savepoint("probe_1").await.unwrap();
    tx.execute("INSERT INTO widgets (id) VALUES (1)", &[])
        .await
        .unwrap();
    sp.release().await.unwrap();

    let rows = tx.query("SELECT count(*) FROM widgets", &[]).await.unwrap();
    let count: i64 = rows[0].get(0);
    assert_eq!(count, 1);

    tx.finish().await.unwrap();

    let check = db.connect().await;
    let rows = check
        .query("SELECT count(*) FROM widgets", &[])
        .await
        .unwrap();
    let count: i64 = rows[0].get(0);
    assert_eq!(
        count, 0,
        "outer ROLLBACK must undo a released savepoint too"
    );
    db.close().await;
}
