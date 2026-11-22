use std::collections::HashMap;
use std::time::Instant;
use vectra_optimizer::*;
use vectra_sql::*;
use vectra_storage::Catalog;
use vectra_types::*;

fn build_tpch_catalog_and_stats() -> (Catalog, StatsCatalog) {
    let cat = Catalog::new();
    let stats = StatsCatalog::new();
    let dec = LogicalType::Decimal128 {
        precision: 15,
        scale: 2,
    };

    type ColDef<'a> = (&'a str, LogicalType, f64);
    type TableDef<'a> = (&'a str, u64, Vec<ColDef<'a>>);
    let defs: Vec<TableDef<'_>> = vec![
        (
            "region",
            5,
            vec![
                ("r_regionkey", LogicalType::Int64, 5.0),
                ("r_name", LogicalType::Utf8, 5.0),
                ("r_comment", LogicalType::Utf8, 5.0),
            ],
        ),
        (
            "nation",
            25,
            vec![
                ("n_nationkey", LogicalType::Int64, 25.0),
                ("n_name", LogicalType::Utf8, 25.0),
                ("n_regionkey", LogicalType::Int64, 5.0),
                ("n_comment", LogicalType::Utf8, 25.0),
            ],
        ),
        (
            "supplier",
            100,
            vec![
                ("s_suppkey", LogicalType::Int64, 100.0),
                ("s_name", LogicalType::Utf8, 100.0),
                ("s_address", LogicalType::Utf8, 100.0),
                ("s_nationkey", LogicalType::Int64, 25.0),
                ("s_phone", LogicalType::Utf8, 100.0),
                ("s_acctbal", dec, 100.0),
                ("s_comment", LogicalType::Utf8, 100.0),
            ],
        ),
        (
            "customer",
            1_500,
            vec![
                ("c_custkey", LogicalType::Int64, 1_500.0),
                ("c_name", LogicalType::Utf8, 1_500.0),
                ("c_address", LogicalType::Utf8, 1_500.0),
                ("c_nationkey", LogicalType::Int64, 25.0),
                ("c_phone", LogicalType::Utf8, 1_500.0),
                ("c_acctbal", dec, 1_500.0),
                ("c_mktsegment", LogicalType::Utf8, 5.0),
                ("c_comment", LogicalType::Utf8, 1_500.0),
            ],
        ),
        (
            "part",
            2_000,
            vec![
                ("p_partkey", LogicalType::Int64, 2_000.0),
                ("p_name", LogicalType::Utf8, 2_000.0),
                ("p_mfgr", LogicalType::Utf8, 5.0),
                ("p_brand", LogicalType::Utf8, 25.0),
                ("p_type", LogicalType::Utf8, 150.0),
                ("p_size", LogicalType::Int64, 50.0),
                ("p_container", LogicalType::Utf8, 40.0),
                ("p_retailprice", dec, 1_500.0),
                ("p_comment", LogicalType::Utf8, 2_000.0),
            ],
        ),
        (
            "partsupp",
            8_000,
            vec![
                ("ps_partkey", LogicalType::Int64, 2_000.0),
                ("ps_suppkey", LogicalType::Int64, 100.0),
                ("ps_availqty", LogicalType::Int64, 5_000.0),
                ("ps_supplycost", dec, 7_500.0),
                ("ps_comment", LogicalType::Utf8, 8_000.0),
            ],
        ),
        (
            "orders",
            15_000,
            vec![
                ("o_orderkey", LogicalType::Int64, 15_000.0),
                ("o_custkey", LogicalType::Int64, 1_000.0),
                ("o_orderstatus", LogicalType::Utf8, 3.0),
                ("o_totalprice", dec, 15_000.0),
                ("o_orderdate", LogicalType::Date32, 2_400.0),
                ("o_orderpriority", LogicalType::Utf8, 5.0),
                ("o_clerk", LogicalType::Utf8, 1_000.0),
                ("o_shippriority", LogicalType::Int64, 1.0),
                ("o_comment", LogicalType::Utf8, 15_000.0),
            ],
        ),
        (
            "lineitem",
            60_000,
            vec![
                ("l_orderkey", LogicalType::Int64, 15_000.0),
                ("l_partkey", LogicalType::Int64, 2_000.0),
                ("l_suppkey", LogicalType::Int64, 100.0),
                ("l_linenumber", LogicalType::Int64, 7.0),
                ("l_quantity", dec, 50.0),
                ("l_extendedprice", dec, 45_000.0),
                ("l_discount", dec, 11.0),
                ("l_tax", dec, 9.0),
                ("l_returnflag", LogicalType::Utf8, 3.0),
                ("l_linestatus", LogicalType::Utf8, 2.0),
                ("l_shipdate", LogicalType::Date32, 2_500.0),
                ("l_commitdate", LogicalType::Date32, 2_500.0),
                ("l_receiptdate", LogicalType::Date32, 2_500.0),
                ("l_shipinstruct", LogicalType::Utf8, 4.0),
                ("l_shipmode", LogicalType::Utf8, 7.0),
                ("l_comment", LogicalType::Utf8, 60_000.0),
            ],
        ),
    ];

    for (tname, rows, cols) in defs {
        let fields: Vec<Field> = cols
            .iter()
            .map(|(n, ty, _)| Field::new(*n, *ty, false))
            .collect();
        cat.create_table(tname, Schema::new(fields), false).unwrap();

        let col_stats: Vec<ColumnStats> = cols
            .iter()
            .map(|(n, ty, ndv)| ColumnStats {
                col_name: (*n).to_string(),
                ndv: *ndv,
                null_fraction: 0.0,
                min_value: None,
                max_value: None,
                avg_byte_width: ty.fixed_byte_width() as f64,
                histogram: EquiHeightHistogram::default(),
            })
            .collect();
        stats.insert(TableStats {
            table_name: tname.to_string(),
            row_count: rows,
            columns: col_stats,
        });
    }

    (cat, stats)
}

const TPCH_22: [&str; 22] = [
    "SELECT l_returnflag, l_linestatus, SUM(l_quantity) AS sum_qty, SUM(l_extendedprice) AS sum_base_price,
            SUM(l_extendedprice * (1 - l_discount)) AS sum_disc_price,
            SUM(l_extendedprice * (1 - l_discount) * (1 + l_tax)) AS sum_charge,
            AVG(l_quantity) AS avg_qty, AVG(l_extendedprice) AS avg_price, AVG(l_discount) AS avg_disc,
            COUNT(*) AS count_order
     FROM lineitem WHERE l_shipdate <= DATE '1998-12-01' - INTERVAL '90' DAY
     GROUP BY l_returnflag, l_linestatus ORDER BY l_returnflag, l_linestatus",
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
    "SELECT l_orderkey, SUM(l_extendedprice * (1 - l_discount)) AS revenue, o_orderdate, o_shippriority
     FROM customer, orders, lineitem
     WHERE c_mktsegment = 'BUILDING' AND c_custkey = o_custkey AND l_orderkey = o_orderkey
       AND o_orderdate < DATE '1995-03-15' AND l_shipdate > DATE '1995-03-15'
     GROUP BY l_orderkey, o_orderdate, o_shippriority ORDER BY revenue DESC, o_orderdate LIMIT 10",
    "SELECT o_orderpriority, COUNT(*) AS order_count FROM orders
     WHERE o_orderdate >= DATE '1993-07-01' AND o_orderdate < DATE '1993-07-01' + INTERVAL '3' MONTH
       AND EXISTS (SELECT * FROM lineitem WHERE l_orderkey = o_orderkey AND l_commitdate < l_receiptdate)
     GROUP BY o_orderpriority ORDER BY o_orderpriority",
    "SELECT n_name, SUM(l_extendedprice * (1 - l_discount)) AS revenue
     FROM customer, orders, lineitem, supplier, nation, region
     WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey AND l_suppkey = s_suppkey
       AND c_nationkey = s_nationkey AND s_nationkey = n_nationkey AND n_regionkey = r_regionkey
       AND r_name = 'ASIA' AND o_orderdate >= DATE '1994-01-01' AND o_orderdate < DATE '1994-01-01' + INTERVAL '1' YEAR
     GROUP BY n_name ORDER BY revenue DESC",
    "SELECT SUM(l_extendedprice * l_discount) AS revenue FROM lineitem
     WHERE l_shipdate >= DATE '1994-01-01' AND l_shipdate < DATE '1994-01-01' + INTERVAL '1' YEAR
       AND l_discount BETWEEN 0.05 AND 0.07 AND l_quantity < 24",
    "SELECT supp_nation, cust_nation, l_year, SUM(volume) AS revenue FROM (
         SELECT n1.n_name AS supp_nation, n2.n_name AS cust_nation, EXTRACT(YEAR FROM l_shipdate) AS l_year,
                l_extendedprice * (1 - l_discount) AS volume
         FROM supplier, lineitem, orders, customer, nation n1, nation n2
         WHERE s_suppkey = l_suppkey AND o_orderkey = l_orderkey AND c_custkey = o_custkey
           AND s_nationkey = n1.n_nationkey AND c_nationkey = n2.n_nationkey
           AND ((n1.n_name = 'FRANCE' AND n2.n_name = 'GERMANY') OR (n1.n_name = 'GERMANY' AND n2.n_name = 'FRANCE'))
           AND l_shipdate BETWEEN DATE '1995-01-01' AND DATE '1996-12-31'
     ) AS shipping GROUP BY supp_nation, cust_nation, l_year ORDER BY supp_nation, cust_nation, l_year",
    "SELECT o_year, SUM(CASE WHEN nation = 'BRAZIL' THEN volume ELSE 0 END) / SUM(volume) AS mkt_share FROM (
         SELECT EXTRACT(YEAR FROM o_orderdate) AS o_year, l_extendedprice * (1 - l_discount) AS volume, n2.n_name AS nation
         FROM part, supplier, lineitem, orders, customer, nation n1, nation n2, region
         WHERE p_partkey = l_partkey AND s_suppkey = l_suppkey AND l_orderkey = o_orderkey
           AND o_custkey = c_custkey AND c_nationkey = n1.n_nationkey AND n1.n_regionkey = r_regionkey
           AND r_name = 'AMERICA' AND s_nationkey = n2.n_nationkey
           AND o_orderdate BETWEEN DATE '1995-01-01' AND DATE '1996-12-31' AND p_type = 'ECONOMY ANODIZED STEEL'
     ) AS all_nations GROUP BY o_year ORDER BY o_year",
    "SELECT nation, o_year, SUM(amount) AS sum_profit FROM (
         SELECT n_name AS nation, EXTRACT(YEAR FROM o_orderdate) AS o_year,
                l_extendedprice * (1 - l_discount) - ps_supplycost * l_quantity AS amount
         FROM part, supplier, lineitem, partsupp, orders, nation
         WHERE s_suppkey = l_suppkey AND ps_suppkey = l_suppkey AND ps_partkey = l_partkey
           AND p_partkey = l_partkey AND o_orderkey = l_orderkey AND s_nationkey = n_nationkey
           AND p_name LIKE '%green%'
     ) AS profit GROUP BY nation, o_year ORDER BY nation, o_year DESC",
    "SELECT c_custkey, c_name, SUM(l_extendedprice * (1 - l_discount)) AS revenue, c_acctbal, n_name, c_address, c_phone, c_comment
     FROM customer, orders, lineitem, nation
     WHERE c_custkey = o_custkey AND l_orderkey = o_orderkey
       AND o_orderdate >= DATE '1993-10-01' AND o_orderdate < DATE '1993-10-01' + INTERVAL '3' MONTH
       AND l_returnflag = 'R' AND c_nationkey = n_nationkey
     GROUP BY c_custkey, c_name, c_acctbal, c_phone, n_name, c_address, c_comment ORDER BY revenue DESC LIMIT 20",
    "SELECT ps_partkey, SUM(ps_supplycost * ps_availqty) AS value
     FROM partsupp, supplier, nation
     WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = 'GERMANY'
     GROUP BY ps_partkey
     HAVING SUM(ps_supplycost * ps_availqty) > (
         SELECT SUM(ps_supplycost * ps_availqty) * 0.0001 FROM partsupp, supplier, nation
         WHERE ps_suppkey = s_suppkey AND s_nationkey = n_nationkey AND n_name = 'GERMANY'
     ) ORDER BY value DESC",
    "SELECT l_shipmode,
            SUM(CASE WHEN o_orderpriority = '1-URGENT' OR o_orderpriority = '2-HIGH' THEN 1 ELSE 0 END) AS high_line_count,
            SUM(CASE WHEN o_orderpriority <> '1-URGENT' AND o_orderpriority <> '2-HIGH' THEN 1 ELSE 0 END) AS low_line_count
     FROM orders, lineitem
     WHERE o_orderkey = l_orderkey AND l_shipmode IN ('MAIL', 'SHIP')
       AND l_commitdate < l_receiptdate AND l_shipdate < l_commitdate
       AND l_receiptdate >= DATE '1994-01-01' AND l_receiptdate < DATE '1994-01-01' + INTERVAL '1' YEAR
     GROUP BY l_shipmode ORDER BY l_shipmode",
    "SELECT c_count, COUNT(*) AS custdist FROM (
         SELECT c_custkey, COUNT(o_orderkey) AS c_count
         FROM customer LEFT OUTER JOIN orders ON c_custkey = o_custkey AND o_comment NOT LIKE '%special%requests%'
         GROUP BY c_custkey
     ) AS c_orders (c_custkey, c_count) GROUP BY c_count ORDER BY custdist DESC, c_count DESC",
    "SELECT 100.00 * SUM(CASE WHEN p_type LIKE 'PROMO%' THEN l_extendedprice * (1 - l_discount) ELSE 0 END) /
            SUM(l_extendedprice * (1 - l_discount)) AS promo_revenue
     FROM lineitem, part WHERE l_partkey = p_partkey
       AND l_shipdate >= DATE '1995-09-01' AND l_shipdate < DATE '1995-09-01' + INTERVAL '1' MONTH",
    "WITH revenue0 (supplier_no, total_revenue) AS (
         SELECT l_suppkey, SUM(l_extendedprice * (1 - l_discount)) FROM lineitem
         WHERE l_shipdate >= DATE '1996-01-01' AND l_shipdate < DATE '1996-01-01' + INTERVAL '3' MONTH
         GROUP BY l_suppkey
     )
     SELECT s_suppkey, s_name, s_address, s_phone, total_revenue FROM supplier, revenue0
     WHERE s_suppkey = supplier_no AND total_revenue = (SELECT MAX(total_revenue) FROM revenue0)
     ORDER BY s_suppkey",
    "SELECT p_brand, p_type, p_size, COUNT(DISTINCT ps_suppkey) AS supplier_cnt
     FROM partsupp, part
     WHERE p_partkey = ps_partkey AND p_brand <> 'Brand#45' AND p_type NOT LIKE 'MEDIUM POLISHED%'
       AND p_size IN (49, 14, 23, 45, 19, 3, 36, 9)
       AND ps_suppkey NOT IN (SELECT s_suppkey FROM supplier WHERE s_comment LIKE '%Customer%Complaints%')
     GROUP BY p_brand, p_type, p_size ORDER BY supplier_cnt DESC, p_brand, p_type, p_size",
    "SELECT SUM(l_extendedprice) / 7.0 AS avg_yearly FROM lineitem, part
     WHERE p_partkey = l_partkey AND p_brand = 'Brand#23' AND p_container = 'MED BOX'
       AND l_quantity < (SELECT 0.2 * AVG(l_quantity) FROM lineitem WHERE l_partkey = p_partkey)",
    "SELECT c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice, SUM(l_quantity)
     FROM customer, orders, lineitem
     WHERE o_orderkey IN (SELECT l_orderkey FROM lineitem GROUP BY l_orderkey HAVING SUM(l_quantity) > 300)
       AND c_custkey = o_custkey AND o_orderkey = l_orderkey
     GROUP BY c_name, c_custkey, o_orderkey, o_orderdate, o_totalprice ORDER BY o_totalprice DESC, o_orderdate LIMIT 100",
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
    "SELECT s_name, COUNT(*) AS numwait FROM supplier, lineitem l1, orders, nation
     WHERE s_suppkey = l1.l_suppkey AND o_orderkey = l1.l_orderkey AND o_orderstatus = 'F'
       AND l1.l_receiptdate > l1.l_commitdate
       AND EXISTS (SELECT * FROM lineitem l2 WHERE l2.l_orderkey = l1.l_orderkey AND l2.l_suppkey <> l1.l_suppkey)
       AND NOT EXISTS (SELECT * FROM lineitem l3 WHERE l3.l_orderkey = l1.l_orderkey AND l3.l_suppkey <> l1.l_suppkey AND l3.l_receiptdate > l3.l_commitdate)
       AND s_nationkey = n_nationkey AND n_name = 'SAUDI ARABIA'
     GROUP BY s_name ORDER BY numwait DESC, s_name LIMIT 100",
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

fn walk_plan<F: FnMut(&PhysicalPlan)>(plan: &PhysicalPlan, f: &mut F) {
    f(plan);
    for child in plan.children() {
        walk_plan(child, f);
    }
}

#[test]
fn test_hll_and_equi_height_histogram_accuracy() {
    let mut hll_all = HyperLogLog::new();
    let mut hll_a = HyperLogLog::new();
    let mut hll_b = HyperLogLog::new();
    for i in 0..100_000i64 {
        let v = ScalarValue::Int64(i % 50_000);
        hll_all.insert_scalar(&v);
        if i < 50_000 {
            hll_a.insert_scalar(&v);
        } else {
            hll_b.insert_scalar(&v);
        }
    }
    hll_a.merge(&hll_b);
    let est = hll_all.estimate();
    assert!(
        (est - 50_000.0).abs() / 50_000.0 < 0.03,
        "HLL estimate {est} outside 3% of 50,000"
    );
    assert!((hll_a.estimate() - est).abs() < 1e-6);

    let vals: Vec<ScalarValue> = (0..10_000).map(ScalarValue::Int64).collect();
    let hist = EquiHeightHistogram::build(vals, 64);
    assert_eq!(hist.buckets.len(), 64);
    let s_lt = hist
        .selectivity_lt(&ScalarValue::Int64(2_500), false)
        .unwrap();
    assert!((s_lt - 0.25).abs() < 0.03, "histogram lt sel={s_lt}");
    let s_bet = hist
        .selectivity_between(&ScalarValue::Int64(2_000), &ScalarValue::Int64(6_000))
        .unwrap();
    assert!((s_bet - 0.40).abs() < 0.03, "histogram between sel={s_bet}");
}

#[test]
fn test_tpch_22_plan_quality_pruning_and_build_side_invariants() {
    let (cat, stats) = build_tpch_catalog_and_stats();
    let views = HashMap::new();
    let opt = Optimizer::new(&stats);

    for (idx, sql) in TPCH_22.iter().enumerate() {
        let qnum = idx + 1;
        let ast = parse_single_statement(sql).unwrap();
        let mut binder = Binder::new(&cat, &views);
        let LogicalStatement::Query(bound) = binder.bind_statement(&ast).unwrap() else {
            panic!();
        };
        let mut cg = ColIdGen::new();
        cg.set_min_next(binder.next_col_id());
        let bundle = opt
            .optimize_full(*bound, &mut cg)
            .unwrap_or_else(|e| panic!("TPC-H Q{qnum} optimize failed: {e}"));

        let phys = &bundle.physical;

        // 1. Assert ZERO accidental Cartesian Cross joins!
        //    (Allow Cross only if it came from a 1-row scalar/empty relation or uncorrelated Single!)
        let mut hash_joins = 0usize;
        let mut has_topn = false;
        let mut lineitem_cols = None;
        let mut lineitem_has_pruning = false;

        walk_plan(phys, &mut |node| {
            match &node.op {
                PhysicalOp::NestedLoopJoin {
                    left,
                    right,
                    join_type,
                    ..
                } => {
                    if *join_type == PhysicalJoinType::Cross {
                        // Only allowed if one side is a 1-row scalar aggregate!
                        assert!(
                            left.estimated_rows <= 1.5 || right.estimated_rows <= 1.5,
                            "TPC-H Q{qnum} produced accidental CrossJoin ({:.0} x {:.0})!\n{}",
                            left.estimated_rows,
                            right.estimated_rows,
                            phys.format_tree()
                        );
                    }
                }
                PhysicalOp::HashJoin {
                    left,
                    right,
                    join_type,
                    ..
                } => {
                    hash_joins += 1;
                    // 2. Assert build side <= probe side whenever join orientation is symmetric!
                    if matches!(
                        join_type,
                        PhysicalJoinType::Inner
                            | PhysicalJoinType::LeftOuter
                            | PhysicalJoinType::RightOuter
                    ) {
                        assert!(
                            right.estimated_rows <= left.estimated_rows + 1e-6,
                            "TPC-H Q{qnum} HashJoin build ({:.0}) > probe ({:.0})!\n{}",
                            right.estimated_rows,
                            left.estimated_rows,
                            phys.format_tree()
                        );
                    }
                }
                PhysicalOp::Scan {
                    table_name,
                    projection,
                    pruning_predicate,
                    ..
                } => {
                    if table_name == "lineitem" {
                        lineitem_cols = Some(projection.len());
                        if pruning_predicate.is_some() {
                            lineitem_has_pruning = true;
                        }
                    }
                }
                PhysicalOp::TopN { .. } => {
                    has_topn = true;
                }
                _ => {}
            }
        });

        // 3. Verify Projection Pruning & ZoneMap Pushdown on Q1 & Q6!
        if qnum == 1 {
            assert_eq!(
                lineitem_cols,
                Some(7),
                "TPC-H Q1 must read 7 of 16 lineitem columns!\n{}",
                phys.format_tree()
            );
            assert!(
                lineitem_has_pruning,
                "TPC-H Q1 must push folded l_shipdate filter into ZoneMap PruningPredicate!"
            );
        }
        if qnum == 6 {
            assert_eq!(
                lineitem_cols,
                Some(4),
                "TPC-H Q6 must read 4 of 16 lineitem columns!\n{}",
                phys.format_tree()
            );
            assert!(
                lineitem_has_pruning,
                "TPC-H Q6 must push folded date/discount/quantity filters into ZoneMap PruningPredicate!"
            );
        }
        // 4. Verify TPC-H Q19 factored shared `p_partkey = l_partkey` into an Equi HashJoin!
        if qnum == 19 {
            assert!(
                hash_joins >= 1,
                "TPC-H Q19 must extract shared join key into PhysicalHashJoin!\n{}",
                phys.format_tree()
            );
        }
        // 5. Verify TopN fusion on ORDER BY + LIMIT queries!
        if matches!(qnum, 2 | 3 | 10 | 18 | 21) {
            assert!(
                has_topn,
                "TPC-H Q{qnum} should fuse Sort+Limit into PhysicalOp::TopN!\n{}",
                phys.format_tree()
            );
        }
    }
}

#[test]
fn test_join_graph_stress_2_to_24_tables_under_20ms() {
    let cat = Catalog::new();
    let stats = StatsCatalog::new();

    for i in 0..24usize {
        let name = format!("t{i}");
        cat.create_table(
            &name,
            Schema::new(vec![
                Field::new("id", LogicalType::Int64, false),
                Field::new("fk", LogicalType::Int64, false),
                Field::new("val", LogicalType::Int64, false),
            ]),
            false,
        )
        .unwrap();
        let rows = ((i + 1) as u64) * 500;
        stats.insert(TableStats {
            table_name: name,
            row_count: rows,
            columns: vec![
                ColumnStats {
                    col_name: "id".into(),
                    ndv: rows as f64,
                    null_fraction: 0.0,
                    min_value: None,
                    max_value: None,
                    avg_byte_width: 8.0,
                    histogram: EquiHeightHistogram::default(),
                },
                ColumnStats {
                    col_name: "fk".into(),
                    ndv: (rows / 2).max(1) as f64,
                    null_fraction: 0.0,
                    min_value: None,
                    max_value: None,
                    avg_byte_width: 8.0,
                    histogram: EquiHeightHistogram::default(),
                },
                ColumnStats {
                    col_name: "val".into(),
                    ndv: 100.0,
                    null_fraction: 0.0,
                    min_value: None,
                    max_value: None,
                    avg_byte_width: 8.0,
                    histogram: EquiHeightHistogram::default(),
                },
            ],
        });
    }

    let views = HashMap::new();
    let opt = Optimizer::new(&stats);

    // Pre-parse & bind queries for chain (N=2, 8, 12, 18, 24) and star (N=10, 24) topologies
    let mut bound_plans = Vec::new();
    for &n in &[2usize, 6, 10, 14, 18, 24] {
        // Chain topology: t0.id = t1.fk AND t1.id = t2.fk ...
        let from_tables: Vec<String> = (0..n).map(|i| format!("t{i}")).collect();
        let chain_preds: Vec<String> = (0..n - 1)
            .map(|i| format!("t{i}.id = t{}.fk", i + 1))
            .collect();
        let sql_chain = format!(
            "SELECT t0.val FROM {} WHERE {}",
            from_tables.join(", "),
            chain_preds.join(" AND ")
        );
        let ast = parse_single_statement(&sql_chain).unwrap();
        let mut binder = Binder::new(&cat, &views);
        let LogicalStatement::Query(p) = binder.bind_statement(&ast).unwrap() else {
            panic!();
        };
        bound_plans.push((format!("chain_{n}"), *p, binder.next_col_id()));

        // Star topology: t0.fk = t1.id AND t0.fk = t2.id ...
        let star_preds: Vec<String> = (1..n).map(|i| format!("t0.fk = t{i}.id")).collect();
        let sql_star = format!(
            "SELECT t0.val FROM {} WHERE {}",
            from_tables.join(", "),
            star_preds.join(" AND ")
        );
        let ast2 = parse_single_statement(&sql_star).unwrap();
        let mut binder2 = Binder::new(&cat, &views);
        let LogicalStatement::Query(p2) = binder2.bind_statement(&ast2).unwrap() else {
            panic!();
        };
        bound_plans.push((format!("star_{n}"), *p2, binder2.next_col_id()));
    }

    // Also add 8-table clique and 16-table snowflake topologies!
    {
        let from_clique: Vec<String> = (0..8).map(|i| format!("t{i}")).collect();
        let mut clique_preds = Vec::new();
        for i in 0..8 {
            for j in (i + 1)..8 {
                clique_preds.push(format!("t{i}.id = t{j}.fk"));
            }
        }
        let sql_clique = format!(
            "SELECT t0.val FROM {} WHERE {}",
            from_clique.join(", "),
            clique_preds.join(" AND ")
        );
        let ast = parse_single_statement(&sql_clique).unwrap();
        let mut binder = Binder::new(&cat, &views);
        let LogicalStatement::Query(p) = binder.bind_statement(&ast).unwrap() else {
            panic!();
        };
        bound_plans.push(("clique_8".into(), *p, binder.next_col_id()));

        // 16-table snowflake: center t0 -> hubs t1..t5 -> leaves t6..t15
        let from_snow: Vec<String> = (0..16).map(|i| format!("t{i}")).collect();
        let mut snow_preds = Vec::new();
        for h in 1..=5 {
            snow_preds.push(format!("t0.fk = t{h}.id"));
        }
        for l in 6..16 {
            let hub = 1 + (l - 6) % 5;
            snow_preds.push(format!("t{hub}.fk = t{l}.id"));
        }
        let sql_snow = format!(
            "SELECT t0.val FROM {} WHERE {}",
            from_snow.join(", "),
            snow_preds.join(" AND ")
        );
        let ast2 = parse_single_statement(&sql_snow).unwrap();
        let mut binder2 = Binder::new(&cat, &views);
        let LogicalStatement::Query(p2) = binder2.bind_statement(&ast2).unwrap() else {
            panic!();
        };
        bound_plans.push(("snowflake_16".into(), *p2, binder2.next_col_id()));
    }

    let start = Instant::now();
    for (label, plan, next_id) in bound_plans {
        let mut cg = ColIdGen::new();
        cg.set_min_next(next_id);
        let bundle = opt.optimize_full(plan, &mut cg).unwrap();
        walk_plan(&bundle.physical, &mut |node| {
            if let PhysicalOp::NestedLoopJoin { join_type, .. } = &node.op {
                assert_ne!(
                    *join_type,
                    PhysicalJoinType::Cross,
                    "{label} generated accidental CrossJoin!"
                );
            }
        });
    }
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "Optimized 14 join graphs (2..24 tables: chain, star, clique, snowflake) in {elapsed_ms:.2} ms"
    );
    assert!(
        elapsed_ms < 20.0,
        "join-graph stress exceeded 20ms budget: {elapsed_ms:.2} ms"
    );
}
