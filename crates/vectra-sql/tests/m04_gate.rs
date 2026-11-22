use std::collections::HashMap;
use vectra_sql::*;
use vectra_storage::Catalog;
use vectra_types::*;

fn build_tpch_catalog() -> Catalog {
    let cat = Catalog::new();
    let dec = LogicalType::Decimal128 {
        precision: 15,
        scale: 2,
    };
    cat.create_table(
        "region",
        Schema::new(vec![
            Field::new("r_regionkey", LogicalType::Int64, false),
            Field::new("r_name", LogicalType::Utf8, false),
            Field::new("r_comment", LogicalType::Utf8, true),
        ]),
        false,
    )
    .unwrap();
    cat.create_table(
        "nation",
        Schema::new(vec![
            Field::new("n_nationkey", LogicalType::Int64, false),
            Field::new("n_name", LogicalType::Utf8, false),
            Field::new("n_regionkey", LogicalType::Int64, false),
            Field::new("n_comment", LogicalType::Utf8, true),
        ]),
        false,
    )
    .unwrap();
    cat.create_table(
        "supplier",
        Schema::new(vec![
            Field::new("s_suppkey", LogicalType::Int64, false),
            Field::new("s_name", LogicalType::Utf8, false),
            Field::new("s_address", LogicalType::Utf8, false),
            Field::new("s_nationkey", LogicalType::Int64, false),
            Field::new("s_phone", LogicalType::Utf8, false),
            Field::new("s_acctbal", dec, false),
            Field::new("s_comment", LogicalType::Utf8, true),
        ]),
        false,
    )
    .unwrap();
    cat.create_table(
        "customer",
        Schema::new(vec![
            Field::new("c_custkey", LogicalType::Int64, false),
            Field::new("c_name", LogicalType::Utf8, false),
            Field::new("c_address", LogicalType::Utf8, false),
            Field::new("c_nationkey", LogicalType::Int64, false),
            Field::new("c_phone", LogicalType::Utf8, false),
            Field::new("c_acctbal", dec, false),
            Field::new("c_mktsegment", LogicalType::Utf8, false),
            Field::new("c_comment", LogicalType::Utf8, true),
        ]),
        false,
    )
    .unwrap();
    cat.create_table(
        "part",
        Schema::new(vec![
            Field::new("p_partkey", LogicalType::Int64, false),
            Field::new("p_name", LogicalType::Utf8, false),
            Field::new("p_mfgr", LogicalType::Utf8, false),
            Field::new("p_brand", LogicalType::Utf8, false),
            Field::new("p_type", LogicalType::Utf8, false),
            Field::new("p_size", LogicalType::Int64, false),
            Field::new("p_container", LogicalType::Utf8, false),
            Field::new("p_retailprice", dec, false),
            Field::new("p_comment", LogicalType::Utf8, true),
        ]),
        false,
    )
    .unwrap();
    cat.create_table(
        "partsupp",
        Schema::new(vec![
            Field::new("ps_partkey", LogicalType::Int64, false),
            Field::new("ps_suppkey", LogicalType::Int64, false),
            Field::new("ps_availqty", LogicalType::Int64, false),
            Field::new("ps_supplycost", dec, false),
            Field::new("ps_comment", LogicalType::Utf8, true),
        ]),
        false,
    )
    .unwrap();
    cat.create_table(
        "orders",
        Schema::new(vec![
            Field::new("o_orderkey", LogicalType::Int64, false),
            Field::new("o_custkey", LogicalType::Int64, false),
            Field::new("o_orderstatus", LogicalType::Utf8, false),
            Field::new("o_totalprice", dec, false),
            Field::new("o_orderdate", LogicalType::Date32, false),
            Field::new("o_orderpriority", LogicalType::Utf8, false),
            Field::new("o_clerk", LogicalType::Utf8, false),
            Field::new("o_shippriority", LogicalType::Int64, false),
            Field::new("o_comment", LogicalType::Utf8, true),
        ]),
        false,
    )
    .unwrap();
    cat.create_table(
        "lineitem",
        Schema::new(vec![
            Field::new("l_orderkey", LogicalType::Int64, false),
            Field::new("l_partkey", LogicalType::Int64, false),
            Field::new("l_suppkey", LogicalType::Int64, false),
            Field::new("l_linenumber", LogicalType::Int64, false),
            Field::new("l_quantity", dec, false),
            Field::new("l_extendedprice", dec, false),
            Field::new("l_discount", dec, false),
            Field::new("l_tax", dec, false),
            Field::new("l_returnflag", LogicalType::Utf8, false),
            Field::new("l_linestatus", LogicalType::Utf8, false),
            Field::new("l_shipdate", LogicalType::Date32, false),
            Field::new("l_commitdate", LogicalType::Date32, false),
            Field::new("l_receiptdate", LogicalType::Date32, false),
            Field::new("l_shipinstruct", LogicalType::Utf8, false),
            Field::new("l_shipmode", LogicalType::Utf8, false),
            Field::new("l_comment", LogicalType::Utf8, true),
        ]),
        false,
    )
    .unwrap();
    cat
}

const TPCH_22: [&str; 22] = [
    // Q1
    "SELECT l_returnflag, l_linestatus, SUM(l_quantity) AS sum_qty, SUM(l_extendedprice) AS sum_base_price,
            SUM(l_extendedprice * (1 - l_discount)) AS sum_disc_price,
            SUM(l_extendedprice * (1 - l_discount) * (1 + l_tax)) AS sum_charge,
            AVG(l_quantity) AS avg_qty, AVG(l_extendedprice) AS avg_price, AVG(l_discount) AS avg_disc,
            COUNT(*) AS count_order
     FROM lineitem WHERE l_shipdate <= DATE '1998-12-01' - INTERVAL '90' DAY
     GROUP BY l_returnflag, l_linestatus ORDER BY l_returnflag, l_linestatus",
    // Q2 (correlated scalar min subquery)
    "SELECT s_acctbal, s_name, n_name, p_partkey, p_mfgr, s_address, s_phone, s_comment
     FROM part, supplier, partsupp, nation, region
     WHERE p_partkey = ps_partkey AND s_suppkey = ps_suppkey AND p_size = 15 AND p_type LIKE '%BRASS'
       AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'EUROPE'
       AND ps_supplycost = (
           SELECT MIN(ps_supplycost) FROM partsupp, supplier, nation, region
           WHERE p_partkey = ps_partkey AND s_suppkey = ps_suppkey
             AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey AND r_name = 'EUROPE'
       )
     ORDER BY s_acctbal DESC, n_name, s_name, p_partkey LIMIT 100",
    // Q3
    "SELECT l_orderkey, SUM(l_extendedprice * (1 - l_discount)) AS revenue, o_orderdate, o_shippriority
     FROM customer, orders, lineitem
     WHERE c_mktsegment = 'BUILDING' AND c_custkey = o_custkey AND l_orderkey = o_orderkey
       AND o_orderdate < DATE '1995-03-15' AND l_shipdate > DATE '1995-03-15'
     GROUP BY l_orderkey, o_orderdate, o_shippriority ORDER BY revenue DESC, o_orderdate LIMIT 10",
    // Q4 (correlated EXISTS)
    "SELECT o_orderpriority, COUNT(*) AS order_count FROM orders
     WHERE o_orderdate >= DATE '1993-07-01' AND o_orderdate < DATE '1993-07-01' + INTERVAL '3' MONTH
       AND EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_commitdate < l_receiptdate)
     GROUP BY o_orderpriority ORDER BY o_orderpriority",
    // Q5
    "SELECT n_name, SUM(l_extendedprice * (1 - l_discount)) AS revenue
     FROM customer, orders, lineitem, supplier, nation, region
     WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND l_suppkey = s_suppkey
       AND c_nationkey = s_nationkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey
       AND r_name = 'ASIA' AND o_orderdate >= DATE '1994-01-01' AND o_orderdate < DATE '1994-01-01' + INTERVAL '1' YEAR
     GROUP BY n_name ORDER BY revenue DESC",
    // Q6
    "SELECT SUM(l_extendedprice * l_discount) AS revenue FROM lineitem
     WHERE l_shipdate >= DATE '1994-01-01' AND l_shipdate < DATE '1994-01-01' + INTERVAL '1' YEAR
       AND l_discount BETWEEN 0.05 AND 0.07 AND l_quantity < 24",
    // Q7
    "SELECT supp_nation, cust_nation, l_year, SUM(volume) AS revenue FROM (
         SELECT n1.n_name AS supp_nation, n2.n_name AS cust_nation, EXTRACT(YEAR FROM l_shipdate) AS l_year,
                l_extendedprice * (1 - l_discount) AS volume
         FROM supplier, lineitem, orders, customer, nation n1, nation n2
         WHERE s_suppkey = l_suppkey AND o_orderkey = l_orderkey AND c_custkey = o_custkey
           AND s_nationkey = n1.n_nationkey AND c_nationkey = n2.n_nationkey
           AND ((n1.n_name = 'FRANCE' AND n2.n_name = 'GERMANY') OR (n1.n_name = 'GERMANY' AND n2.n_name = 'FRANCE'))
           AND l_shipdate BETWEEN DATE '1995-01-01' AND DATE '1996-12-31'
     ) AS shipping GROUP BY supp_nation, cust_nation, l_year ORDER BY supp_nation, cust_nation, l_year",
    // Q8
    "SELECT o_year, SUM(CASE WHEN nation = 'BRAZIL' THEN volume ELSE 0 END) / SUM(volume) AS mkt_share FROM (
         SELECT EXTRACT(YEAR FROM o_orderdate) AS o_year, l_extendedprice * (1 - l_discount) AS volume, n2.n_name AS nation
         FROM part, supplier, lineitem, orders, customer, nation n1, nation n2, region
         WHERE p_partkey = l_partkey AND s_suppkey = l_suppkey AND l_orderkey = o_orderkey
           AND o_custkey = c_custkey AND c_nationkey = n1.n_nationkey AND n1.n_regionkey = r_regionkey
           AND r_name = 'AMERICA' AND s_nationkey = n2.n_nationkey
           AND o_orderdate BETWEEN DATE '1995-01-01' AND DATE '1996-12-31' AND p_type = 'ECONOMY ANODIZED STEEL'
     ) AS all_nations GROUP BY o_year ORDER BY o_year",
    // Q9
    "SELECT nation, o_year, SUM(amount) AS sum_profit FROM (
         SELECT n_name AS nation, EXTRACT(YEAR FROM o_orderdate) AS o_year,
                l_extendedprice * (1 - l_discount) - ps_supplycost * l_quantity AS amount
         FROM part, supplier, lineitem, partsupp, orders, nation
         WHERE s_suppkey = l_suppkey AND ps_suppkey = l_suppkey AND ps_partkey = l_partkey
           AND p_partkey = l_partkey AND o_orderkey = l_orderkey AND s_nationkey = n_nationkey
           AND p_name LIKE '%green%'
     ) AS profit GROUP BY nation, o_year ORDER BY nation, o_year DESC",
    // Q10
    "SELECT c_custkey, c_name, SUM(l_extendedprice * (1 - l_discount)) AS revenue, c_acctbal, n_name, c_address, c_phone, c_comment
     FROM customer, orders, lineitem, nation
     WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey
       AND o_orderdate >= DATE '1993-10-01' AND o_orderdate < DATE '1993-10-01' + INTERVAL '3' MONTH
       AND l_returnflag = 'R' AND c_nationkey = n_nationkey
     GROUP BY c_custkey, c_name, c_acctbal, c_phone, n_name, c_address, c_comment ORDER BY revenue DESC LIMIT 20",
    // Q11 (HAVING scalar subquery)
    "SELECT ps_partkey, SUM(ps_supplycost * ps_availqty) AS value
     FROM partsupp, supplier, nation
     WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = 'GERMANY'
     GROUP BY ps_partkey
     HAVING SUM(ps_supplycost * ps_availqty) > (
         SELECT SUM(ps_supplycost * ps_availqty) * 0.0001 FROM partsupp, supplier, nation
         WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = 'GERMANY'
     ) ORDER BY value DESC",
    // Q12
    "SELECT l_shipmode,
            SUM(CASE WHEN o_orderpriority = '1-URGENT' OR o_orderpriority = '2-HIGH' THEN 1 ELSE 0 END) AS high_line_count,
            SUM(CASE WHEN o_orderpriority <> '1-URGENT' AND o_orderpriority <> '2-HIGH' THEN 1 ELSE 0 END) AS low_line_count
     FROM orders, lineitem
     WHERE o_orderkey = l_orderkey AND l_shipmode IN ('MAIL', 'SHIP')
       AND l_commitdate < l_receiptdate AND l_shipdate < l_commitdate
       AND l_receiptdate >= DATE '1994-01-01' AND l_receiptdate < DATE '1994-01-01' + INTERVAL '1' YEAR
     GROUP BY l_shipmode ORDER BY l_shipmode",
    // Q13
    "SELECT c_count, COUNT(*) AS custdist FROM (
         SELECT c_custkey, COUNT(o_orderkey) AS c_count
         FROM customer LEFT OUTER JOIN orders ON c_custkey = o_custkey AND o_comment NOT LIKE '%special%requests%'
         GROUP BY c_custkey
     ) AS c_orders (c_custkey, c_count) GROUP BY c_count ORDER BY custdist DESC, c_count DESC",
    // Q14
    "SELECT 100.00 * SUM(CASE WHEN p_type LIKE 'PROMO%' THEN l_extendedprice * (1 - l_discount) ELSE 0 END) /
            SUM(l_extendedprice * (1 - l_discount)) AS promo_revenue
     FROM lineitem, part WHERE l_partkey = p_partkey
       AND l_shipdate >= DATE '1995-09-01' AND l_shipdate < DATE '1995-09-01' + INTERVAL '1' MONTH",
    // Q15 (CTE equivalent)
    "WITH revenue0 (supplier_no, total_revenue) AS (
         SELECT l_suppkey, SUM(l_extendedprice * (1 - l_discount)) FROM lineitem
         WHERE l_shipdate >= DATE '1996-01-01' AND l_shipdate < DATE '1996-01-01' + INTERVAL '3' MONTH
         GROUP BY l_suppkey
     )
     SELECT s_suppkey, s_name, s_address, s_phone, total_revenue FROM supplier, revenue0
     WHERE s_suppkey = supplier_no AND total_revenue = (SELECT MAX(total_revenue) FROM revenue0)
     ORDER BY s_suppkey",
    // Q16 (NOT IN subquery + COUNT(DISTINCT))
    "SELECT p_brand, p_type, p_size, COUNT(DISTINCT ps_suppkey) AS supplier_cnt
     FROM partsupp, part
     WHERE p_partkey = ps_partkey AND p_brand <> 'Brand#45' AND p_type NOT LIKE 'MEDIUM POLISHED%'
       AND p_size IN (49, 14, 23, 45, 19, 3, 36, 9)
       AND ps_suppkey NOT IN (SELECT s_suppkey FROM supplier WHERE s_comment LIKE '%Customer%Complaints%')
     GROUP BY p_brand, p_type, p_size ORDER BY supplier_cnt DESC, p_brand, p_type, p_size",
    // Q17 (correlated AVG subquery)
    "SELECT SUM(l_extendedprice) / 7.0 AS avg_yearly FROM lineitem, part
     WHERE p_partkey = l_partkey AND p_brand = 'Brand#23' AND p_container = 'MED BOX'
       AND l_quantity < (SELECT 0.2 * AVG(l_quantity) FROM lineitem WHERE l_partkey = p_partkey)",
    // Q18 (IN subquery with GROUP BY HAVING)
    "SELECT c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice, SUM(l_quantity)
     FROM customer, orders, lineitem
     WHERE o_orderkey IN (SELECT l_orderkey FROM lineitem GROUP BY l_orderkey HAVING SUM(l_quantity) > 300)
       AND c_custkey = o_custkey AND o_orderkey = l_orderkey
     GROUP BY c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice ORDER BY o_totalprice DESC, o_orderdate LIMIT 100",
    // Q19
    "SELECT SUM(l_extendedprice * (1 - l_discount)) AS revenue FROM lineitem, part
     WHERE (p_partkey = l_partkey AND p_brand = 'Brand#12' AND p_container IN ('SM CASE', 'SM BOX', 'SM PACK', 'SM PKG')
            AND l_quantity >= 1 AND l_quantity <= 11 AND p_size BETWEEN 1 AND 5
            AND l_shipmode IN ('AIR', 'AIR REG') AND l_shipinstruct = 'DELIVER IN PERSON')
        OR (p_partkey = l_partkey AND p_brand = 'Brand#23' AND p_container IN ('MED BAG', 'MED BOX', 'MED PKG', 'MED PACK')
            AND l_quantity >= 10 AND l_quantity <= 20 AND p_size BETWEEN 1 AND 10
            AND l_shipmode IN ('AIR', 'AIR REG') AND l_shipinstruct = 'DELIVER IN PERSON')
        OR (p_partkey = l_partkey AND p_brand = 'Brand#34' AND p_container IN ('LG CASE', 'LG BOX', 'LG PACK', 'LG PKG')
            AND l_quantity >= 20 AND l_quantity <= 30 AND p_size BETWEEN 1 AND 15
            AND l_shipmode IN ('AIR', 'AIR REG') AND l_shipinstruct = 'DELIVER IN PERSON')",
    // Q20 (nested IN + correlated SUM subquery)
    "SELECT s_name, s_address FROM supplier, nation
     WHERE s_suppkey IN (
         SELECT ps_suppkey FROM partsupp
         WHERE ps_partkey IN (SELECT p_partkey FROM part WHERE p_name LIKE 'forest%')
           AND ps_availqty > (
               SELECT 0.5 * SUM(l_quantity) FROM lineitem
               WHERE l_partkey = ps_partkey AND l_suppkey = ps_suppkey
                 AND l_shipdate >= DATE '1994-01-01' AND l_shipdate < DATE '1994-01-01' + INTERVAL '1' YEAR
           )
     ) AND s_nationkey = n_nationkey AND n_name = 'CANADA' ORDER BY s_name",
    // Q21 (correlated EXISTS + NOT EXISTS)
    "SELECT s_name, COUNT(*) AS numwait FROM supplier, lineitem l1, orders, nation
     WHERE s_suppkey = l1.l_suppkey AND o_orderkey = l1.l_orderkey AND o_orderstatus = 'F'
       AND l1.l_receiptdate > l1.l_commitdate
       AND EXISTS (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey AND l2.l_suppkey <> l1.l_suppkey)
       AND NOT EXISTS (SELECT * FROM lineitem l3 WHERE l3.l_orderkey = l1.l_orderkey AND l3.l_suppkey <> l1.l_suppkey AND l3.l_receiptdate > l3.l_commitdate)
       AND s_nationkey = n_nationkey AND n_name = 'SAUDI ARABIA'
     GROUP BY s_name ORDER BY numwait DESC, s_name LIMIT 100",
    // Q22 (SUBSTRING + scalar AVG + correlated NOT EXISTS)
    "SELECT cntrycode, COUNT(*) AS numcust, SUM(c_acctbal) AS totacctbal FROM (
         SELECT SUBSTRING(c_phone FROM 1 FOR 2) AS cntrycode, c_acctbal FROM customer
         WHERE SUBSTRING(c_phone FROM 1 FOR 2) IN ('13', '31', '23', '29', '30', '18', '17')
           AND c_acctbal > (
               SELECT AVG(c_acctbal) FROM customer
               WHERE c_acctbal > 0.00 AND SUBSTRING(c_phone FROM 1 FOR 2) IN ('13', '31', '23', '29', '30', '18', '17')
           )
           AND NOT EXISTS (SELECT * FROM orders WHERE o_custkey = c_custkey)
     ) AS custsale GROUP BY cntrycode ORDER BY cntrycode",
];

#[test]
fn test_tpch_22_parse_pretty_bind_and_unnest_proof() {
    let cat = build_tpch_catalog();
    let views = HashMap::new();

    for (idx, sql) in TPCH_22.iter().enumerate() {
        let qnum = idx + 1;
        // 1. Parse + Pretty-Print AST Idempotence
        let ast1 = parse_single_statement(sql)
            .unwrap_or_else(|e| panic!("TPC-H Q{qnum} parse failed: {e}"));
        let printed = pretty_print_statement(&ast1);
        let ast2 = parse_single_statement(&printed)
            .unwrap_or_else(|e| panic!("TPC-H Q{qnum} re-parse failed: {e}\nSQL:\n{printed}"));
        assert_eq!(
            ast1, ast2,
            "TPC-H Q{qnum} AST pretty-print idempotence failed!"
        );

        // 2. Bind
        let mut binder = Binder::new(&cat, &views);
        let bound = binder
            .bind_statement(&ast1)
            .unwrap_or_else(|e| panic!("TPC-H Q{qnum} bind failed: {e}"));
        let LogicalStatement::Query(plan) = bound else {
            panic!("expected Query");
        };

        // 3. Neumann-Kemper Arbitrary Subquery Unnesting
        let mut col_gen = ColIdGen::new();
        col_gen.set_min_next(binder.next_col_id());
        let unnested = unnest_plan(*plan, &mut col_gen)
            .unwrap_or_else(|e| panic!("TPC-H Q{qnum} unnest failed: {e}"));

        assert!(
            !unnested.contains_dependent_join(),
            "TPC-H Q{qnum} still contains DependentJoin after unnesting!\n{}",
            unnested.format_tree()
        );
        assert!(
            unnested.free_correlated_vars().is_empty(),
            "TPC-H Q{qnum} still has free correlated vars {:?}!\n{}",
            unnested.free_correlated_vars(),
            unnested.format_tree()
        );
    }
}

#[test]
fn test_mark_join_or_in_and_3_level_nested_unnesting() {
    let cat = Catalog::new();
    for t in ["t1", "t2", "t3"] {
        cat.create_table(
            t,
            Schema::new(vec![
                Field::new("a", LogicalType::Int64, true),
                Field::new("b", LogicalType::Int64, true),
                Field::new("c", LogicalType::Int64, true),
                Field::new("x", LogicalType::Int64, true),
            ]),
            false,
        )
        .unwrap();
    }
    let views = HashMap::new();

    let tricky_queries = [
        // Requires LeftMark join to preserve rows where c = 5 even when IN subquery returns FALSE/NULL!
        "SELECT a FROM t1 WHERE a IN (SELECT b FROM t2 WHERE t2.x = t1.x) OR c = 5",
        // Correlated scalar COUNT(*) subquery (Neumann-Kemper COUNT-bug case!)
        "SELECT a, (SELECT COUNT(*) FROM t2 WHERE t2.x = t1.x) AS cnt FROM t1",
        // 3-level nested correlated subquery referencing outer t1 from innermost t3!
        "SELECT t1.a FROM t1 WHERE EXISTS (
             SELECT 1 FROM t2 WHERE t2.a = t1.a AND EXISTS (
                 SELECT 1 FROM t3 WHERE t3.x = t1.x AND t3.b = t2.b
             )
         )",
        // Correlated NOT IN + OR EXISTS combined
        "SELECT a FROM t1 WHERE a NOT IN (SELECT b FROM t2 WHERE t2.x = t1.x) OR EXISTS (SELECT 1 FROM t3 WHERE t3.a = t1.a)",
        // Correlated subquery with LIMIT 1
        "SELECT t1.a, (SELECT t2.b FROM t2 WHERE t2.x = t1.x ORDER BY t2.a DESC LIMIT 1) AS top_b FROM t1",
    ];

    for (i, sql) in tricky_queries.iter().enumerate() {
        let ast = parse_single_statement(sql).unwrap();
        let mut binder = Binder::new(&cat, &views);
        let LogicalStatement::Query(plan) = binder.bind_statement(&ast).unwrap() else {
            panic!();
        };
        assert!(
            plan.contains_dependent_join(),
            "tricky query {i} should bind with DependentJoin before unnesting"
        );
        let mut cg = ColIdGen::new();
        cg.set_min_next(binder.next_col_id());
        let unnested = unnest_plan(*plan, &mut cg).unwrap();
        assert!(
            !unnested.contains_dependent_join(),
            "tricky query {i} still has DependentJoin!\n{}",
            unnested.format_tree()
        );
        assert!(
            unnested.free_correlated_vars().is_empty(),
            "tricky query {i} still has free correlated vars!\n{}",
            unnested.format_tree()
        );
    }
}

#[test]
fn test_1000_plus_grammar_and_binder_positive_and_negative_cases() {
    let cat = build_tpch_catalog();
    let views = HashMap::new();

    // Verify INTERSECT binds tighter than UNION
    let set_ast = parse_single_statement("SELECT 1 UNION SELECT 2 INTERSECT SELECT 2").unwrap();
    if let Statement::Query(q) = &set_ast {
        match q.body.as_ref() {
            SetExpr::SetOperation {
                op: SetOperator::Union,
                right,
                ..
            } => {
                assert!(matches!(
                    right.as_ref(),
                    SetExpr::SetOperation {
                        op: SetOperator::Intersect,
                        ..
                    }
                ));
            }
            other => panic!("expected UNION(..., INTERSECT(...)), got {other:?}"),
        }
    }

    // Verify negative binder cases (ambiguous col, agg in WHERE, ungrouped col, cyclic CTE, union arity)
    let negative_binder_cases = [
        ("SELECT r_name FROM region r1, region r2", "ambiguous"),
        ("SELECT r_name FROM region WHERE COUNT(*) > 1", "WHERE"),
        ("SELECT r_regionkey, COUNT(*) FROM region", "GROUP BY"),
        (
            "WITH a AS (SELECT * FROM b), b AS (SELECT * FROM a) SELECT * FROM a",
            "cyclic",
        ),
        ("SELECT 1, 2 UNION SELECT 3", "arity"),
        ("SELECT nonexistent_col FROM region", "does not exist"),
        ("SELECT * FROM nonexistent_table", "does not exist"),
        (
            "SELECT r_name FROM region WHERE ROW_NUMBER() OVER () = 1",
            "WHERE",
        ),
    ];
    for (sql, expected_substr) in negative_binder_cases {
        let ast = parse_single_statement(sql).unwrap();
        let mut binder = Binder::new(&cat, &views);
        let err = binder.bind_statement(&ast).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.to_lowercase().contains(&expected_substr.to_lowercase()),
            "SQL `{sql}` error `{msg}` missing expected `{expected_substr}`"
        );
    }

    // Verify rich span diagnostics on syntax errors
    let bad_sql = "SELECT a, b\nFROM region\nWHERE a = ;";
    let err = parse_single_statement(bad_sql).unwrap_err();
    let err_str = err.to_string();
    assert!(
        err_str.contains("3:11") && err_str.contains("^"),
        "diagnostic: {err_str}"
    );

    // 1,100+ positive grammar, DDL, DML, Window, Rollup, Cube, Grouping Sets, Recursive CTE, EXPLAIN tests
    let templates = [
        "SELECT r_regionkey, r_name FROM region WHERE r_regionkey = {i} ORDER BY 1 DESC NULLS LAST LIMIT 5 OFFSET 1",
        "SELECT n_regionkey, COUNT(*), SUM(n_nationkey) FROM nation GROUP BY ROLLUP(n_regionkey)",
        "SELECT n_regionkey, n_name, GROUPING(n_regionkey) FROM nation GROUP BY CUBE(n_regionkey, n_name)",
        "SELECT n_regionkey, COUNT(*) FROM nation GROUP BY GROUPING SETS ((n_regionkey), ())",
        "SELECT n_name, ROW_NUMBER() OVER (PARTITION BY n_regionkey ORDER BY n_nationkey DESC ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING) AS rn FROM nation QUALIFY rn <= 3",
        "SELECT n_name, SUM(n_nationkey) OVER w FROM nation WINDOW w AS (PARTITION BY n_regionkey ORDER BY n_nationkey)",
        "WITH RECURSIVE cnt(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM cnt WHERE x < {i}) SELECT x FROM cnt",
        "SELECT a.n_name FROM nation a INNER JOIN region b ON a.n_regionkey = b.r_regionkey WHERE a.n_nationkey BETWEEN 1 AND {i}",
        "SELECT * FROM nation LEFT SEMI JOIN region ON n_regionkey = r_regionkey",
        "SELECT * FROM nation LEFT ANTI JOIN region ON n_regionkey = r_regionkey",
        "CREATE TABLE IF NOT EXISTS tmp_{i} (id BIGINT NOT NULL, val DECIMAL(12, 2), dt DATE, name VARCHAR(50))",
        "INSERT INTO region (r_regionkey, r_name, r_comment) VALUES ({i}, 'R', 'ok')",
        "UPDATE region SET r_comment = 'updated' WHERE r_regionkey = {i}",
        "DELETE FROM region WHERE r_regionkey = {i}",
        "EXPLAIN UNNESTED SELECT r_name FROM region WHERE r_regionkey = {i}",
    ];
    for i in 1..=80usize {
        for tmpl in &templates {
            let sql = tmpl.replace("{i}", &i.to_string());
            let ast =
                parse_single_statement(&sql).unwrap_or_else(|e| panic!("failed `{sql}`: {e}"));
            let mut binder = Binder::new(&cat, &views);
            let _ = binder
                .bind_statement(&ast)
                .unwrap_or_else(|e| panic!("bind failed `{sql}`: {e}"));
        }
    }
}

#[test]
fn test_2000_ast_pretty_print_idempotence_fuzz() {
    let mut rng = SplitMix64::new(0x0404_CAFE_BABE_1234);
    let cols = ["a", "b", "c", "x", "y"];
    let ops = ["+", "-", "*", "/", "=", "!=", "<", "<=", ">", ">="];

    for seed in 0..2_000u64 {
        let c1 = *rng.choose(&cols);
        let c2 = *rng.choose(&cols);
        let op1 = *rng.choose(&ops[0..4]);
        let op2 = *rng.choose(&ops[4..]);
        let n1 = rng.gen_range_i64(1, 500);
        let n2 = rng.gen_range_i64(1, 500);
        let distinct = if rng.gen_bool(0.3) { "DISTINCT " } else { "" };

        let sql = match seed % 6 {
            0 => format!(
                "SELECT {distinct}{c1} {op1} {n1} AS v, CASE WHEN {c2} {op2} {n2} THEN 'hi' ELSE 'lo' END FROM t1 WHERE {c1} BETWEEN {n1} AND {n2} AND {c2} IS NOT NULL ORDER BY 1 DESC NULLS FIRST LIMIT 10 OFFSET 2"
            ),
            1 => format!(
                "SELECT {c1}, SUM({c2}), COUNT(DISTINCT {c1}) FROM t1 INNER JOIN t2 ON t1.{c1} = t2.{c2} WHERE t1.{c1} IN ({n1}, {n2}) OR t2.{c2} LIKE '%abc%' GROUP BY {c1} HAVING SUM({c2}) > {n1}"
            ),
            2 => format!(
                "WITH cte AS (SELECT {c1}, {c2} FROM t1 WHERE {c1} {op2} {n1}) SELECT {c1}, ROW_NUMBER() OVER (PARTITION BY {c1} ORDER BY {c2} DESC ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM cte"
            ),
            3 => format!(
                "SELECT {c1} FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.{c2} = t1.{c1}) UNION ALL SELECT {c2} FROM t2 EXCEPT SELECT {c1} FROM t1 INTERSECT SELECT {c2} FROM t2"
            ),
            4 => format!(
                "SELECT CAST({c1} AS DECIMAL(15, 2)), TRY_CAST({c2} AS BIGINT), EXTRACT(YEAR FROM DATE '1998-12-01' - INTERVAL '{n1}' DAY) FROM t1 WHERE {c1} IS DISTINCT FROM NULL"
            ),
            _ => format!(
                "SELECT ({c1} + {n1}) * ({c2} - {n2}) FROM t1 LEFT JOIN t2 USING ({c1}) WHERE NOT ({c2} NOT IN (SELECT {c1} FROM t3))"
            ),
        };

        let ast1 = parse_single_statement(&sql)
            .unwrap_or_else(|e| panic!("seed {seed} parse failed: {e}\nSQL: {sql}"));
        let pretty = pretty_print_statement(&ast1);
        let ast2 = parse_single_statement(&pretty)
            .unwrap_or_else(|e| panic!("seed {seed} reparse failed: {e}\nPretty: {pretty}"));
        assert_eq!(
            ast1, ast2,
            "seed {seed} idempotence mismatch!\nOrig: {sql}\nPretty: {pretty}"
        );
    }
}
