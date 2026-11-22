# M05 Design Document: `vectra-optimizer`

## 1. Objectives & Architectural Guarantees
`vectra-optimizer` implements VectraDB's multi-pass algebraic rewrite engine, statistical cardinality estimator (`HyperLogLog++` + `EquiHeightHistogram`), `DPccp`/`DPsub` + `GOO` bushy join-order cost-based optimizer, and physical operator selector (`#![deny(unsafe_code)]`).

## 2. Statistics & Cardinality Estimation (`stats.rs`, `cardinality.rs`)
- **HyperLogLog++ (`HyperLogLog`)**: 14-bit precision (`m = 16,384` registers), standard harmonic mean bias constant $\alpha_m = 0.7213 / (1 + 1.079 / 16384)$, and linear counting correction when $E \le 2.5 m$ and zero registers $> 0$. Supports associative `merge(&other)` for parallel `ANALYZE` and `APPROX_COUNT_DISTINCT`.
- **Equi-Height Histogram (`EquiHeightHistogram`)**: 64 sorted buckets recording `(min, max, count, ndv)` for range selectivity (`<, <=, >, >=, BETWEEN`).
- **Selectivity Model**:
  - `col = const`: `(1 - null_frac) / max(1, ndv)` clamped to `0` outside `[min, max]`.
  - Conjunctions (`AND`): sorted ascending by selectivity with exponential backoff `sel_0 * sel_1^(1/2) * sel_2^(1/4) * ...` to avoid correlated under-estimation collapse.
  - Equi-join: `(|R| * |S|) / max(1, max(ndv_a, ndv_b))`.

## 3. Multi-Pass Logical Rewrite Pipeline (`rules.rs`)
1. **Constant Folding & Disjunctive Common-Factor Extraction**: folds constant date/interval/arithmetic expressions (`DATE '1998-12-01' - INTERVAL '90' DAY`), eliminates contradictions (`EmptyRelation`), and factors common join conjuncts out of `OR` trees (turning TPC-H Q19 into an Equi Hash Join!).
2. **MarkJoin -> Semi/Anti Simplification**: simplifies `Filter(mark, LeftMarkJoin)` into `LeftSemiJoin` / `LeftAntiJoin` where NULL-safe.
3. **Outer-to-Inner Join Simplification**: converts `LEFT`/`RIGHT`/`FULL OUTER JOIN` into `INNER JOIN` when an upstream `WHERE` filter rejects NULLs on the nullable side.
4. **Predicate Pushdown**: pushes predicates through `Project`, `Aggregate`, `Join`, and leaf `Scan::pushed_filters` (compiling range/equality/IN/IS NULL filters into M03 `PruningPredicate` for ZoneMap + Bloom skipping!).
5. **Projection Pruning**: top-down required `ColId` propagation pruning unused columns from every `LogicalPlan::Scan` and intermediate `Project` (TPC-H Q1 reads 7/16 `lineitem` columns; Q6 reads 4/16).
6. **Limit-to-TopN Pushdown**: fuses `Limit(Sort(...))` into `TopN` and pushes `Limit` into `UNION ALL`.

## 4. Join Order Optimization & Physical Selection (`join_order.rs`, `physical_plan.rs`)
- Extracts reorderable connected join hypergraphs from `Inner`/`Cross` join trees.
- Uses exact connected-subgraph dynamic programming (`DPccp`/`DPsub`) enumerating bushy trees for $N \le 14$ relations without accidental cross-products, falling back to $O(N^2)$ **Greedy Operator Ordering (GOO)** for $N > 14$ (optimizing 24-table graphs in <20 ms).
- Always orients smaller estimated cardinality onto the **Right (Build)** side of `PhysicalHashJoin` (flipping symmetric `INNER` and `LEFT <-> RIGHT` joins automatically!).
- Extracts equi-join key pairs (`=` and `IS NOT DISTINCT FROM`) for `PhysicalHashJoin`, reserving `PhysicalNestedLoopJoin` strictly for inequality-only joins.
