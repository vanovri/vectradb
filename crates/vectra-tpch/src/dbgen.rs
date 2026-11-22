use crate::schema::{
    customer_schema, lineitem_schema, nation_schema, orders_schema, part_schema, partsupp_schema,
    region_schema, supplier_schema, TPCH_DEC,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use vectra_exec::{chunk_from_scalar_rows, Database};
use vectra_types::{
    days_from_civil, BoundedMemoryPool, DataChunk, MemoryPool, Result, ScalarValue, Schema,
    SplitMix64, VECTOR_CAPACITY,
};

const REGIONS: [(i64, &str, &str); 5] = [
    (0, "AFRICA", "lar deposits. blithely final packages cajole."),
    (1, "AMERICA", "hs use ironic, even requests. s"),
    (2, "ASIA", "ges. thinly even pinto beans ca"),
    (3, "EUROPE", "ly final courts cajole furiously final excuse"),
    (
        4,
        "MIDDLE EAST",
        "uickly special accounts cajole carefully.",
    ),
];

const NATIONS: [(i64, &str, i64); 25] = [
    (0, "ALGERIA", 0),
    (1, "ARGENTINA", 1),
    (2, "BRAZIL", 1),
    (3, "CANADA", 1),
    (4, "EGYPT", 4),
    (5, "ETHIOPIA", 0),
    (6, "FRANCE", 3),
    (7, "GERMANY", 3),
    (8, "INDIA", 2),
    (9, "INDONESIA", 2),
    (10, "IRAN", 4),
    (11, "IRAQ", 4),
    (12, "JAPAN", 2),
    (13, "JORDAN", 4),
    (14, "KENYA", 0),
    (15, "MOROCCO", 0),
    (16, "MOZAMBIQUE", 0),
    (17, "PERU", 1),
    (18, "CHINA", 2),
    (19, "ROMANIA", 3),
    (20, "SAUDI ARABIA", 4),
    (21, "VIETNAM", 2),
    (22, "RUSSIA", 3),
    (23, "UNITED KINGDOM", 3),
    (24, "UNITED STATES", 1),
];

const SEGMENTS: [&str; 5] = [
    "AUTOMOBILE",
    "BUILDING",
    "FURNITURE",
    "MACHINERY",
    "HOUSEHOLD",
];

const PRIORITIES: [&str; 5] = ["1-URGENT", "2-HIGH", "3-MEDIUM", "4-NOT SPECIFIED", "5-LOW"];
const SHIPMODES: [&str; 7] = ["AIR REG", "AIR", "RAIL", "SHIP", "TRUCK", "MAIL", "FOB"];
const SHIPINSTRUCTS: [&str; 4] = [
    "DELIVER IN PERSON",
    "COLLECT COD",
    "NONE",
    "TAKE BACK RETURN",
];
const PART_COLORS: [&str; 12] = [
    "almond", "antique", "forest", "green", "ivory", "lavender", "metallic", "olive", "peru",
    "purple", "rose", "sky",
];
const TYPE_SYL1: [&str; 6] = ["STANDARD", "SMALL", "MEDIUM", "LARGE", "ECONOMY", "PROMO"];
const TYPE_SYL2: [&str; 5] = ["ANODIZED", "BURNISHED", "PLATED", "POLISHED", "BRUSHED"];
const TYPE_SYL3: [&str; 5] = ["TIN", "NICKEL", "BRASS", "STEEL", "COPPER"];
const CONTAINERS: [&str; 12] = [
    "SM CASE", "SM BOX", "SM PACK", "SM PKG", "MED BAG", "MED BOX", "MED PKG", "MED PACK",
    "LG CASE", "LG BOX", "LG PACK", "LG PKG",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TpchLoadSummary {
    pub scale_factor: f64,
    pub region_rows: usize,
    pub nation_rows: usize,
    pub supplier_rows: usize,
    pub part_rows: usize,
    pub partsupp_rows: usize,
    pub customer_rows: usize,
    pub orders_rows: usize,
    pub lineitem_rows: usize,
}

impl TpchLoadSummary {
    pub fn total_rows(&self) -> usize {
        self.region_rows
            + self.nation_rows
            + self.supplier_rows
            + self.part_rows
            + self.partsupp_rows
            + self.customer_rows
            + self.orders_rows
            + self.lineitem_rows
    }
}

#[inline]
fn dec(cents: i128) -> ScalarValue {
    ScalarValue::Decimal128 {
        value: cents,
        precision: 15,
        scale: 2,
    }
}

/// Generates deterministic TPC-H rows at `scale_factor` and streams each table's `DataChunk`s
/// into `sink(table_name, schema, chunk)`.
pub fn generate_tpch_streaming<F>(
    scale_factor: f64,
    pool: &Arc<dyn MemoryPool>,
    mut sink: F,
) -> Result<TpchLoadSummary>
where
    F: FnMut(&str, &Schema, DataChunk) -> Result<()>,
{
    let sf = scale_factor.max(0.001);
    let mut rng = SplitMix64::new(0x7063_685f_5645_4354);

    let supp_n = ((10_000.0 * sf).round() as usize).max(25);
    let part_n = ((200_000.0 * sf).round() as usize).max(100);
    let cust_n = ((150_000.0 * sf).round() as usize).max(100);
    let ord_n = ((1_500_000.0 * sf).round() as usize).max(500);

    // 1. REGION (5 rows)
    let r_schema = region_schema();
    let r_rows: Vec<Vec<ScalarValue>> = REGIONS
        .iter()
        .map(|&(k, name, cmt)| {
            vec![
                ScalarValue::Int64(k),
                ScalarValue::Utf8(name.to_string()),
                ScalarValue::Utf8(cmt.to_string()),
            ]
        })
        .collect();
    sink(
        "region",
        &r_schema,
        chunk_from_scalar_rows(&r_schema.logical_types(), &r_rows, pool)?,
    )?;

    // 2. NATION (25 rows)
    let n_schema = nation_schema();
    let n_rows: Vec<Vec<ScalarValue>> = NATIONS
        .iter()
        .map(|&(k, name, rk)| {
            vec![
                ScalarValue::Int64(k),
                ScalarValue::Utf8(name.to_string()),
                ScalarValue::Int64(rk),
                ScalarValue::Utf8(format!("nation comment for {name}")),
            ]
        })
        .collect();
    sink(
        "nation",
        &n_schema,
        chunk_from_scalar_rows(&n_schema.logical_types(), &n_rows, pool)?,
    )?;

    // 3. SUPPLIER
    let s_schema = supplier_schema();
    let s_types = s_schema.logical_types();
    let mut buf = Vec::with_capacity(VECTOR_CAPACITY);
    for sk in 1..=(supp_n as i64) {
        let nat = (sk - 1) % 25;
        let cc = 10 + nat;
        let phone = format!(
            "{cc:02}-{:03}-{:03}-{:04}",
            rng.gen_range_i64(100, 999),
            rng.gen_range_i64(100, 999),
            rng.gen_range_i64(1000, 9999)
        );
        let acctbal = rng.gen_range_i64(-99_999, 999_999);
        let comment = if sk % 40 == 0 {
            "regular Customer feedback Complaints logged".to_string()
        } else if sk % 37 == 0 {
            "regular Customer feedback Recommends logged".to_string()
        } else {
            format!("supplier comment {sk}")
        };
        buf.push(vec![
            ScalarValue::Int64(sk),
            ScalarValue::Utf8(format!("Supplier#{sk:09}")),
            ScalarValue::Utf8(format!("Addr_{sk}_St")),
            ScalarValue::Int64(nat),
            ScalarValue::Utf8(phone),
            dec(i128::from(acctbal)),
            ScalarValue::Utf8(comment),
        ]);
        if buf.len() >= VECTOR_CAPACITY {
            sink(
                "supplier",
                &s_schema,
                chunk_from_scalar_rows(&s_types, &buf, pool)?,
            )?;
            buf.clear();
        }
    }
    if !buf.is_empty() {
        sink(
            "supplier",
            &s_schema,
            chunk_from_scalar_rows(&s_types, &buf, pool)?,
        )?;
        buf.clear();
    }

    // 4. PART
    let p_schema = part_schema();
    let p_types = p_schema.logical_types();
    let mut part_prices: Vec<i128> = Vec::with_capacity(part_n + 1);
    part_prices.push(0);
    for pk in 1..=(part_n as i64) {
        let c1 = PART_COLORS[rng.next_usize(PART_COLORS.len())];
        let c2 = PART_COLORS[rng.next_usize(PART_COLORS.len())];
        let c3 = PART_COLORS[rng.next_usize(PART_COLORS.len())];
        let name = format!("{c1} {c2} {c3}");
        let m = (pk % 5) + 1;
        let b = (rng.next_usize(5) as i64) + 1;
        let brand = format!("Brand#{m}{b}");
        let ptype = format!(
            "{} {} {}",
            TYPE_SYL1[rng.next_usize(TYPE_SYL1.len())],
            TYPE_SYL2[rng.next_usize(TYPE_SYL2.len())],
            TYPE_SYL3[rng.next_usize(TYPE_SYL3.len())]
        );
        let size = rng.gen_range_i64(1, 50);
        let container = CONTAINERS[rng.next_usize(CONTAINERS.len())];
        let retail_cents = i128::from(90_000 + ((pk % 1000) * 100) + (pk % 99));
        part_prices.push(retail_cents);

        buf.push(vec![
            ScalarValue::Int64(pk),
            ScalarValue::Utf8(name),
            ScalarValue::Utf8(format!("Manufacturer#{m}")),
            ScalarValue::Utf8(brand),
            ScalarValue::Utf8(ptype),
            ScalarValue::Int64(size),
            ScalarValue::Utf8(container.to_string()),
            dec(retail_cents),
            ScalarValue::Utf8(format!("part_cmt_{pk}")),
        ]);
        if buf.len() >= VECTOR_CAPACITY {
            sink(
                "part",
                &p_schema,
                chunk_from_scalar_rows(&p_types, &buf, pool)?,
            )?;
            buf.clear();
        }
    }
    if !buf.is_empty() {
        sink(
            "part",
            &p_schema,
            chunk_from_scalar_rows(&p_types, &buf, pool)?,
        )?;
        buf.clear();
    }

    // 5. PARTSUPP (4 suppliers per part)
    let ps_schema = partsupp_schema();
    let ps_types = ps_schema.logical_types();
    let mut partsupp_rows = 0usize;
    let sn = supp_n as i64;
    for pk in 1..=(part_n as i64) {
        for j in 0..4i64 {
            let sk = ((pk + j * ((sn / 4).max(1) + (pk / 256))) % sn) + 1;
            let avail = rng.gen_range_i64(1, 9999);
            let cost = rng.gen_range_i64(100, 100_000);
            buf.push(vec![
                ScalarValue::Int64(pk),
                ScalarValue::Int64(sk),
                ScalarValue::Int64(avail),
                dec(i128::from(cost)),
                ScalarValue::Utf8(format!("ps_{pk}_{sk}")),
            ]);
            partsupp_rows += 1;
            if buf.len() >= VECTOR_CAPACITY {
                sink(
                    "partsupp",
                    &ps_schema,
                    chunk_from_scalar_rows(&ps_types, &buf, pool)?,
                )?;
                buf.clear();
            }
        }
    }
    if !buf.is_empty() {
        sink(
            "partsupp",
            &ps_schema,
            chunk_from_scalar_rows(&ps_types, &buf, pool)?,
        )?;
        buf.clear();
    }

    // 6. CUSTOMER
    let c_schema = customer_schema();
    let c_types = c_schema.logical_types();
    for ck in 1..=(cust_n as i64) {
        let nat = rng.gen_range_i64(0, 24);
        let cc = 10 + nat;
        let phone = format!(
            "{cc:02}-{:03}-{:03}-{:04}",
            rng.gen_range_i64(100, 999),
            rng.gen_range_i64(100, 999),
            rng.gen_range_i64(1000, 9999)
        );
        let acctbal = rng.gen_range_i64(-99_999, 999_999);
        let seg = SEGMENTS[rng.next_usize(SEGMENTS.len())];
        buf.push(vec![
            ScalarValue::Int64(ck),
            ScalarValue::Utf8(format!("Customer#{ck:09}")),
            ScalarValue::Utf8(format!("CustAddr_{ck}")),
            ScalarValue::Int64(nat),
            ScalarValue::Utf8(phone),
            dec(i128::from(acctbal)),
            ScalarValue::Utf8(seg.to_string()),
            ScalarValue::Utf8(format!("cust_cmt_{ck}")),
        ]);
        if buf.len() >= VECTOR_CAPACITY {
            sink(
                "customer",
                &c_schema,
                chunk_from_scalar_rows(&c_types, &buf, pool)?,
            )?;
            buf.clear();
        }
    }
    if !buf.is_empty() {
        sink(
            "customer",
            &c_schema,
            chunk_from_scalar_rows(&c_types, &buf, pool)?,
        )?;
        buf.clear();
    }

    // 7 & 8. ORDERS + LINEITEM (generated jointly so o_totalprice & o_orderstatus are 100% consistent!)
    let o_schema = orders_schema();
    let o_types = o_schema.logical_types();
    let l_schema = lineitem_schema();
    let l_types = l_schema.logical_types();

    let start_date = days_from_civil(1992, 1, 1);
    let end_date = days_from_civil(1998, 8, 2);
    let current_date = days_from_civil(1995, 6, 17);
    let date_span = i64::from(end_date - start_date);

    let mut ord_buf = Vec::with_capacity(VECTOR_CAPACITY);
    let mut li_buf = Vec::with_capacity(VECTOR_CAPACITY);
    let mut lineitem_rows = 0usize;

    for oi in 1..=(ord_n as i64) {
        // TPC-H sparse orderkeys (1, 2, 3, 4, 5, 6, 7, 32, ...) or dense; dense is spec-valid and fast.
        let okey = oi;
        // Leave 1/3 of customers (ck % 3 == 0) with 0 orders (authentic TPC-H rule for Q13 & Q22!).
        let mut ckey = rng.gen_range_i64(1, cust_n as i64);
        if ckey % 3 == 0 {
            ckey = ((ckey % (cust_n as i64)) + 1).min(cust_n as i64);
        }
        let odate = start_date + (rng.gen_range_i64(0, date_span) as i32);
        let prio = PRIORITIES[rng.next_usize(PRIORITIES.len())];
        let clerk = format!(
            "Clerk#{:09}",
            rng.gen_range_i64(1, (1000.0 * sf).max(10.0) as i64)
        );
        let o_cmt = if oi % 11 == 0 {
            "unusual special requests pending".to_string()
        } else {
            format!("order comment {oi}")
        };

        let n_lines = rng.gen_range_i64(1, 7);
        let mut total_price_cents: i128 = 0;
        let mut count_o = 0;
        let mut count_f = 0;

        for lnum in 1..=n_lines {
            let pkey = rng.gen_range_i64(1, part_n as i64);
            let supp_slot = rng.gen_range_i64(0, 3);
            let skey = ((pkey + supp_slot * ((sn / 4).max(1) + (pkey / 256))) % sn) + 1;
            // Include occasional large quantities (up to 60) so Q18 `SUM(l_quantity) > 300` hits!
            let qty = if oi % 120 == 0 {
                55
            } else {
                rng.gen_range_i64(1, 50)
            };
            let disc_bp = rng.gen_range_i64(0, 10); // 0.00 .. 0.10
            let tax_bp = rng.gen_range_i64(0, 8); // 0.00 .. 0.08
            let unit_cents = part_prices[pkey as usize];
            let ext_cents = unit_cents * i128::from(qty);
            let disc_price = ext_cents * (100 - i128::from(disc_bp)) / 100;
            let charged = disc_price * (100 + i128::from(tax_bp)) / 100;
            total_price_cents += charged;

            let shipdate = odate + (rng.gen_range_i64(1, 121) as i32);
            let commitdate = odate + (rng.gen_range_i64(30, 90) as i32);
            let receiptdate = shipdate + (rng.gen_range_i64(1, 30) as i32);

            let rflag = if receiptdate <= current_date {
                if rng.next_usize(2) == 0 {
                    "R"
                } else {
                    "A"
                }
            } else {
                "N"
            };
            let lstatus = if shipdate > current_date {
                count_o += 1;
                "O"
            } else {
                count_f += 1;
                "F"
            };
            let shipinst = SHIPINSTRUCTS[rng.next_usize(SHIPINSTRUCTS.len())];
            let shipmode = SHIPMODES[rng.next_usize(SHIPMODES.len())];

            li_buf.push(vec![
                ScalarValue::Int64(okey),
                ScalarValue::Int64(pkey),
                ScalarValue::Int64(skey),
                ScalarValue::Int64(lnum),
                dec(i128::from(qty) * 100),
                dec(ext_cents),
                dec(i128::from(disc_bp)),
                dec(i128::from(tax_bp)),
                ScalarValue::Utf8(rflag.to_string()),
                ScalarValue::Utf8(lstatus.to_string()),
                ScalarValue::Date32(shipdate),
                ScalarValue::Date32(commitdate),
                ScalarValue::Date32(receiptdate),
                ScalarValue::Utf8(shipinst.to_string()),
                ScalarValue::Utf8(shipmode.to_string()),
                ScalarValue::Utf8(format!("li_{okey}_{lnum}")),
            ]);
            lineitem_rows += 1;
            if li_buf.len() >= VECTOR_CAPACITY {
                sink(
                    "lineitem",
                    &l_schema,
                    chunk_from_scalar_rows(&l_types, &li_buf, pool)?,
                )?;
                li_buf.clear();
            }
        }

        let ostatus = if count_o == 0 {
            "F"
        } else if count_f == 0 {
            "O"
        } else {
            "P"
        };

        ord_buf.push(vec![
            ScalarValue::Int64(okey),
            ScalarValue::Int64(ckey),
            ScalarValue::Utf8(ostatus.to_string()),
            dec(total_price_cents),
            ScalarValue::Date32(odate),
            ScalarValue::Utf8(prio.to_string()),
            ScalarValue::Utf8(clerk),
            ScalarValue::Int64(0),
            ScalarValue::Utf8(o_cmt),
        ]);
        if ord_buf.len() >= VECTOR_CAPACITY {
            sink(
                "orders",
                &o_schema,
                chunk_from_scalar_rows(&o_types, &ord_buf, pool)?,
            )?;
            ord_buf.clear();
        }
    }

    if !ord_buf.is_empty() {
        sink(
            "orders",
            &o_schema,
            chunk_from_scalar_rows(&o_types, &ord_buf, pool)?,
        )?;
    }
    if !li_buf.is_empty() {
        sink(
            "lineitem",
            &l_schema,
            chunk_from_scalar_rows(&l_types, &li_buf, pool)?,
        )?;
    }

    let _ = TPCH_DEC;
    Ok(TpchLoadSummary {
        scale_factor: sf,
        region_rows: REGIONS.len(),
        nation_rows: NATIONS.len(),
        supplier_rows: supp_n,
        part_rows: part_n,
        partsupp_rows,
        customer_rows: cust_n,
        orders_rows: ord_n,
        lineitem_rows,
    })
}

/// Creates (or replaces) all 8 TPC-H tables inside `db`, streams synthetic data at `scale_factor`,
/// commits the MVCC load transaction, and runs `ANALYZE` to populate optimizer HLL & Histograms!
pub fn load_tpch_into_database(db: &Arc<Database>, scale_factor: f64) -> Result<TpchLoadSummary> {
    let pool = BoundedMemoryPool::unbounded().as_dyn();
    for (name, schema) in crate::schema::all_tpch_schemas() {
        let _ = db.catalog().drop_table(name, true);
        db.catalog().create_table(name, schema, false)?;
    }

    let snap = db.catalog().begin_txn();
    let res = generate_tpch_streaming(scale_factor, &pool, |tname, _schema, chunk| {
        let tbl = db.catalog().get_table(tname)?;
        tbl.insert_chunk(&chunk, snap, &pool)?;
        Ok(())
    });
    match res {
        Ok(summary) => {
            db.catalog().commit_txn(snap.txn_id)?;
            db.analyze_table(None)?;
            Ok(summary)
        }
        Err(e) => {
            let _ = db.catalog().rollback_txn(snap.txn_id);
            Err(e)
        }
    }
}
