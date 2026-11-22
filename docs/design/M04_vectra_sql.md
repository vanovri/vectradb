# M04 Design Document: `vectra-sql`

## 1. Objectives & Architectural Guarantees
`vectra-sql` implements VectraDB's complete hand-crafted SQL compiler frontend (`#![deny(unsafe_code)]`, zero parser-generator crates):
1. Zero-copy byte-offset `Span` Lexer (`lexer.rs`) + multi-line caret diagnostic renderer (`diagnostics.rs`).
2. Pratt Expression & Recursive-Descent Analytical SQL Parser (`parser.rs`, `ast.rs`) covering full TPC-H Q1–Q22, CTEs (`WITH RECURSIVE`), `GROUPING SETS`/`ROLLUP`/`CUBE`, Windows (`ROWS`/`RANGE` + named `WINDOW`), Set Operations (`INTERSECT` > `UNION`/`EXCEPT`), Subqueries (`EXISTS`/`IN`/`ANY`/`ALL`/scalar), DDL/DML/COPY/ANALYZE/TXN/EXPLAIN, and PostgreSQL `$1` parameters.
3. Deterministic `AstPrettyPrinter` (`pretty.rs`) guaranteeing `parse(pretty_print(parse(sql))) == parse(sql)`.
4. Semantic Binder (`binder.rs`, `coercion.rs`) resolving hierarchical scopes, aliases, ordinals, outer correlated bindings, and SQL Least-Upper-Bound (LUB) type coercion lattice.
5. Neumann & Kemper (BTW 2015) Arbitrary Subquery Unnesting (`unnest.rs`) guaranteeing zero remaining `DependentJoin` nodes and zero free correlated variables.

## 2. Unique Column Identifier (`ColId`) Architecture
- Every relation column, computed projection, aggregate result, window output, and mark-join flag is assigned a globally unique `ColId(u32)` during binding.
- Upstream expressions reference `LogicalExpr::Col { id, name, data_type }` rather than fragile positional offsets, ensuring subquery unnesting, join reordering, and projection pruning preserve column identity without re-indexing bugs.

## 3. Neumann-Kemper Arbitrary Subquery Unnesting (`unnest.rs`)
- Bound correlated subqueries (`EXISTS`, `NOT EXISTS`, `IN`, `NOT IN`, `ANY`/`ALL`, scalar `(SELECT ...)`) initially lower into `LogicalPlan::DependentJoin { left, right, join_type, correlated_cols }`.
- Unnesting recursively pushes the deduplicated outer domain $D = \pi_{\text{distinct}}(\text{correlated\_cols})$ down through `Filter`, `Project`, `Aggregate`, `Join`, `Distinct`, and `SetOp`, replacing `CorrelatedCol(c_i)` with regular domain columns `Col(d_i)`.
- **COUNT-bug safety:** When pushing $D$ through a global aggregate (`group_by.is_empty()`), `unnest` joins $D$ via `LeftOuterJoin` and wraps `COUNT` outputs in `COALESCE(count_col, 0)` so empty inner groups preserve exact SQL `COUNT(*) = 0` semantics.
- **3-valued `NOT IN` / `OR EXISTS` safety:** `EXISTS` and `IN` subqueries inside arbitrary boolean trees lower via `JoinType::LeftMark { mark_col, probe_pair }`, preserving NULL semantics without losing rows.

## 4. Edge Cases Audited
- Ambiguous column references, aggregates disallowed in `WHERE`, ungrouped columns in `SELECT`, cyclic non-recursive CTEs, mismatched `UNION` arity.
- Multi-level 3-deep nested correlated subqueries (e.g., TPC-H Q2, Q20, Q21).
- `INTERSECT` binding tighter than `UNION` and `EXCEPT`.
- `DATE 'YYYY-MM-DD' +/- INTERVAL 'N' DAY|MONTH|YEAR`, `EXTRACT(part FROM expr)`, `SUBSTRING(s FROM a FOR b)`, `CASE expr WHEN ...`.
