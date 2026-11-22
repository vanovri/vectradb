use vectra_types::{Field, LogicalType, Schema};

pub const TPCH_DEC: LogicalType = LogicalType::Decimal128 {
    precision: 15,
    scale: 2,
};

pub fn region_schema() -> Schema {
    Schema::new(vec![
        Field::new("r_regionkey", LogicalType::Int64, false),
        Field::new("r_name", LogicalType::Utf8, false),
        Field::new("r_comment", LogicalType::Utf8, true),
    ])
}

pub fn nation_schema() -> Schema {
    Schema::new(vec![
        Field::new("n_nationkey", LogicalType::Int64, false),
        Field::new("n_name", LogicalType::Utf8, false),
        Field::new("n_regionkey", LogicalType::Int64, false),
        Field::new("n_comment", LogicalType::Utf8, true),
    ])
}

pub fn supplier_schema() -> Schema {
    Schema::new(vec![
        Field::new("s_suppkey", LogicalType::Int64, false),
        Field::new("s_name", LogicalType::Utf8, false),
        Field::new("s_address", LogicalType::Utf8, false),
        Field::new("s_nationkey", LogicalType::Int64, false),
        Field::new("s_phone", LogicalType::Utf8, false),
        Field::new("s_acctbal", TPCH_DEC, false),
        Field::new("s_comment", LogicalType::Utf8, true),
    ])
}

pub fn part_schema() -> Schema {
    Schema::new(vec![
        Field::new("p_partkey", LogicalType::Int64, false),
        Field::new("p_name", LogicalType::Utf8, false),
        Field::new("p_mfgr", LogicalType::Utf8, false),
        Field::new("p_brand", LogicalType::Utf8, false),
        Field::new("p_type", LogicalType::Utf8, false),
        Field::new("p_size", LogicalType::Int64, false),
        Field::new("p_container", LogicalType::Utf8, false),
        Field::new("p_retailprice", TPCH_DEC, false),
        Field::new("p_comment", LogicalType::Utf8, true),
    ])
}

pub fn partsupp_schema() -> Schema {
    Schema::new(vec![
        Field::new("ps_partkey", LogicalType::Int64, false),
        Field::new("ps_suppkey", LogicalType::Int64, false),
        Field::new("ps_availqty", LogicalType::Int64, false),
        Field::new("ps_supplycost", TPCH_DEC, false),
        Field::new("ps_comment", LogicalType::Utf8, true),
    ])
}

pub fn customer_schema() -> Schema {
    Schema::new(vec![
        Field::new("c_custkey", LogicalType::Int64, false),
        Field::new("c_name", LogicalType::Utf8, false),
        Field::new("c_address", LogicalType::Utf8, false),
        Field::new("c_nationkey", LogicalType::Int64, false),
        Field::new("c_phone", LogicalType::Utf8, false),
        Field::new("c_acctbal", TPCH_DEC, false),
        Field::new("c_mktsegment", LogicalType::Utf8, false),
        Field::new("c_comment", LogicalType::Utf8, true),
    ])
}

pub fn orders_schema() -> Schema {
    Schema::new(vec![
        Field::new("o_orderkey", LogicalType::Int64, false),
        Field::new("o_custkey", LogicalType::Int64, false),
        Field::new("o_orderstatus", LogicalType::Utf8, false),
        Field::new("o_totalprice", TPCH_DEC, false),
        Field::new("o_orderdate", LogicalType::Date32, false),
        Field::new("o_orderpriority", LogicalType::Utf8, false),
        Field::new("o_clerk", LogicalType::Utf8, false),
        Field::new("o_shippriority", LogicalType::Int64, false),
        Field::new("o_comment", LogicalType::Utf8, true),
    ])
}

pub fn lineitem_schema() -> Schema {
    Schema::new(vec![
        Field::new("l_orderkey", LogicalType::Int64, false),
        Field::new("l_partkey", LogicalType::Int64, false),
        Field::new("l_suppkey", LogicalType::Int64, false),
        Field::new("l_linenumber", LogicalType::Int64, false),
        Field::new("l_quantity", TPCH_DEC, false),
        Field::new("l_extendedprice", TPCH_DEC, false),
        Field::new("l_discount", TPCH_DEC, false),
        Field::new("l_tax", TPCH_DEC, false),
        Field::new("l_returnflag", LogicalType::Utf8, false),
        Field::new("l_linestatus", LogicalType::Utf8, false),
        Field::new("l_shipdate", LogicalType::Date32, false),
        Field::new("l_commitdate", LogicalType::Date32, false),
        Field::new("l_receiptdate", LogicalType::Date32, false),
        Field::new("l_shipinstruct", LogicalType::Utf8, false),
        Field::new("l_shipmode", LogicalType::Utf8, false),
        Field::new("l_comment", LogicalType::Utf8, true),
    ])
}

pub fn all_tpch_schemas() -> [(&'static str, Schema); 8] {
    [
        ("region", region_schema()),
        ("nation", nation_schema()),
        ("supplier", supplier_schema()),
        ("part", part_schema()),
        ("partsupp", partsupp_schema()),
        ("customer", customer_schema()),
        ("orders", orders_schema()),
        ("lineitem", lineitem_schema()),
    ]
}
