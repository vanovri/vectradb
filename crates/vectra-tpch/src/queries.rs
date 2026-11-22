/// Metadata & official ANSI SQL text for a TPC-H benchmark query.
#[derive(Debug, Clone, Copy)]
pub struct TpchQuery {
    pub id: usize,
    pub name: &'static str,
    pub sql: &'static str,
    pub ordered: bool,
}

pub const TPCH_QUERIES: [TpchQuery; 22] = [
    TpchQuery {
        id: 1,
        name: "Q01 Pricing Summary Report",
        sql: include_str!("../queries/q01.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 2,
        name: "Q02 Minimum Cost Supplier",
        sql: include_str!("../queries/q02.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 3,
        name: "Q03 Shipping Priority",
        sql: include_str!("../queries/q03.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 4,
        name: "Q04 Order Priority Checking",
        sql: include_str!("../queries/q04.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 5,
        name: "Q05 Local Supplier Volume",
        sql: include_str!("../queries/q05.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 6,
        name: "Q06 Forecasting Revenue Change",
        sql: include_str!("../queries/q06.sql"),
        ordered: false,
    },
    TpchQuery {
        id: 7,
        name: "Q07 Volume Shipping",
        sql: include_str!("../queries/q07.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 8,
        name: "Q08 National Market Share",
        sql: include_str!("../queries/q08.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 9,
        name: "Q09 Product Type Profit Measure",
        sql: include_str!("../queries/q09.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 10,
        name: "Q10 Returned Item Reporting",
        sql: include_str!("../queries/q10.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 11,
        name: "Q11 Important Stock Identification",
        sql: include_str!("../queries/q11.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 12,
        name: "Q12 Shipping Modes & Order Priority",
        sql: include_str!("../queries/q12.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 13,
        name: "Q13 Customer Distribution",
        sql: include_str!("../queries/q13.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 14,
        name: "Q14 Promotion Effect",
        sql: include_str!("../queries/q14.sql"),
        ordered: false,
    },
    TpchQuery {
        id: 15,
        name: "Q15 Top Supplier",
        sql: include_str!("../queries/q15.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 16,
        name: "Q16 Parts/Supplier Relationship",
        sql: include_str!("../queries/q16.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 17,
        name: "Q17 Small-Quantity-Order Revenue",
        sql: include_str!("../queries/q17.sql"),
        ordered: false,
    },
    TpchQuery {
        id: 18,
        name: "Q18 Large Volume Customer",
        sql: include_str!("../queries/q18.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 19,
        name: "Q19 Discounted Revenue",
        sql: include_str!("../queries/q19.sql"),
        ordered: false,
    },
    TpchQuery {
        id: 20,
        name: "Q20 Potential Part Promotion",
        sql: include_str!("../queries/q20.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 21,
        name: "Q21 Suppliers Who Kept Orders Waiting",
        sql: include_str!("../queries/q21.sql"),
        ordered: true,
    },
    TpchQuery {
        id: 22,
        name: "Q22 Global Sales Opportunity",
        sql: include_str!("../queries/q22.sql"),
        ordered: true,
    },
];

pub fn tpch_query(id: usize) -> Option<&'static TpchQuery> {
    if (1..=22).contains(&id) {
        Some(&TPCH_QUERIES[id - 1])
    } else {
        None
    }
}

/// Mechanical dialect translation of ANSI TPC-H date/extract/substring syntax into SQLite syntax
/// solely for ground-truth differential validation against `SqliteOracle`.
pub fn translate_tpch_to_sqlite(ansi_sql: &str) -> String {
    ansi_sql
        .replace("DATE '1998-12-01' - INTERVAL '90' DAY", "'1998-09-02'")
        .replace("DATE '1993-07-01' + INTERVAL '3' MONTH", "'1993-10-01'")
        .replace("DATE '1994-01-01' + INTERVAL '1' YEAR", "'1995-01-01'")
        .replace("DATE '1993-10-01' + INTERVAL '3' MONTH", "'1994-01-01'")
        .replace("DATE '1995-09-01' + INTERVAL '1' MONTH", "'1995-10-01'")
        .replace("DATE '1996-01-01' + INTERVAL '3' MONTH", "'1996-04-01'")
        .replace("DATE '", "'")
        .replace(
            "EXTRACT(YEAR FROM l_shipdate)",
            "CAST(substr(l_shipdate, 1, 4) AS INTEGER)",
        )
        .replace(
            "EXTRACT(YEAR FROM o_orderdate)",
            "CAST(substr(o_orderdate, 1, 4) AS INTEGER)",
        )
        .replace("SUBSTRING(c_phone FROM 1 FOR 2)", "substr(c_phone, 1, 2)")
        .replace("AS c_orders (c_custkey, c_count)", "AS c_orders")
        .replace(
            "SUM(CASE WHEN nation = 'BRAZIL' THEN volume ELSE 0 END) / SUM(volume)",
            "CAST(SUM(CASE WHEN nation = 'BRAZIL' THEN volume ELSE 0 END) AS REAL) / CAST(SUM(volume) AS REAL)",
        )
}
