use alloy::primitives::{Address, I256, U256};
use alloy::providers::{Provider, ProviderBuilder};
use alloy::sol;
use futures::StreamExt;
use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;
use mongodb::Client as MongoClient;
use plotters::prelude::*;
use serde::Deserialize;
use std::str::FromStr;
use std::time::Instant;

use sim_common::progress;
use v3_pool::math::{full_math, sqrt_price_math, tick_math, FIXED_POINT_128_Q128};
use v3_pool::pool::UniswapV3Pool;
use v3_pool::tick;

// ── Sentinel address for our simulated LP position ─────────────────────────
const SIM_OWNER: Address = Address::new([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0xDE, 0xAD, 0xBE, 0xEF,
]);

// ── Config ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SimConfig {
    rpc: String,
    pool_address: String,
    mongo_uri: String,
    db_name: String,
    fee: u32,
    tick_spacing: i32,
    genesis_block: u64,
    from_block: u64,
    to_block: u64,
    tick_lower: Option<i32>,
    tick_upper: Option<i32>,
    price_range_pct: Option<f64>,
    deposit_weth: String,
    token0_decimals: u8,
    token1_decimals: u8,
    token0_symbol: String,
    token1_symbol: String,
}

impl SimConfig {
    fn load() -> Self {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sim_config.toml");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{} not found", path.display()));
        toml::from_str(&raw).expect("invalid sim_config.toml")
    }
}

// ── On-chain contract for verification ──────────────────────────────────────

sol! {
    #[sol(rpc)]
    contract IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96, int24 tick, uint16 observationIndex,
            uint16 observationCardinality, uint16 observationCardinalityNext,
            uint8 feeProtocol, bool unlocked
        );
        function liquidity() external view returns (uint128);
    }
}

// ── Arg parsing helpers ─────────────────────────────────────────────────────

fn arg_str<'a>(args: &'a Document, key: &str) -> &'a str {
    args.get_str(key).unwrap_or_else(|_| panic!("missing arg '{}'", key))
}
fn arg_u256(args: &Document, key: &str) -> U256 {
    U256::from_str(arg_str(args, key)).unwrap_or_else(|_| panic!("bad U256 '{}'", key))
}
fn arg_i256(args: &Document, key: &str) -> I256 {
    I256::from_dec_str(arg_str(args, key)).unwrap_or_else(|_| panic!("bad I256 '{}'", key))
}
fn arg_i32(args: &Document, key: &str) -> i32 {
    arg_str(args, key).parse().unwrap_or_else(|_| panic!("bad i32 '{}'", key))
}
fn arg_u128(args: &Document, key: &str) -> u128 {
    arg_str(args, key).parse().unwrap_or_else(|_| panic!("bad u128 '{}'", key))
}
fn arg_u8(args: &Document, key: &str) -> u8 {
    arg_str(args, key).parse().unwrap_or_else(|_| panic!("bad u8 '{}'", key))
}
fn arg_u16(args: &Document, key: &str) -> u16 {
    arg_str(args, key).parse().unwrap_or_else(|_| panic!("bad u16 '{}'", key))
}
fn arg_address(args: &Document, key: &str) -> Address {
    Address::from_str(arg_str(args, key)).unwrap_or_else(|_| panic!("bad address '{}'", key))
}

// ── Price helpers ───────────────────────────────────────────────────────────

fn sqrt_price_to_human(sqrt_price_x96: U256, dec0: u8, dec1: u8) -> f64 {
    let s: f64 = sqrt_price_x96.to_string().parse().unwrap();
    let q96 = 2f64.powi(96);
    let raw_price = (s / q96).powi(2);
    raw_price * 10f64.powi(dec0 as i32 - dec1 as i32)
}

fn tick_to_price(tick: i32, dec0: u8, dec1: u8) -> f64 {
    sqrt_price_to_human(tick_math::get_sqrt_ratio_at_tick(tick), dec0, dec1)
}

fn price_to_tick(price: f64, dec0: u8, dec1: u8, tick_spacing: i32) -> i32 {
    let raw_price = price / 10f64.powi(dec0 as i32 - dec1 as i32);
    let tick = (raw_price.ln() / 1.0001f64.ln()).round() as i32;
    (tick / tick_spacing) * tick_spacing
}

/// Compute accrued fees for our position without modifying pool state.
fn compute_accrued_fees(
    pool: &UniswapV3Pool,
    tick_lower: i32,
    tick_upper: i32,
    sim_liquidity: u128,
) -> (f64, f64, f64, f64) {
    // (fees_0_raw, fees_1_raw, principal_0_raw, principal_1_raw)
    let (fg_inside_0, fg_inside_1) = tick::get_fee_growth_inside(
        &pool.ticks,
        tick_lower,
        tick_upper,
        pool.slot0.tick,
        pool.fee_growth_global_0_x128,
        pool.fee_growth_global_1_x128,
    );

    let pos = pool.positions.get(&(SIM_OWNER, tick_lower, tick_upper));
    let (last_fg0, last_fg1) = match pos {
        Some(p) => (p.fee_growth_inside_0_last_x128, p.fee_growth_inside_1_last_x128),
        None => (U256::ZERO, U256::ZERO),
    };

    let delta_fg0 = fg_inside_0.wrapping_sub(last_fg0);
    let delta_fg1 = fg_inside_1.wrapping_sub(last_fg1);

    let fees_0 = full_math::mul_div(delta_fg0, U256::from(sim_liquidity), FIXED_POINT_128_Q128);
    let fees_1 = full_math::mul_div(delta_fg1, U256::from(sim_liquidity), FIXED_POINT_128_Q128);

    let fees_0_f = fees_0.to_string().parse::<f64>().unwrap();
    let fees_1_f = fees_1.to_string().parse::<f64>().unwrap();

    // Principal: what we'd get back if we burned now
    let current_tick = pool.slot0.tick;
    let sqrt_price = pool.slot0.sqrt_price_x96;
    let sqrt_lower = tick_math::get_sqrt_ratio_at_tick(tick_lower);
    let sqrt_upper = tick_math::get_sqrt_ratio_at_tick(tick_upper);

    let p0 = if current_tick < tick_lower {
        sqrt_price_math::get_amount0_delta(sqrt_lower, sqrt_upper, sim_liquidity, false)
    } else if current_tick < tick_upper {
        sqrt_price_math::get_amount0_delta(sqrt_price, sqrt_upper, sim_liquidity, false)
    } else {
        U256::ZERO
    };

    let p1 = if current_tick < tick_lower {
        U256::ZERO
    } else if current_tick < tick_upper {
        sqrt_price_math::get_amount1_delta(sqrt_lower, sqrt_price, sim_liquidity, false)
    } else {
        sqrt_price_math::get_amount1_delta(sqrt_lower, sqrt_upper, sim_liquidity, false)
    };

    let p0_f = p0.to_string().parse::<f64>().unwrap();
    let p1_f = p1.to_string().parse::<f64>().unwrap();

    (fees_0_f, fees_1_f, p0_f, p1_f)
}

// ── Snapshot for time series ────────────────────────────────────────────────

#[derive(Clone)]
struct Snapshot {
    block: u64,
    _weth_price: f64,
    cumulative_fees_usd: f64,
    #[allow(dead_code)] cumulative_volume_usd: f64,
    net_position_value_usd: f64,
    #[allow(dead_code)] overall_pnl_usd: f64,
    fee_return_pct: f64,
    #[allow(dead_code)] overall_return_pct: f64,
}

// ── Process a single event ──────────────────────────────────────────────────

fn process_event(
    pool: &mut UniswapV3Pool,
    event_doc: &Document,
    block_timestamp: &mut u32,
    last_block: &mut i64,
    sim_position: Option<(i32, i32, u128)>,
) {
    let event_name = event_doc.get_str("eventName").expect("missing eventName");
    let args = event_doc.get_document("args").expect("missing args");
    let block_number = event_doc.get_i64("blockNumber").unwrap_or(0);

    if block_number != *last_block {
        *block_timestamp += 1;
        *last_block = block_number;
    }

    match event_name {
        "Initialize" => {
            pool.initialize(arg_u256(args, "sqrtPriceX96"));
        }
        "Mint" => {
            let owner = arg_address(args, "owner");
            let tl = arg_i32(args, "tickLower");
            let tu = arg_i32(args, "tickUpper");
            let amount = arg_u128(args, "amount");
            pool.mint(owner, tl, tu, amount, *block_timestamp);
        }
        "Burn" => {
            let owner = arg_address(args, "owner");
            let tl = arg_i32(args, "tickLower");
            let tu = arg_i32(args, "tickUpper");
            let amount = arg_u128(args, "amount");
            pool.burn(owner, tl, tu, amount, *block_timestamp);
        }
        "Swap" => {
            let amount0 = arg_i256(args, "amount0");
            let amount1 = arg_i256(args, "amount1");
            let event_sqrt_price = arg_u256(args, "sqrtPriceX96");
            let event_tick = arg_i32(args, "tick");
            let event_liquidity = arg_u128(args, "liquidity");

            let zero_for_one = amount0 > I256::ZERO;
            let amount_specified = if zero_for_one { amount0 } else { amount1 };
            let sqrt_price_limit = if zero_for_one {
                tick_math::MIN_SQRT_RATIO + U256::from(1)
            } else {
                tick_math::MAX_SQRT_RATIO - U256::from(1)
            };

            pool.swap(zero_for_one, amount_specified, sqrt_price_limit, *block_timestamp);

            pool.slot0.sqrt_price_x96 = event_sqrt_price;
            pool.slot0.tick = event_tick;

            match sim_position {
                Some((tl, tu, our_liq)) => {
                    let our_in_range = event_tick >= tl && event_tick < tu;
                    pool.liquidity = event_liquidity + if our_in_range { our_liq } else { 0 };
                }
                None => {
                    pool.liquidity = event_liquidity;
                }
            }
        }
        "Collect" => {
            let owner = arg_address(args, "owner");
            let tl = arg_i32(args, "tickLower");
            let tu = arg_i32(args, "tickUpper");
            let a0 = arg_u128(args, "amount0");
            let a1 = arg_u128(args, "amount1");
            pool.collect(owner, tl, tu, a0, a1);
        }
        "Flash" => {
            let paid0 = arg_u256(args, "paid0");
            let paid1 = arg_u256(args, "paid1");
            pool.flash_with_paid(paid0, paid1);
        }
        "SetFeeProtocol" => {
            pool.set_fee_protocol(
                arg_u8(args, "feeProtocol0New"),
                arg_u8(args, "feeProtocol1New"),
            );
        }
        "IncreaseObservationCardinalityNext" => {
            pool.increase_observation_cardinality_next(
                arg_u16(args, "observationCardinalityNextNew"),
            );
        }
        "CollectProtocol" => {
            let a0 = arg_u128(args, "amount0");
            let a1 = arg_u128(args, "amount1");
            if a0 > 0 { pool.protocol_fees.token0 = pool.protocol_fees.token0.saturating_sub(a0); }
            if a1 > 0 { pool.protocol_fees.token1 = pool.protocol_fees.token1.saturating_sub(a1); }
        }
        _ => {}
    }
}

// ── Charting ────────────────────────────────────────────────────────────────

fn draw_charts(
    snapshots: &[Snapshot],
    from_block: u64,
    _to_block: u64,
    from_block_timestamp: u64,
    _user_capital_usd: f64,
) {
    if snapshots.len() < 2 {
        println!("  Not enough data points for charts.");
        return;
    }

    let chart_base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let arb_block_time: f64 = 0.25;

    let block_to_ts = |block: u64| -> f64 {
        from_block_timestamp as f64 + (block as f64 - from_block as f64) * arb_block_time
    };

    let ts_to_date = |ts: f64| -> String {
        let secs = ts as i64;
        chrono::DateTime::from_timestamp(secs, 0)
            .unwrap().naive_utc().format("%b %d").to_string()
    };

    let ts_to_datetime = |ts: f64| -> String {
        let secs = ts as i64;
        chrono::DateTime::from_timestamp(secs, 0)
            .unwrap().naive_utc().format("%b %d %H:%M").to_string()
    };

    let x_min = block_to_ts(from_block);
    let x_max = block_to_ts(snapshots.last().unwrap().block);
    let duration_secs = x_max - x_min;
    let use_datetime = duration_secs < 3.0 * 86400.0;
    let x_fmt = move |v: &f64| -> String {
        if use_datetime { ts_to_datetime(*v) } else { ts_to_date(*v) }
    };

    // ── Chart 1: Cumulative Fees ────────────────────────────────────────────
    {
        let path_buf = chart_base.join("chart_fees.png");
        let path = path_buf.to_str().unwrap();
        let y_max = snapshots.last().unwrap().cumulative_fees_usd * 1.1;

        let root = BitMapBackend::new(path, (2400, 1000)).into_drawing_area();
        root.fill(&RGBColor(18, 18, 24)).unwrap();

        let mut chart = ChartBuilder::on(&root)
            .caption("Cumulative Fees Generated (USD)", ("sans-serif", 40).into_font().color(&WHITE))
            .margin(40).x_label_area_size(65).y_label_area_size(90)
            .build_cartesian_2d(x_min..x_max, 0.0..y_max.max(0.01)).unwrap();

        chart.configure_mesh()
            .bold_line_style(RGBColor(40, 40, 50)).light_line_style(RGBColor(30, 30, 38))
            .axis_style(RGBColor(120, 120, 140))
            .label_style(("sans-serif", 18).into_font().color(&RGBColor(180, 180, 200)))
            .x_label_formatter(&x_fmt).y_label_formatter(&|v| format!("${:.2}", v))
            .x_desc("Date").y_desc("Fees (USD)").draw().unwrap();

        chart.draw_series(AreaSeries::new(
            snapshots.iter().map(|s| (block_to_ts(s.block), s.cumulative_fees_usd)),
            0.0, RGBColor(0, 220, 160).mix(0.2),
        ).border_style(ShapeStyle::from(RGBColor(0, 220, 160)).stroke_width(2))).unwrap();

        root.present().unwrap();
        println!("  Saved: {}", path);
    }

    // ── Chart 2: Net Position Value ─────────────────────────────────────────
    {
        let path_buf = chart_base.join("chart_position_value.png");
        let path = path_buf.to_str().unwrap();

        let vals: Vec<f64> = snapshots.iter().map(|s| s.net_position_value_usd).collect();
        let y_min = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let y_max = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let margin = (y_max - y_min).abs() * 0.1;

        let root = BitMapBackend::new(path, (2400, 1000)).into_drawing_area();
        root.fill(&RGBColor(18, 18, 24)).unwrap();

        let mut chart = ChartBuilder::on(&root)
            .caption("Net Position Value (excl. borrowed USDC)", ("sans-serif", 40).into_font().color(&WHITE))
            .margin(40).x_label_area_size(65).y_label_area_size(90)
            .build_cartesian_2d(x_min..x_max, (y_min - margin)..(y_max + margin)).unwrap();

        chart.configure_mesh()
            .bold_line_style(RGBColor(40, 40, 50)).light_line_style(RGBColor(30, 30, 38))
            .axis_style(RGBColor(120, 120, 140))
            .label_style(("sans-serif", 18).into_font().color(&RGBColor(180, 180, 200)))
            .x_label_formatter(&x_fmt).y_label_formatter(&|v| format!("${:.0}", v))
            .x_desc("Date").y_desc("USD").draw().unwrap();

        chart.draw_series(AreaSeries::new(
            snapshots.iter().map(|s| (block_to_ts(s.block), s.net_position_value_usd)),
            y_min - margin,
            RGBColor(138, 43, 226).mix(0.25),
        ).border_style(ShapeStyle::from(RGBColor(168, 80, 255)).stroke_width(2))).unwrap();

        root.present().unwrap();
        println!("  Saved: {}", path);
    }

    // ── Chart 3: Fee Return % ─────────────────────────────────────────────
    {
        let path_buf = chart_base.join("chart_fee_return.png");
        let path = path_buf.to_str().unwrap();

        let y_max = snapshots.iter().map(|s| s.fee_return_pct).fold(0f64, f64::max) * 1.1;

        let root = BitMapBackend::new(path, (2400, 1000)).into_drawing_area();
        root.fill(&RGBColor(18, 18, 24)).unwrap();

        let mut chart = ChartBuilder::on(&root)
            .caption("Fee Return (%)", ("sans-serif", 40).into_font().color(&WHITE))
            .margin(40).x_label_area_size(65).y_label_area_size(90)
            .build_cartesian_2d(x_min..x_max, 0.0..y_max.max(0.01)).unwrap();

        chart.configure_mesh()
            .bold_line_style(RGBColor(40, 40, 50)).light_line_style(RGBColor(30, 30, 38))
            .axis_style(RGBColor(120, 120, 140))
            .label_style(("sans-serif", 18).into_font().color(&RGBColor(180, 180, 200)))
            .x_label_formatter(&x_fmt).y_label_formatter(&|v| format!("{:.2}%", v))
            .x_desc("Date").y_desc("Return (%)").draw().unwrap();

        chart.draw_series(AreaSeries::new(
            snapshots.iter().map(|s| (block_to_ts(s.block), s.fee_return_pct)),
            0.0, RGBColor(255, 180, 40).mix(0.2),
        ).border_style(ShapeStyle::from(RGBColor(255, 180, 40)).stroke_width(2))).unwrap();

        root.present().unwrap();
        println!("  Saved: {}", path);
    }
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cfg = SimConfig::load();
    let deposit_amount_0 = U256::from_str(&cfg.deposit_weth).expect("bad deposit_weth");
    let dec0 = 10f64.powi(cfg.token0_decimals as i32);
    let dec1 = 10f64.powi(cfg.token1_decimals as i32);

    println!("═══════════════════════════════════════════════════════");
    println!("  Uniswap V3 LP Backtesting Simulator");
    println!("═══════════════════════════════════════════════════════");
    println!();
    println!(
        "  Pool:         {} ({}/{} {:.2}%)",
        &cfg.pool_address, cfg.token0_symbol, cfg.token1_symbol,
        cfg.fee as f64 / 10_000.0
    );
    println!("  Genesis:      {}", cfg.genesis_block);
    println!("  LP entry:     {} (from_block)", cfg.from_block);
    println!("  LP exit:      {} (to_block)", cfg.to_block);
    if let Some(pct) = cfg.price_range_pct {
        println!("  Range:        ±{:.0}% from entry price", pct);
    }
    println!();

    // ── MongoDB setup ───────────────────────────────────────────────────────
    let mongo = MongoClient::with_uri_str(&cfg.mongo_uri)
        .await
        .expect("MongoDB connection failed");
    let db = mongo.database(&cfg.db_name);
    let events_col = db.collection::<Document>("events");

    // ── Create pool ─────────────────────────────────────────────────────────
    let mut pool = UniswapV3Pool::new(cfg.fee, cfg.tick_spacing);
    let mut block_timestamp: u32 = 0;
    let mut last_block: i64 = 0;

    // ════════════════════════════════════════════════════════════════════════
    // Phase 1: Warm-up replay [genesis_block, from_block)
    // ════════════════════════════════════════════════════════════════════════
    println!("Phase 1: Warming up pool state (genesis -> from_block)...");

    let filter_p1 = doc! {
        "blockNumber": { "$gte": cfg.genesis_block as i64, "$lt": cfg.from_block as i64 }
    };
    let sort = doc! { "blockNumber": 1, "transactionIndex": 1, "logIndex": 1 };
    let total_p1 = events_col.count_documents(filter_p1.clone()).await.unwrap();
    println!("  {} events to replay\n", total_p1);

    if total_p1 > 0 {
        let find_opts = FindOptions::builder().sort(sort.clone()).build();
        let mut cursor = events_col.find(filter_p1).with_options(find_opts).await.expect("find failed");

        let start = Instant::now();
        let mut count: u64 = 0;
        while let Some(result) = cursor.next().await {
            let doc = result.expect("cursor error");
            process_event(&mut pool, &doc, &mut block_timestamp, &mut last_block, None);
            count += 1;
            if count % 500 == 0 || count == total_p1 {
                progress::render("warmup", count, total_p1, "", start);
            }
        }
        println!(
            "\n  Phase 1 done: {} events in {}\n",
            count, progress::format_duration(start.elapsed().as_millis())
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    // Phase 2: Inject simulated LP position at from_block
    // ════════════════════════════════════════════════════════════════════════
    println!("Phase 2: Adding simulated LP position...");

    let entry_sqrt_price = pool.slot0.sqrt_price_x96;
    let entry_tick = pool.slot0.tick;
    let entry_price = sqrt_price_to_human(entry_sqrt_price, cfg.token0_decimals, cfg.token1_decimals);

    // Resolve tick range
    let (tick_lower, tick_upper) = if let Some(pct) = cfg.price_range_pct {
        let lower_price = entry_price * (1.0 - pct / 100.0);
        let upper_price = entry_price * (1.0 + pct / 100.0);
        let tl = price_to_tick(lower_price, cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
        let tu = price_to_tick(upper_price, cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
        println!(
            "  Range ±{:.0}%: ${:.2} - ${:.2} -> ticks [{}, {}]",
            pct, lower_price, upper_price, tl, tu
        );
        (tl, tu)
    } else {
        let tl = cfg.tick_lower.expect("tick_lower required when price_range_pct not set");
        let tu = cfg.tick_upper.expect("tick_upper required when price_range_pct not set");
        (tl, tu)
    };

    let sqrt_lower = tick_math::get_sqrt_ratio_at_tick(tick_lower);
    let sqrt_upper = tick_math::get_sqrt_ratio_at_tick(tick_upper);

    // Calculate liquidity from deposit_amount_0 (1 WETH)
    let q96 = U256::from(1) << 96;
    let sim_liquidity: u128 = if entry_tick < tick_lower {
        let intermediate = full_math::mul_div(deposit_amount_0, sqrt_lower, q96);
        full_math::mul_div(intermediate, sqrt_upper, sqrt_upper - sqrt_lower).to::<u128>()
    } else if entry_tick < tick_upper {
        let intermediate = full_math::mul_div(deposit_amount_0, entry_sqrt_price, q96);
        full_math::mul_div(intermediate, sqrt_upper, sqrt_upper - entry_sqrt_price).to::<u128>()
    } else {
        panic!("Price is above tick range — cannot deposit token0 (WETH). Adjust tick range.");
    };
    assert!(sim_liquidity > 0, "Calculated liquidity is zero");

    // Compute actual amounts deposited
    let amount0_deposited = if entry_tick < tick_lower {
        sqrt_price_math::get_amount0_delta(sqrt_lower, sqrt_upper, sim_liquidity, true)
    } else {
        sqrt_price_math::get_amount0_delta(entry_sqrt_price, sqrt_upper, sim_liquidity, true)
    };
    let amount1_deposited = if entry_tick < tick_lower {
        U256::ZERO
    } else {
        sqrt_price_math::get_amount1_delta(sqrt_lower, entry_sqrt_price, sim_liquidity, true)
    };

    // Mint into pool
    pool.mint(SIM_OWNER, tick_lower, tick_upper, sim_liquidity, block_timestamp);

    let a0_human = amount0_deposited.to_string().parse::<f64>().unwrap() / dec0;
    let a1_human = amount1_deposited.to_string().parse::<f64>().unwrap() / dec1;

    // User's actual capital = 1 WETH only. The USDC is borrowed.
    let user_capital_weth = a0_human; // ~1.0 WETH
    let borrowed_usdc = a1_human;     // borrowed from CDP
    let user_capital_usd = user_capital_weth * entry_price; // 1 WETH in USD at entry

    println!("  {} price:       ${:.2}", cfg.token0_symbol, entry_price);
    println!(
        "  Deposited:        {:.6} {} + {:.2} {} (borrowed)",
        a0_human, cfg.token0_symbol, a1_human, cfg.token1_symbol
    );
    println!("  User capital:     {:.6} {} = ${:.2}", user_capital_weth, cfg.token0_symbol, user_capital_usd);
    println!("  Borrowed:         {:.2} {}", borrowed_usdc, cfg.token1_symbol);
    println!("  Liquidity:        {}", sim_liquidity);
    println!();

    // ════════════════════════════════════════════════════════════════════════
    // Phase 3: Replay [from_block, to_block] with time-series data collection
    // ════════════════════════════════════════════════════════════════════════
    println!("Phase 3: Replaying with LP position active...");

    let filter_p3 = doc! {
        "blockNumber": { "$gte": cfg.from_block as i64, "$lte": cfg.to_block as i64 }
    };
    let total_p3 = events_col.count_documents(filter_p3.clone()).await.unwrap();
    println!("  {} events to replay\n", total_p3);

    let sim_pos = Some((tick_lower, tick_upper, sim_liquidity));
    let mut snapshots: Vec<Snapshot> = Vec::new();
    let mut cumulative_volume_usd: f64 = 0.0;

    let mut last_snapshot_block: u64 = 0;

    let arb_block_time = 0.25; // Arbitrum ~0.25s/block
    let seconds_per_year = 365.25 * 86400.0;

    if total_p3 > 0 {
        let find_opts = FindOptions::builder().sort(sort.clone()).build();
        let mut cursor = events_col.find(filter_p3).with_options(find_opts).await.expect("find failed");

        let start = Instant::now();
        let mut count: u64 = 0;
        while let Some(result) = cursor.next().await {
            let event_doc = result.expect("cursor error");
            let block_number = event_doc.get_i64("blockNumber").unwrap_or(0) as u64;
            let event_name = event_doc.get_str("eventName").unwrap_or("");

            // Track volume from swaps
            if event_name == "Swap" {
                let args = event_doc.get_document("args").unwrap();
                let a0 = arg_i256(args, "amount0");
                let _a1 = arg_i256(args, "amount1");
                let current_price = sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals);

                // Volume = |amount0| * price (convert WETH side to USD)
                let a0_abs = if a0 >= I256::ZERO { a0.into_raw() } else { (-a0).into_raw() };
                let vol = a0_abs.to_string().parse::<f64>().unwrap() / dec0 * current_price;
                cumulative_volume_usd += vol;
            }

            process_event(&mut pool, &event_doc, &mut block_timestamp, &mut last_block, sim_pos);
            count += 1;

            // Collect snapshot every 1000 blocks (or at the very end)
            let blocks_since_snapshot = block_number.saturating_sub(last_snapshot_block);
            if blocks_since_snapshot >= 1000 || count == total_p3 {
                last_snapshot_block = block_number;
                let current_price = sqrt_price_to_human(
                    pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals,
                );

                let (fees_0, fees_1, principal_0, principal_1) =
                    compute_accrued_fees(&pool, tick_lower, tick_upper, sim_liquidity);

                let fees_usd = fees_0 / dec0 * current_price + fees_1 / dec1;
                let principal_usd = principal_0 / dec0 * current_price + principal_1 / dec1;
                let total_position_usd = principal_usd + fees_usd;

                // Net = total position - borrowed USDC (must repay)
                let net_position_value = total_position_usd - borrowed_usdc;

                // Overall PnL = net position - user capital (1 WETH at entry price)
                let overall_pnl = net_position_value - user_capital_usd;

                // Current return % (not annualized)
                let fee_return_pct = if user_capital_usd > 0.0 {
                    (fees_usd / user_capital_usd) * 100.0
                } else {
                    0.0
                };

                let overall_return_pct = if user_capital_usd > 0.0 {
                    (overall_pnl / user_capital_usd) * 100.0
                } else {
                    0.0
                };

                snapshots.push(Snapshot {
                    block: block_number,
                    _weth_price: current_price,
                    cumulative_fees_usd: fees_usd,
                    cumulative_volume_usd,
                    net_position_value_usd: net_position_value,
                    overall_pnl_usd: overall_pnl,
                    fee_return_pct,
                    overall_return_pct,
                });
            }

            if count % 500 == 0 || count == total_p3 {
                progress::render("sim", count, total_p3, "", start);
            }
        }
        println!(
            "\n  Phase 3 done: {} events in {} ({} snapshots)\n",
            count, progress::format_duration(start.elapsed().as_millis()), snapshots.len()
        );
    }

    // ════════════════════════════════════════════════════════════════════════
    // Phase 4: Final exit calculations
    // ════════════════════════════════════════════════════════════════════════

    let exit_sqrt_price = pool.slot0.sqrt_price_x96;
    let exit_price = sqrt_price_to_human(exit_sqrt_price, cfg.token0_decimals, cfg.token1_decimals);

    // Burn our position
    let (burn_amount0, burn_amount1) = pool.burn(SIM_OWNER, tick_lower, tick_upper, sim_liquidity, block_timestamp);

    let pos = pool.positions.get(&(SIM_OWNER, tick_lower, tick_upper)).expect("position missing");
    let total_owed_0 = pos.tokens_owed_0;
    let total_owed_1 = pos.tokens_owed_1;

    let principal_0 = burn_amount0.to_string().parse::<u128>().unwrap();
    let principal_1 = burn_amount1.to_string().parse::<u128>().unwrap();
    let fees_0 = total_owed_0.saturating_sub(principal_0);
    let fees_1 = total_owed_1.saturating_sub(principal_1);

    let principal_0_h = principal_0 as f64 / dec0;
    let principal_1_h = principal_1 as f64 / dec1;
    let fees_0_h = fees_0 as f64 / dec0;
    let fees_1_h = fees_1 as f64 / dec1;
    let total_0_h = total_owed_0 as f64 / dec0;
    let total_1_h = total_owed_1 as f64 / dec1;

    let fees_usd = fees_0_h * exit_price + fees_1_h;
    let principal_usd = principal_0_h * exit_price + principal_1_h;
    let total_position_usd = total_0_h * exit_price + total_1_h;

    // Net position = total - borrowed USDC repayment
    let net_position_usd = total_position_usd - borrowed_usdc;
    let overall_pnl = net_position_usd - user_capital_usd;

    // HODL: user just held 1 WETH
    let hodl_1weth_usd = user_capital_weth * exit_price;
    let pnl_vs_hodl = net_position_usd - hodl_1weth_usd;

    // IL: principal (what liquidity is worth) minus what we'd have if we held both tokens
    let hodl_both_usd = a0_human * exit_price + a1_human;
    let il_usd = principal_usd - hodl_both_usd;

    let duration_blocks = cfg.to_block - cfg.from_block;
    let duration_seconds = duration_blocks as f64 * arb_block_time;
    let duration_days = duration_seconds / 86400.0;
    let duration_years = duration_seconds / seconds_per_year;

    // APR on user's 1 WETH capital
    let fee_apr = if user_capital_usd > 0.0 && duration_years > 0.0 {
        (fees_usd / user_capital_usd) / duration_years * 100.0
    } else { 0.0 };

    let overall_apr = if user_capital_usd > 0.0 && duration_years > 0.0 {
        (overall_pnl / user_capital_usd) / duration_years * 100.0
    } else { 0.0 };

    // ── Verify pool state at from_block ─────────────────────────────────────
    println!("Verifying pool state at from_block against on-chain...");
    let rpc_url: url::Url = cfg.rpc.parse().expect("invalid rpc url");
    let provider = ProviderBuilder::new().connect_http(rpc_url);
    let pool_address: Address = cfg.pool_address.parse().expect("invalid pool_address");
    let contract = IUniswapV3Pool::new(pool_address, &provider);

    let block_id = alloy::eips::BlockId::number(cfg.from_block);
    match contract.slot0().block(block_id).call().await {
        Ok(s) => {
            let on_sqrt = U256::from(s.sqrtPriceX96);
            let on_tick = s.tick.as_i32();
            println!("  sqrtPriceX96 at entry: [{}]", if entry_sqrt_price == on_sqrt { "PASS" } else { "FAIL" });
            println!("  tick at entry:         [{}]", if entry_tick == on_tick { "PASS" } else { "FAIL" });
        }
        Err(e) => println!("  Could not verify: {}", e),
    }

    // ── Fetch from_block timestamp for chart dates ────────────────────────
    let from_block_timestamp: u64 = {
        use alloy::eips::BlockNumberOrTag;
        let block = provider
            .get_block_by_number(BlockNumberOrTag::Number(cfg.from_block))
            .await
            .ok()
            .flatten()
            .map(|b| b.header.timestamp)
            .unwrap_or(0);
        block
    };

    // ── Generate charts ─────────────────────────────────────────────────────
    println!("\nGenerating charts...");
    draw_charts(&snapshots, cfg.from_block, cfg.to_block, from_block_timestamp, user_capital_usd);

    // ── Output Report ───────────────────────────────────────────────────────
    println!();
    println!("═══════════════════════════════════════════════════════");
    println!("  LP Backtesting Report");
    println!("═══════════════════════════════════════════════════════");
    println!();
    println!("  Pool:           {}/{} ({:.2}%)", cfg.token0_symbol, cfg.token1_symbol, cfg.fee as f64 / 10_000.0);
    println!("  Tick range:     [{}, {}]", tick_lower, tick_upper);
    println!(
        "  Price range:    ${:.2} - ${:.2}",
        tick_to_price(tick_lower, cfg.token0_decimals, cfg.token1_decimals),
        tick_to_price(tick_upper, cfg.token0_decimals, cfg.token1_decimals),
    );
    println!("  Duration:       {} blocks (~{:.1} days)", duration_blocks, duration_days);
    println!();
    println!("─── Capital Structure ─────────────────────────────────");
    println!("  User deposit:      {:.6} {} (${:.2})", user_capital_weth, cfg.token0_symbol, user_capital_usd);
    println!("  Borrowed (CDP):    {:.2} {}", borrowed_usdc, cfg.token1_symbol);
    println!("  Total in pool:     {:.6} {} + {:.2} {}", a0_human, cfg.token0_symbol, a1_human, cfg.token1_symbol);
    println!();
    println!("─── Entry ────────────────────────────────────────────");
    println!("  {} price:       ${:.2}", cfg.token0_symbol, entry_price);
    println!("  User capital:     ${:.2} ({:.6} {})", user_capital_usd, user_capital_weth, cfg.token0_symbol);
    println!();
    println!("─── Exit ─────────────────────────────────────────────");
    println!("  {} price:       ${:.2}", cfg.token0_symbol, exit_price);
    println!("  Principal:        {:.6} {} + {:.2} {}", principal_0_h, cfg.token0_symbol, principal_1_h, cfg.token1_symbol);
    println!(
        "  Fees earned:      {:.6} {} (${:.2}) + {:.2} {} (${:.2})",
        fees_0_h, cfg.token0_symbol, fees_0_h * exit_price, fees_1_h, cfg.token1_symbol, fees_1_h,
    );
    println!("  Total fees:       ${:.2}", fees_usd);
    println!("  Position total:   ${:.2}", total_position_usd);
    println!("  Repay borrowed:   -${:.2}", borrowed_usdc);
    println!("  Net to user:      ${:.2}", net_position_usd);
    println!();
    println!("─── PnL (on 1 WETH capital) ────────────────────────");
    println!("  IL (vs hold both):     {}{:.2}", if il_usd >= 0.0 { "+$" } else { "-$" }, il_usd.abs());
    println!("  Fees earned:           +${:.2}", fees_usd);
    println!("  vs HODL 1 WETH:        {}{:.2}", if pnl_vs_hodl >= 0.0 { "+$" } else { "-$" }, pnl_vs_hodl.abs());
    println!(
        "  Overall PnL:           {}{:.2} ({:.2}%)",
        if overall_pnl >= 0.0 { "+$" } else { "-$" },
        overall_pnl.abs(),
        if user_capital_usd > 0.0 { overall_pnl / user_capital_usd * 100.0 } else { 0.0 }
    );
    println!("  Fee APR:               {:.1}%", fee_apr);
    println!("  Overall APR:           {:.1}%", overall_apr);
    println!();
    println!("  Volume traded:         ${:.2}", cumulative_volume_usd);
    println!();
    println!("═══════════════════════════════════════════════════════");
}
