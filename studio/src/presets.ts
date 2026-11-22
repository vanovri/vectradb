export interface QueryPreset {
  id: string;
  title: string;
  badge: string;
  memoryMb?: number;
  sql: string;
}

export const QUERY_PRESETS: QueryPreset[] = [
  {
    id: "q01",
    title: "TPC-H Q01 · Pricing Summary",
    badge: "HashAgg + Decimal128",
    sql: `SELECT
  l_returnflag,
  l_linestatus,
  SUM(l_quantity) AS sum_qty,
  SUM(l_extendedprice * (1 - l_discount)) AS sum_disc_price,
  AVG(l_discount) AS avg_disc,
  COUNT(*) AS count_order
FROM lineitem
WHERE l_shipdate <= DATE '1998-12-01' - INTERVAL '90' DAY
GROUP BY l_returnflag, l_linestatus
ORDER BY l_returnflag, l_linestatus;`
  },
  {
    id: "q02",
    title: "TPC-H Q02 · Correlated Unnesting",
    badge: "Neumann-Kemper",
    sql: `SELECT s_acctbal, s_name, n_name, p_partkey, p_mfgr
FROM part, supplier, partsupp, nation, region
WHERE p_partkey = ps_partkey
  AND s_suppkey = ps_suppkey
  AND p_size = 15
  AND p_type LIKE '%BRASS'
  AND s_nationkey = n_nationkey
  AND n_regionkey = r_regionkey
  AND r_name = 'EUROPE'
  AND ps_supplycost = (
    SELECT MIN(ps_supplycost)
    FROM partsupp, supplier, nation, region
    WHERE p_partkey = ps_partkey
      AND s_suppkey = ps_suppkey
      AND s_nationkey = n_nationkey
      AND n_regionkey = r_regionkey
      AND r_name = 'EUROPE'
  )
ORDER BY s_acctbal DESC, n_name, s_name, p_partkey
LIMIT 25;`
  },
  {
    id: "q08",
    title: "TPC-H Q08 · 8-Table DPccp Reorder",
    badge: "DPccp Join Order",
    sql: `SELECT o_year,
  SUM(CASE WHEN nation = 'BRAZIL' THEN volume ELSE 0 END) / SUM(volume) AS mkt_share
FROM (
  SELECT EXTRACT(YEAR FROM o_orderdate) AS o_year,
    l_extendedprice * (1 - l_discount) AS volume,
    n2.n_name AS nation
  FROM part, supplier, lineitem, orders, customer, nation n1, nation n2, region
  WHERE p_partkey = l_partkey AND s_suppkey = l_suppkey AND l_orderkey = o_orderkey
    AND o_custkey = c_custkey AND c_nationkey = n1.n_nationkey AND n1.n_regionkey = r_regionkey
    AND r_name = 'AMERICA' AND s_nationkey = n2.n_nationkey
    AND o_orderdate BETWEEN DATE '1995-01-01' AND DATE '1996-12-31'
    AND p_type = 'ECONOMY ANODIZED STEEL'
) AS all_nations
GROUP BY o_year
ORDER BY o_year;`
  },
  {
    id: "q19",
    title: "TPC-H Q19 · Disjunctive Factoring",
    badge: "OR -> Equi HashJoin",
    sql: `SELECT SUM(l_extendedprice * (1 - l_discount)) AS revenue
FROM lineitem, part
WHERE (p_partkey = l_partkey AND p_brand = 'Brand#12'
       AND p_container IN ('SM CASE', 'SM BOX', 'SM PACK', 'SM PKG')
       AND l_quantity >= 1 AND l_quantity <= 11 AND p_size BETWEEN 1 AND 5
       AND l_shipmode IN ('AIR', 'AIR REG') AND l_shipinstruct = 'DELIVER IN PERSON')
   OR (p_partkey = l_partkey AND p_brand = 'Brand#23'
       AND p_container IN ('MED BAG', 'MED BOX', 'MED PKG', 'MED PACK')
       AND l_quantity >= 10 AND l_quantity <= 20 AND p_size BETWEEN 1 AND 10
       AND l_shipmode IN ('AIR', 'AIR REG') AND l_shipinstruct = 'DELIVER IN PERSON')
   OR (p_partkey = l_partkey AND p_brand = 'Brand#34'
       AND p_container IN ('LG CASE', 'LG BOX', 'LG PACK', 'LG PKG')
       AND l_quantity >= 20 AND l_quantity <= 30 AND p_size BETWEEN 1 AND 15
       AND l_shipmode IN ('AIR', 'AIR REG') AND l_shipinstruct = 'DELIVER IN PERSON');`
  },
  {
    id: "spill",
    title: "Low-RAM Grace Hash Spill (2 MiB)",
    badge: "Out-of-Core Spill",
    memoryMb: 2,
    sql: `SELECT l_orderkey, l_partkey, SUM(l_extendedprice) AS total_ext, COUNT(*) AS cnt
FROM lineitem
GROUP BY l_orderkey, l_partkey
ORDER BY total_ext DESC
LIMIT 40;`
  },
  {
    id: "window",
    title: "Sliding Window · Segment Tree O(log N)",
    badge: "Window Frame",
    sql: `SELECT
  o_orderpriority,
  o_orderkey,
  o_totalprice,
  ROW_NUMBER() OVER (PARTITION BY o_orderpriority ORDER BY o_totalprice DESC) AS rn,
  SUM(o_totalprice) OVER (
    PARTITION BY o_orderpriority
    ORDER BY o_orderkey
    ROWS BETWEEN 5 PRECEDING AND 2 FOLLOWING
  ) AS rolling_sum
FROM orders
ORDER BY o_orderpriority, rn
LIMIT 40;`
  }
];
