use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use vectra_exec::Database;
use vectra_server::pgwire::{PG_OID_INT8, PG_OID_TEXT};
use vectra_server::{CliState, PgServer, PgWireClient};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_m09_32_concurrent_clients_extended_query_mvcc_and_cancel() {
    let db = Arc::new(Database::new());
    vectra_tpch::load_tpch_into_database(&db, 0.005).unwrap();

    // Shared transactional table for concurrent MVCC writers.
    {
        let mut s = db.new_session();
        s.execute_sql("CREATE TABLE ledger (client_id BIGINT, seq BIGINT, note VARCHAR);")
            .unwrap();
    }

    let server = Arc::new(PgServer::new(Arc::clone(&db)));
    let (listener, addr) = server.bind("127.0.0.1:0").await.unwrap();
    let shutdown = Arc::new(AtomicBool::new(false));
    let srv_handle = tokio::spawn({
        let s = Arc::clone(&server);
        let sh = Arc::clone(&shutdown);
        async move { s.serve(listener, sh).await }
    });

    // 1. Handshake checks: SSLRequest ('N'), PG compatibility queries, & Extended Query PortalSuspended!
    {
        let mut c = PgWireClient::connect(addr, "vectra", "vectradb", true)
            .await
            .expect("SSLRequest + Startup handshake must succeed");
        assert!(c.pid >= 1000);

        let ver = c.simple_query("SELECT version()").await.unwrap();
        assert_eq!(ver.rows.len(), 1);
        assert!(ver.rows[0][0].as_deref().unwrap().contains("VectraDB"));

        let tbls = c
            .simple_query("SELECT table_name FROM information_schema.tables")
            .await
            .unwrap();
        assert!(tbls.rows.len() >= 9);

        // Extended Query with $1/$2 parameters + PortalSuspended (max_rows = 2 over 5 regions!)
        let p1 = c
            .extended_parse_bind_execute(
                "stmt_reg",
                "portal_reg",
                "SELECT r_regionkey, r_name FROM region WHERE r_regionkey >= $1 AND r_name <> $2 ORDER BY r_regionkey",
                &[PG_OID_INT8, PG_OID_TEXT],
                &[Some("0"), Some("NO_SUCH_REGION")],
                2,
            )
            .await
            .unwrap();
        assert_eq!(p1.rows.len(), 2);
        assert!(
            p1.suspended,
            "first Execute(max_rows=2) must suspend portal!"
        );
        assert_eq!(p1.columns[0].1, PG_OID_INT8);
        assert_eq!(p1.columns[1].1, PG_OID_TEXT);

        let p2 = c.execute_portal("portal_reg", 2).await.unwrap();
        assert_eq!(p2.rows.len(), 2);
        assert!(
            p2.suspended,
            "second Execute(max_rows=2) must suspend portal!"
        );

        let p3 = c.execute_portal("portal_reg", 2).await.unwrap();
        assert_eq!(p3.rows.len(), 1);
        assert!(
            !p3.suspended,
            "final Execute must finish with CommandComplete!"
        );
        assert_eq!(p3.command_tag, "SELECT 5");

        // Intentional errors & exact PostgreSQL SQLSTATE verification + clean ReadyForQuery recovery!
        let err_div = c.simple_query("SELECT 1 / 0").await.unwrap_err();
        assert_eq!(err_div.sqlstate, "22012");
        assert_eq!(err_div.ready_status, b'I');

        let err_tbl = c
            .simple_query("SELECT * FROM does_not_exist")
            .await
            .unwrap_err();
        assert_eq!(err_tbl.sqlstate, "42P01");
        assert_eq!(err_tbl.ready_status, b'I');

        let err_syn = c.simple_query("SELECT FROM WHERE").await.unwrap_err();
        assert_eq!(err_syn.sqlstate, "42601");
        assert_eq!(err_syn.ready_status, b'I');

        // Connection MUST remain completely healthy right after errors!
        let ok_after = c.simple_query("SELECT 42 AS answer").await.unwrap();
        assert_eq!(ok_after.rows[0][0].as_deref(), Some("42"));
        c.terminate().await.unwrap();
    }

    // 2. Launch 32 CONCURRENT async TCP clients performing interleaved TPC-H queries,
    //    MVCC BEGIN/INSERT/COMMIT transactions, Extended Queries, & error recovery!
    let mut handles = Vec::with_capacity(32);
    for cid in 0..32i64 {
        handles.push(tokio::spawn(async move {
            let mut client = PgWireClient::connect(addr, "bench", "vectradb", cid % 2 == 0)
                .await
                .unwrap();

            // Interleaved TPC-H query
            let tpch_q = match cid % 4 {
                0 => vectra_tpch::tpch_query(1).unwrap().sql,
                1 => vectra_tpch::tpch_query(6).unwrap().sql,
                2 => vectra_tpch::tpch_query(12).unwrap().sql,
                _ => vectra_tpch::tpch_query(14).unwrap().sql,
            };
            let qres = client.simple_query(tpch_q).await.unwrap();
            assert!(!qres.rows.is_empty());

            // Explicit MVCC transaction inserting 3 rows per client
            let b = client.simple_query("BEGIN").await.unwrap();
            assert_eq!(b.ready_status, b'T');
            for seq in 0..3i64 {
                client
                    .simple_query(&format!(
                        "INSERT INTO ledger VALUES ({cid}, {seq}, 'client_{cid}_{seq}')"
                    ))
                    .await
                    .unwrap();
            }
            let cm = client.simple_query("COMMIT").await.unwrap();
            assert_eq!(cm.ready_status, b'I');

            // Prepared extended query with parameters
            let ext = client
                .extended_parse_bind_execute(
                    "s1",
                    "p1",
                    "SELECT COUNT(*) FROM nation WHERE n_regionkey = $1",
                    &[PG_OID_INT8],
                    &[Some(&(cid % 5).to_string())],
                    0,
                )
                .await
                .unwrap();
            assert_eq!(ext.rows[0][0].as_deref(), Some("5"));

            // Intentional error + immediate recovery check
            let err = client.simple_query("SELECT 10 / 0").await.unwrap_err();
            assert_eq!(err.sqlstate, "22012");
            let rec = client.simple_query("SELECT 1 + 1").await.unwrap();
            assert_eq!(rec.rows[0][0].as_deref(), Some("2"));

            client.terminate().await.unwrap();
        }));
    }

    for h in handles {
        h.await.expect("concurrent client task panicked");
    }

    // Verify exact committed row count across all 32 concurrent MVCC clients (32 * 3 = 96 rows!)
    {
        let mut verify = PgWireClient::connect(addr, "verify", "vectradb", false)
            .await
            .unwrap();
        let res = verify
            .simple_query("SELECT COUNT(*), COUNT(DISTINCT client_id) FROM ledger")
            .await
            .unwrap();
        assert_eq!(res.rows[0][0].as_deref(), Some("96"));
        assert_eq!(res.rows[0][1].as_deref(), Some("32"));
        verify.terminate().await.unwrap();
    }

    // 3. Real Mid-Flight Out-of-Band `CancelRequest` returning PostgreSQL SQLSTATE 57014!
    {
        let mut victim = PgWireClient::connect(addr, "victim", "vectradb", false)
            .await
            .unwrap();
        let victim_pid = victim.pid;
        let victim_secret = victim.secret_key;

        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            PgWireClient::send_cancel_request(addr, victim_pid, victim_secret)
                .await
                .unwrap();
        });

        // Heavy Cartesian/series query guaranteed to still be executing at 25 ms!
        let cancel_res = victim
            .simple_query(
                "SELECT COUNT(*) FROM generate_series(1, 50000000) WHERE generate_series % 7 = 0",
            )
            .await;
        cancel_task.await.unwrap();

        let err = cancel_res.expect_err("mid-flight CancelRequest must abort query!");
        assert_eq!(
            err.sqlstate, "57014",
            "expected PG query_canceled SQLSTATE 57014, got {} ({})",
            err.sqlstate, err.message
        );

        // Victim connection recovers immediately after cancel!
        let healthy = victim.simple_query("SELECT 777").await.unwrap();
        assert_eq!(healthy.rows[0][0].as_deref(), Some("777"));
        victim.terminate().await.unwrap();
    }

    shutdown.store(true, Ordering::Release);
    let _ = srv_handle.await;

    // 4. Verify CLI meta-commands & Unicode box-drawing / EXPLAIN ANALYZE formatting!
    let mut cli = CliState::new(Arc::clone(&db));
    assert!(cli
        .execute_meta_command(".help")
        .unwrap()
        .unwrap()
        .contains(".tpch"));
    assert!(cli
        .execute_meta_command(".tables")
        .unwrap()
        .unwrap()
        .contains("lineitem"));
    assert!(cli
        .execute_meta_command(".schema region")
        .unwrap()
        .unwrap()
        .contains("r_regionkey"));
    assert!(cli
        .execute_meta_command(".stats region")
        .unwrap()
        .unwrap()
        .contains("RowGroups"));
    let tbl_out = cli
        .execute_sql_formatted("SELECT r_regionkey, r_name FROM region ORDER BY 1")
        .unwrap();
    assert!(tbl_out.contains("┌") && tbl_out.contains("EUROPE"));
    let exp_out = cli
        .execute_sql_formatted("EXPLAIN ANALYZE SELECT COUNT(*) FROM nation")
        .unwrap();
    assert!(exp_out.contains("EXPLAIN ANALYZE") && exp_out.contains("act="));
}
