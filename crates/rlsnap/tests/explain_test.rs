//! `rlsnap explain` prints the grants/policies behind one cell.

mod support;

use support::TestHarness;

#[tokio::test]
async fn explain_names_the_policy_and_grant_behind_a_cell() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key, owner text); \
         ALTER TABLE widgets ENABLE ROW LEVEL SECURITY; \
         GRANT SELECT ON widgets TO authenticated; \
         CREATE POLICY widgets_owner_select ON widgets FOR SELECT TO authenticated \
            USING (owner = current_setting('request.jwt.claim.sub', true));",
        "schemas = [\"public\"]\n\n[[personas]]\nname = \"staff\"\nrole = \"authenticated\"\nclaims = { sub = \"staff-1\" }\n",
        "behavioural",
        1000,
    )
    .await;

    let code = h
        .run(&[
            "explain",
            "staff",
            "public.widgets",
            "select",
            "--target",
            "test",
        ])
        .await
        .unwrap();
    assert_eq!(code, 0);

    h.close().await;
}

#[tokio::test]
async fn explain_rejects_unknown_op() {
    let h = TestHarness::new(
        "CREATE TABLE widgets (id int primary key);",
        "schemas = [\"public\"]\n",
        "behavioural",
        1000,
    )
    .await;

    let result = h
        .run(&[
            "explain",
            "anon",
            "public.widgets",
            "yolo",
            "--target",
            "test",
        ])
        .await;
    assert!(result.is_err());

    h.close().await;
}
