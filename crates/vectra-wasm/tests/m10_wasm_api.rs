use serde_json::Value;
use vectra_wasm::VectraWasmDb;

#[test]
fn test_m10_wasm_db_api_tpch_csv_byteview_xray_and_4_stages() {
    let mut db = VectraWasmDb::new(64);
    assert_eq!(db.memory_limit_mb(), 64);

    // 1. Load TPC-H SF=0.005
    let load: Value = serde_json::from_str(&db.load_tpch(0.005)).unwrap();
    assert_eq!(load["ok"], true);
    assert!(load["tables"].as_array().unwrap().len() >= 8);

    // 2. Execute TPC-H Q02 (correlated subquery -> Neumann-Kemper unnesting -> 4-stage DAG!)
    let q02 = vectra_tpch::tpch_query(2).unwrap().sql;
    let res: Value = serde_json::from_str(&db.execute_json(q02)).unwrap();
    assert_eq!(res["ok"], true, "Q02 failed: {res}");
    assert!(res["stages"]["bound_logical"].is_object());
    assert!(res["stages"]["unnested_logical"].is_object());
    assert!(res["stages"]["optimized_logical"].is_object());
    assert!(res["stages"]["physical"].is_object());
    // Bound logical has DependentJoin, Unnested has 0 DependentJoin!
    let bound_str = res["stages"]["bound_logical"].to_string();
    let unnested_str = res["stages"]["unnested_logical"].to_string();
    assert!(bound_str.contains("DependentJoin"));
    assert!(!unnested_str.contains("DependentJoin"));
    assert!(res["explain_analyze"]
        .as_str()
        .unwrap()
        .contains("EXPLAIN ANALYZE"));

    // 3. Import CSV with both inline (<=12B) and out-of-line (>12B) German Strings
    let csv = "id,tag,description\n1,short,inline_12b!!\n2,long,this_string_exceeds_twelve_bytes_german_heap\n";
    let imp: Value = serde_json::from_str(&db.import_csv("demo_strings", csv)).unwrap();
    assert_eq!(imp["ok"], true);

    // 4. Inspect Storage Layout + 16-Byte ByteView X-Ray!
    let xray: Value = serde_json::from_str(&db.inspect_storage_layout("demo_strings")).unwrap();
    assert_eq!(xray["ok"], true);
    let bvs = xray["byte_views"].as_array().unwrap();
    assert!(!bvs.is_empty());
    let has_inline = bvs
        .iter()
        .any(|b| b["is_inline"] == true && b["bytes_hex"].as_array().unwrap().len() == 16);
    let has_outline = bvs
        .iter()
        .any(|b| b["is_inline"] == false && b["bytes_hex"].as_array().unwrap().len() == 16);
    assert!(has_inline, "must observe <=12B inline ByteView");
    assert!(has_outline, "must observe >12B out-of-line ByteView");

    // 5. Verify SQLSTATE error reporting on invalid SQL
    let err: Value = serde_json::from_str(&db.execute_json("SELECT 1 / 0")).unwrap();
    assert_eq!(err["ok"], false);
    assert_eq!(err["error"]["sqlstate"], "22012");
}
