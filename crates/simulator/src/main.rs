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
    deposit_weth: String,
    token0_decimals: u8,
    token1_decimals: u8,
    token0_symbol: String,
    token1_symbol: String,
    wide_range_pct: f64,
    base_range_pct: f64,
    limit_order_pct: f64,
    wide_alloc_pct: f64,
    rebal_wide_alloc_pct: f64,
    rebalance_price_pct: f64,
    rebalance_interval_blocks: u64,
}

impl SimConfig {
    fn load() -> Self {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sim_config.toml");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("{} not found", path.display()));
        toml::from_str(&raw).expect("invalid sim_config.toml")
    }
}

sol! {
    #[sol(rpc)]
    contract IUniswapV3Pool {
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked);
        function liquidity() external view returns (uint128);
    }
}

// ── Arg parsing ─────────────────────────────────────────────────────────────

fn arg_str<'a>(d: &'a Document, k: &str) -> &'a str { d.get_str(k).unwrap_or_else(|_| panic!("missing '{}'", k)) }
fn arg_u256(d: &Document, k: &str) -> U256 { U256::from_str(arg_str(d, k)).unwrap() }
fn arg_i256(d: &Document, k: &str) -> I256 { I256::from_dec_str(arg_str(d, k)).unwrap() }
fn arg_i32(d: &Document, k: &str) -> i32 { arg_str(d, k).parse().unwrap() }
fn arg_u128(d: &Document, k: &str) -> u128 { arg_str(d, k).parse().unwrap() }
fn arg_u8(d: &Document, k: &str) -> u8 { arg_str(d, k).parse().unwrap() }
fn arg_u16(d: &Document, k: &str) -> u16 { arg_str(d, k).parse().unwrap() }
fn arg_address(d: &Document, k: &str) -> Address { Address::from_str(arg_str(d, k)).unwrap() }

// ── Price helpers ───────────────────────────────────────────────────────────

fn sqrt_price_to_human(sp: U256, d0: u8, d1: u8) -> f64 {
    let s: f64 = sp.to_string().parse().unwrap();
    let q = 2f64.powi(96);
    (s / q).powi(2) * 10f64.powi(d0 as i32 - d1 as i32)
}

fn price_to_tick(price: f64, d0: u8, d1: u8, ts: i32) -> i32 {
    let raw = price / 10f64.powi(d0 as i32 - d1 as i32);
    let t = (raw.ln() / 1.0001f64.ln()).round() as i32;
    (t / ts) * ts
}

// ── Virtual Position (not minted into pool) ─────────────────────────────────

#[derive(Clone, Debug)]
struct VirtualPosition {
    tick_lower: i32,
    tick_upper: i32,
    liquidity: u128,
    /// feeGrowthInside at the time this position was created/last collected
    fg_inside_0_last: U256,
    fg_inside_1_last: U256,
    /// Raw token amounts deposited (for capping principal on recovery)
    deposited_t0: U256,
    deposited_t1: U256,
    #[allow(dead_code)]
    label: &'static str,
}

impl VirtualPosition {
    fn new(pool: &UniswapV3Pool, tl: i32, tu: i32, liq: u128, label: &'static str) -> Self {
        let (fg0, fg1) = tick::get_fee_growth_inside(
            &pool.ticks, tl, tu, pool.slot0.tick,
            pool.fee_growth_global_0_x128, pool.fee_growth_global_1_x128,
        );
        let (d0, d1) = amounts_for_liquidity(liq, pool.slot0.sqrt_price_x96, tl, tu, pool.slot0.tick);
        VirtualPosition { tick_lower: tl, tick_upper: tu, liquidity: liq, fg_inside_0_last: fg0, fg_inside_1_last: fg1, deposited_t0: d0, deposited_t1: d1, label }
    }

    /// Compute pending fees since last update (read-only, does NOT modify state)
    fn pending_fees_usd(&self, pool: &UniswapV3Pool, price: f64, dec0: f64, dec1: f64) -> f64 {
        if self.liquidity == 0 { return 0.0; }
        let (fg0, fg1) = tick::get_fee_growth_inside(
            &pool.ticks, self.tick_lower, self.tick_upper, pool.slot0.tick,
            pool.fee_growth_global_0_x128, pool.fee_growth_global_1_x128,
        );
        // Fee growth inside a fixed range should only increase.
        // If it appears to decrease, a tick crossing flipped inside/outside —
        // use saturating subtraction (treat negative delta as 0 fees).
        let delta0 = if fg0 >= self.fg_inside_0_last { fg0 - self.fg_inside_0_last } else { U256::ZERO };
        let delta1 = if fg1 >= self.fg_inside_1_last { fg1 - self.fg_inside_1_last } else { U256::ZERO };
        let f0 = full_math::mul_div(delta0, U256::from(self.liquidity), FIXED_POINT_128_Q128);
        let f1 = full_math::mul_div(delta1, U256::from(self.liquidity), FIXED_POINT_128_Q128);
        // Result fits in u128 (fee per unit liquidity * our small liquidity)
        let f0_f = f0.to_string().parse::<f64>().unwrap_or(0.0);
        let f1_f = f1.to_string().parse::<f64>().unwrap_or(0.0);
        (f0_f / dec0) * price + (f1_f / dec1)
    }


    /// Current principal value (what tokens we'd get if we burned)
    fn principal(&self, pool: &UniswapV3Pool) -> (U256, U256) {
        if self.liquidity == 0 { return (U256::ZERO, U256::ZERO); }
        let sqrt_lower = tick_math::get_sqrt_ratio_at_tick(self.tick_lower);
        let sqrt_upper = tick_math::get_sqrt_ratio_at_tick(self.tick_upper);
        let sp = pool.slot0.sqrt_price_x96;
        let ct = pool.slot0.tick;

        let a0 = if ct < self.tick_lower {
            sqrt_price_math::get_amount0_delta(sqrt_lower, sqrt_upper, self.liquidity, false)
        } else if ct < self.tick_upper {
            sqrt_price_math::get_amount0_delta(sp, sqrt_upper, self.liquidity, false)
        } else { U256::ZERO };

        let a1 = if ct >= self.tick_upper {
            sqrt_price_math::get_amount1_delta(sqrt_lower, sqrt_upper, self.liquidity, false)
        } else if ct >= self.tick_lower {
            sqrt_price_math::get_amount1_delta(sqrt_lower, sp, self.liquidity, false)
        } else { U256::ZERO };

        (a0, a1)
    }

    /// Principal value in USD, capped to deposited value (virtual positions can't gain principal)
    fn principal_usd(&self, pool: &UniswapV3Pool, price: f64, dec0: f64, dec1: f64) -> f64 {
        let (p0, p1) = self.principal(pool);
        let current = p0.to_string().parse::<f64>().unwrap() / dec0 * price + p1.to_string().parse::<f64>().unwrap() / dec1;
        let deposited = self.deposited_t0.to_string().parse::<f64>().unwrap() / dec0 * price
            + self.deposited_t1.to_string().parse::<f64>().unwrap() / dec1;
        current.min(deposited)
    }
}

/// Compute liquidity from token0 amount for a centered position
fn liquidity_from_amount0(amount0: U256, sqrt_price: U256, tl: i32, tu: i32, current_tick: i32) -> u128 {
    let q96 = U256::from(1) << 96;
    let sl = tick_math::get_sqrt_ratio_at_tick(tl);
    let su = tick_math::get_sqrt_ratio_at_tick(tu);
    if current_tick < tl {
        let i = full_math::mul_div(amount0, sl, q96);
        full_math::mul_div(i, su, su - sl).to::<u128>()
    } else if current_tick < tu {
        let i = full_math::mul_div(amount0, sqrt_price, q96);
        full_math::mul_div(i, su, su - sqrt_price).to::<u128>()
    } else { 0 }
}

fn liquidity_from_amount1(amount1: U256, sqrt_price: U256, tl: i32, tu: i32, current_tick: i32) -> u128 {
    let q96 = U256::from(1) << 96;
    let sl = tick_math::get_sqrt_ratio_at_tick(tl);
    let su = tick_math::get_sqrt_ratio_at_tick(tu);
    if current_tick >= tu {
        full_math::mul_div(amount1, q96, su - sl).to::<u128>()
    } else if current_tick >= tl {
        full_math::mul_div(amount1, q96, sqrt_price - sl).to::<u128>()
    } else { 0 }
}

fn amounts_for_liquidity(liq: u128, sp: U256, tl: i32, tu: i32, ct: i32) -> (U256, U256) {
    let sl = tick_math::get_sqrt_ratio_at_tick(tl);
    let su = tick_math::get_sqrt_ratio_at_tick(tu);
    let a0 = if ct < tl { sqrt_price_math::get_amount0_delta(sl, su, liq, true) }
        else if ct < tu { sqrt_price_math::get_amount0_delta(sp, su, liq, true) }
        else { U256::ZERO };
    let a1 = if ct >= tu { sqrt_price_math::get_amount1_delta(sl, su, liq, true) }
        else if ct >= tl { sqrt_price_math::get_amount1_delta(sl, sp, liq, true) }
        else { U256::ZERO };
    (a0, a1)
}

// ── Deploy virtual positions ────────────────────────────────────────────────

fn deploy_virtual(
    pool: &UniswapV3Pool,
    total_t0: U256,
    total_t1: U256,
    cfg: &SimConfig,
    wide_alloc: f64,
    dec0: f64,
    dec1: f64,
) -> (VirtualPosition, VirtualPosition, Option<VirtualPosition>, f64) {
    let price = sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals);
    let ct = pool.slot0.tick;
    let sp = pool.slot0.sqrt_price_x96;

    let wide_tl = price_to_tick(price * (1.0 - cfg.wide_range_pct / 100.0), cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
    let wide_tu = price_to_tick(price * (1.0 + cfg.wide_range_pct / 100.0), cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
    let base_tl = price_to_tick(price * (1.0 - cfg.base_range_pct / 100.0), cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
    let base_tu = price_to_tick(price * (1.0 + cfg.base_range_pct / 100.0), cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);

    let t0f = total_t0.to_string().parse::<f64>().unwrap();
    let t1f = total_t1.to_string().parse::<f64>().unwrap();
    let total_usd: f64 = t0f / dec0 * price + t1f / dec1;

    // For a given range, compute the USD value per unit of liquidity.
    // Then allocate liquidity such that wide_value = wide_alloc% of total_usd.
    let usd_per_liq = |tl: i32, tu: i32| -> f64 {
        let test_liq: u128 = 1_000_000_000_000; // 1e12 reference
        let (a0, a1) = amounts_for_liquidity(test_liq, sp, tl, tu, ct);
        let v0 = a0.to_string().parse::<f64>().unwrap() / dec0 * price;
        let v1 = a1.to_string().parse::<f64>().unwrap() / dec1;
        (v0 + v1) / test_liq as f64
    };

    let wide_upl = usd_per_liq(wide_tl, wide_tu);
    let base_upl = usd_per_liq(base_tl, base_tu);

    let wide_target_usd = total_usd * wide_alloc / 100.0;
    let base_target_usd = total_usd * (100.0 - wide_alloc) / 100.0;

    let wide_liq = if wide_upl > 0.0 { (wide_target_usd / wide_upl) as u128 } else { 0 };
    let base_liq = if base_upl > 0.0 { (base_target_usd / base_upl) as u128 } else { 0 };

    let (w0, w1) = amounts_for_liquidity(wide_liq, sp, wide_tl, wide_tu, ct);
    let (b0, b1) = amounts_for_liquidity(base_liq, sp, base_tl, base_tu, ct);
    let used_t0 = w0 + b0;
    let used_t1 = w1 + b1;

    // If we need more tokens than available, scale down proportionally
    let scale_t0 = if used_t0 > total_t0 {
        total_t0.to_string().parse::<f64>().unwrap() / used_t0.to_string().parse::<f64>().unwrap()
    } else { 1.0 };
    let scale_t1 = if used_t1 > total_t1 {
        total_t1.to_string().parse::<f64>().unwrap() / used_t1.to_string().parse::<f64>().unwrap()
    } else { 1.0 };
    let scale = scale_t0.min(scale_t1);

    let wide_liq = (wide_liq as f64 * scale) as u128;
    let base_liq = (base_liq as f64 * scale) as u128;

    let (w0, w1) = amounts_for_liquidity(wide_liq, sp, wide_tl, wide_tu, ct);
    let (b0, b1) = amounts_for_liquidity(base_liq, sp, base_tl, base_tu, ct);
    let used_t0 = w0 + b0;
    let used_t1 = w1 + b1;
    let excess_t0 = total_t0.saturating_sub(used_t0);
    let excess_t1 = total_t1.saturating_sub(used_t1);

    let excess_t0_usd = excess_t0.to_string().parse::<f64>().unwrap() / dec0 * price;
    let excess_t1_usd = excess_t1.to_string().parse::<f64>().unwrap() / dec1;

    let borrowed = total_t1.to_string().parse::<f64>().unwrap() / dec1;

    let wide = VirtualPosition::new(pool, wide_tl, wide_tu, wide_liq, "wide");
    let base = VirtualPosition::new(pool, base_tl, base_tu, base_liq, "base");

    // Limit order: cap excess to 10% of total value to prevent
    // disproportionate concentrated liquidity
    let max_limit_usd = total_usd * 0.10;
    let capped_excess_t1 = if excess_t1_usd > max_limit_usd {
        U256::from((max_limit_usd * dec1) as u128)
    } else { excess_t1 };
    let capped_excess_t0 = if excess_t0_usd > max_limit_usd {
        U256::from((max_limit_usd / price * dec0) as u128)
    } else { excess_t0 };
    let capped_t1_usd = capped_excess_t1.to_string().parse::<f64>().unwrap() / dec1;
    let capped_t0_usd = capped_excess_t0.to_string().parse::<f64>().unwrap() / dec0 * price;

    let mut limit = None;
    if capped_t1_usd > 0.5 {
        let lo = ct / cfg.tick_spacing * cfg.tick_spacing + cfg.tick_spacing;
        let hi = price_to_tick(price * (1.0 + cfg.limit_order_pct / 100.0), cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
        if hi > lo {
            let liq = liquidity_from_amount1(capped_excess_t1, sp, lo, hi, ct);
            if liq > 0 { limit = Some(VirtualPosition::new(pool, lo, hi, liq, "limit")); }
        }
    } else if capped_t0_usd > 0.5 {
        let lo = price_to_tick(price * (1.0 - cfg.limit_order_pct / 100.0), cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
        let hi = ct / cfg.tick_spacing * cfg.tick_spacing;
        if hi > lo {
            let liq = liquidity_from_amount0(capped_excess_t0, sp, lo, hi, ct);
            if liq > 0 { limit = Some(VirtualPosition::new(pool, lo, hi, liq, "limit")); }
        }
    }

    (wide, base, limit, borrowed)
}

// ── Process event (clean replay, no simulated positions) ────────────────────

fn process_event(pool: &mut UniswapV3Pool, doc: &Document, ts: &mut u32, last_block: &mut i64) {
    let name = doc.get_str("eventName").unwrap();
    let args = doc.get_document("args").unwrap();
    let bn = doc.get_i64("blockNumber").unwrap_or(0);
    if bn != *last_block { *ts += 1; *last_block = bn; }

    match name {
        "Initialize" => { pool.initialize(arg_u256(args, "sqrtPriceX96")); }
        "Mint" => { pool.mint(arg_address(args, "owner"), arg_i32(args, "tickLower"), arg_i32(args, "tickUpper"), arg_u128(args, "amount"), *ts); }
        "Burn" => { pool.burn(arg_address(args, "owner"), arg_i32(args, "tickLower"), arg_i32(args, "tickUpper"), arg_u128(args, "amount"), *ts); }
        "Swap" => {
            let a0 = arg_i256(args, "amount0");
            let esp = arg_u256(args, "sqrtPriceX96");
            let et = arg_i32(args, "tick");
            let el = arg_u128(args, "liquidity");
            let z = a0 > I256::ZERO;
            let amt = if z { a0 } else { arg_i256(args, "amount1") };
            let lim = if z { tick_math::MIN_SQRT_RATIO + U256::from(1) } else { tick_math::MAX_SQRT_RATIO - U256::from(1) };
            pool.swap(z, amt, lim, *ts);
            pool.slot0.sqrt_price_x96 = esp;
            pool.slot0.tick = et;
            pool.liquidity = el;
        }
        "Collect" => { pool.collect(arg_address(args, "owner"), arg_i32(args, "tickLower"), arg_i32(args, "tickUpper"), arg_u128(args, "amount0"), arg_u128(args, "amount1")); }
        "Flash" => { pool.flash_with_paid(arg_u256(args, "paid0"), arg_u256(args, "paid1")); }
        "SetFeeProtocol" => { pool.set_fee_protocol(arg_u8(args, "feeProtocol0New"), arg_u8(args, "feeProtocol1New")); }
        "IncreaseObservationCardinalityNext" => { pool.increase_observation_cardinality_next(arg_u16(args, "observationCardinalityNextNew")); }
        "CollectProtocol" => {
            let a0 = arg_u128(args, "amount0"); let a1 = arg_u128(args, "amount1");
            if a0 > 0 { pool.protocol_fees.token0 = pool.protocol_fees.token0.saturating_sub(a0); }
            if a1 > 0 { pool.protocol_fees.token1 = pool.protocol_fees.token1.saturating_sub(a1); }
        }
        _ => {}
    }
}

// ── Snapshot ─────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Snapshot { block: u64, cumulative_fees_usd: f64, net_position_value_usd: f64, fee_return_pct: f64 }

// ── Charting ────────────────────────────────────────────────────────────────

fn draw_charts(snapshots: &[Snapshot], from_block: u64, from_ts: u64) {
    if snapshots.len() < 2 { println!("  Not enough data."); return; }
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let bt = 0.25f64;
    let b2t = |b: u64| -> f64 { from_ts as f64 + (b as f64 - from_block as f64) * bt };
    let fmt_d = |ts: f64| -> String { chrono::DateTime::from_timestamp(ts as i64, 0).unwrap().naive_utc().format("%b %d").to_string() };
    let fmt_dt = |ts: f64| -> String { chrono::DateTime::from_timestamp(ts as i64, 0).unwrap().naive_utc().format("%b %d %H:%M").to_string() };
    let xn = b2t(from_block); let xx = b2t(snapshots.last().unwrap().block);
    let ud = (xx-xn) < 3.0*86400.0;
    let xf = move |v:&f64| -> String { if ud { fmt_dt(*v) } else { fmt_d(*v) } };

    for (name, title, get_y, yfmt) in [
        ("chart_fees.png", "Cumulative Fees (USD)", Box::new(|s:&Snapshot| s.cumulative_fees_usd) as Box<dyn Fn(&Snapshot)->f64>, Box::new(|v:&f64| format!("${:.2}",v)) as Box<dyn Fn(&f64)->String>),
        ("chart_position_value.png", "Net Position Value (excl. borrowed USDC)", Box::new(|s:&Snapshot| s.net_position_value_usd), Box::new(|v:&f64| format!("${:.0}",v))),
        ("chart_fee_return.png", "Fee Return (%)", Box::new(|s:&Snapshot| s.fee_return_pct), Box::new(|v:&f64| format!("{:.2}%",v))),
    ] {
        let p = base.join(name); let path = p.to_str().unwrap();
        let vals: Vec<f64> = snapshots.iter().map(|s| get_y(s)).collect();
        let ylo = vals.iter().cloned().fold(f64::INFINITY, f64::min);
        let yhi = vals.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let m = (yhi-ylo).abs()*0.1;
        let root = BitMapBackend::new(path, (2400,1000)).into_drawing_area();
        root.fill(&RGBColor(18,18,24)).unwrap();
        let mut c = ChartBuilder::on(&root).caption(title, ("sans-serif",40).into_font().color(&WHITE))
            .margin(40).x_label_area_size(65).y_label_area_size(90)
            .build_cartesian_2d(xn..xx, (ylo-m).min(0.0)..(yhi+m).max(0.01)).unwrap();
        c.configure_mesh().bold_line_style(RGBColor(40,40,50)).light_line_style(RGBColor(30,30,38))
            .axis_style(RGBColor(120,120,140)).label_style(("sans-serif",18).into_font().color(&RGBColor(180,180,200)))
            .x_label_formatter(&xf).y_label_formatter(&*yfmt).x_desc("Date").draw().unwrap();
        c.draw_series(AreaSeries::new(snapshots.iter().map(|s| (b2t(s.block), get_y(s))), (ylo-m).min(0.0), RGBColor(0,220,160).mix(0.2))
            .border_style(ShapeStyle::from(RGBColor(0,220,160)).stroke_width(2))).unwrap();
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
    println!("  Uniswap V3 LP Strategy Backtester");
    println!("═══════════════════════════════════════════════════════\n");
    println!("  Pool:            {}/{} ({:.2}%)", cfg.token0_symbol, cfg.token1_symbol, cfg.fee as f64 / 10_000.0);
    println!("  Genesis:         {}", cfg.genesis_block);
    println!("  LP entry:        {}", cfg.from_block);
    println!("  LP exit:         {}", cfg.to_block);
    println!("  Wide: ±{:.0}%  Base: ±{:.0}%  Limit: ±{:.0}%", cfg.wide_range_pct, cfg.base_range_pct, cfg.limit_order_pct);
    println!("  Init alloc:      {:.0}% wide / {:.0}% base", cfg.wide_alloc_pct, 100.0 - cfg.wide_alloc_pct);
    println!("  Rebal alloc:     {:.0}% wide / {:.0}% base", cfg.rebal_wide_alloc_pct, 100.0 - cfg.rebal_wide_alloc_pct);
    println!("  Rebal trigger:   >{:.0}% price OR every {} blocks\n", cfg.rebalance_price_pct, cfg.rebalance_interval_blocks);

    let mongo = MongoClient::with_uri_str(&cfg.mongo_uri).await.expect("MongoDB failed");
    let events_col = mongo.database(&cfg.db_name).collection::<Document>("events");
    let mut pool = UniswapV3Pool::new(cfg.fee, cfg.tick_spacing);
    let mut ts: u32 = 0;
    let mut lb: i64 = 0;

    // Phase 1: warmup
    println!("Phase 1: Warming up...");
    let f1 = doc! { "blockNumber": { "$gte": cfg.genesis_block as i64, "$lt": cfg.from_block as i64 } };
    let sort = doc! { "blockNumber": 1, "transactionIndex": 1, "logIndex": 1 };
    let n1 = events_col.count_documents(f1.clone()).await.unwrap();
    println!("  {} events\n", n1);
    if n1 > 0 {
        let mut cur = events_col.find(f1).with_options(FindOptions::builder().sort(sort.clone()).build()).await.unwrap();
        let t = Instant::now(); let mut c: u64 = 0;
        while let Some(r) = cur.next().await { process_event(&mut pool, &r.unwrap(), &mut ts, &mut lb); c += 1; if c%500==0||c==n1 { progress::render("warmup",c,n1,"",t); } }
        println!("\n  Done: {} events in {}\n", c, progress::format_duration(t.elapsed().as_millis()));
    }

    // Phase 2: deploy virtual positions
    println!("Phase 2: Deploying strategy...");
    let entry_sp = pool.slot0.sqrt_price_x96;
    let entry_tick = pool.slot0.tick;
    let entry_price = sqrt_price_to_human(entry_sp, cfg.token0_decimals, cfg.token1_decimals);
    let user_capital_usd = deposit_amount_0.to_string().parse::<f64>().unwrap() / dec0 * entry_price;
    let initial_usdc = U256::from((entry_price * dec1) as u128);

    let (mut wide, mut base, mut limit, borrowed_usdc) = deploy_virtual(
        &pool, deposit_amount_0, initial_usdc, &cfg, cfg.wide_alloc_pct, dec0, dec1,
    );
    let mut cumulative_fees_usd: f64 = 0.0;
    let mut rebalance_count: u32 = 0;

    println!("  {} price:       ${:.2}", cfg.token0_symbol, entry_price);
    println!("  User capital:     ${:.2}", user_capital_usd);
    println!("  Borrowed:         {:.2} {}", borrowed_usdc, cfg.token1_symbol);
    println!("  Wide:  [{}, {}] liq={}", wide.tick_lower, wide.tick_upper, wide.liquidity);
    println!("  Base:  [{}, {}] liq={}", base.tick_lower, base.tick_upper, base.liquidity);
    if let Some(ref l) = limit { println!("  Limit: [{}, {}] liq={}", l.tick_lower, l.tick_upper, l.liquidity); }
    println!();

    // Phase 3: replay with virtual position tracking
    println!("Phase 3: Replaying...");
    let f3 = doc! { "blockNumber": { "$gte": cfg.from_block as i64, "$lte": cfg.to_block as i64 } };
    let n3 = events_col.count_documents(f3.clone()).await.unwrap();
    println!("  {} events\n", n3);

    let mut snapshots: Vec<Snapshot> = Vec::new();
    let mut last_snap: u64 = 0;
    let mut last_rebal_price = entry_price;
    let mut last_rebal_block = cfg.from_block;

    if n3 > 0 {
        let mut cur = events_col.find(f3).with_options(FindOptions::builder().sort(sort.clone()).build()).await.unwrap();
        let t = Instant::now(); let mut c: u64 = 0;

        while let Some(r) = cur.next().await {
            let doc = r.unwrap();
            let bn = doc.get_i64("blockNumber").unwrap_or(0) as u64;

            // Replay event (clean — no sim positions minted)
            process_event(&mut pool, &doc, &mut ts, &mut lb);
            c += 1;

            // Check rebalance
            let price = sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals);
            let price_move = ((price - last_rebal_price) / last_rebal_price).abs() * 100.0;
            let blocks_since = bn.saturating_sub(last_rebal_block);

            if (price_move >= cfg.rebalance_price_pct || blocks_since >= cfg.rebalance_interval_blocks) && c > 1 {
                // Collect pending fees into cumulative tracker
                cumulative_fees_usd += wide.pending_fees_usd(&pool, price, dec0, dec1);
                cumulative_fees_usd += base.pending_fees_usd(&pool, price, dec0, dec1);
                if let Some(ref l) = limit { cumulative_fees_usd += l.pending_fees_usd(&pool, price, dec0, dec1); }

                // Principal tokens we'd recover, capped to deposited amounts per position.
                // Virtual positions can shift between tokens but total value can't exceed deposit.
                let cap = |p: (U256, U256), d0: U256, d1: U256| -> (U256, U256) {
                    (p.0.min(d0), p.1.min(d1))
                };
                let (wp0, wp1) = cap(wide.principal(&pool), wide.deposited_t0, wide.deposited_t1);
                let (bp0, bp1) = cap(base.principal(&pool), base.deposited_t0, base.deposited_t1);
                let (lp0, lp1) = limit.as_ref().map_or((U256::ZERO, U256::ZERO), |l| {
                    // Limit order: deposited single-sided. When converted, tokens shift.
                    // Cap total value (not per-token) to deposited value.
                    let (p0, p1) = l.principal(&pool);
                    let dep_total = l.deposited_t0 + l.deposited_t1;
                    let cur_total = p0 + p1;
                    if cur_total > dep_total {
                        // Scale down proportionally
                        let scale_num = dep_total;
                        let scale_den = cur_total;
                        (full_math::mul_div(p0, scale_num, scale_den), full_math::mul_div(p1, scale_num, scale_den))
                    } else { (p0, p1) }
                });

                let tot_t0 = wp0 + bp0 + lp0;
                let tot_t1 = wp1 + bp1 + lp1;

                let (nw, nb, nl, _) = deploy_virtual(&pool, tot_t0, tot_t1, &cfg, cfg.rebal_wide_alloc_pct, dec0, dec1);
                wide = nw; base = nb; limit = nl;
                last_rebal_price = price;
                last_rebal_block = bn;
                rebalance_count += 1;
            }

            // Snapshot every 1000 blocks
            if bn.saturating_sub(last_snap) >= 1000 || c == n3 {
                last_snap = bn;
                let price = sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals);

                let pending_fees = wide.pending_fees_usd(&pool, price, dec0, dec1)
                    + base.pending_fees_usd(&pool, price, dec0, dec1)
                    + limit.as_ref().map_or(0.0, |l| l.pending_fees_usd(&pool, price, dec0, dec1));

                let total_fees = cumulative_fees_usd + pending_fees;
                let principal = wide.principal_usd(&pool, price, dec0, dec1)
                    + base.principal_usd(&pool, price, dec0, dec1)
                    + limit.as_ref().map_or(0.0, |l| l.principal_usd(&pool, price, dec0, dec1));
                let net_val = principal + total_fees - borrowed_usdc;
                let fee_ret = if user_capital_usd > 0.0 { total_fees / user_capital_usd * 100.0 } else { 0.0 };

                snapshots.push(Snapshot { block: bn, cumulative_fees_usd: total_fees, net_position_value_usd: net_val, fee_return_pct: fee_ret });
            }

            if c%500==0||c==n3 { progress::render("sim",c,n3,"",t); }
        }
        println!("\n  Done: {} events, {} rebalances, {} snapshots\n", c, rebalance_count, snapshots.len());
    }

    // Phase 4: exit
    let exit_price = sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals);

    let pending_fees = wide.pending_fees_usd(&pool, exit_price, dec0, dec1)
        + base.pending_fees_usd(&pool, exit_price, dec0, dec1)
        + limit.as_ref().map_or(0.0, |l| l.pending_fees_usd(&pool, exit_price, dec0, dec1));
    cumulative_fees_usd += pending_fees;

    let principal = wide.principal_usd(&pool, exit_price, dec0, dec1)
        + base.principal_usd(&pool, exit_price, dec0, dec1)
        + limit.as_ref().map_or(0.0, |l| l.principal_usd(&pool, exit_price, dec0, dec1));
    let total_val = principal + cumulative_fees_usd;
    let net_val = total_val - borrowed_usdc;
    let overall_pnl = net_val - user_capital_usd;
    let hodl = deposit_amount_0.to_string().parse::<f64>().unwrap() / dec0 * exit_price;
    let vs_hodl = net_val - hodl;
    let duration_days = (cfg.to_block - cfg.from_block) as f64 * 0.25 / 86400.0;

    // Verify
    println!("Verifying entry state...");
    let rpc_url: url::Url = cfg.rpc.parse().unwrap();
    let prov = ProviderBuilder::new().connect_http(rpc_url);
    let contract = IUniswapV3Pool::new(cfg.pool_address.parse::<Address>().unwrap(), &prov);
    match contract.slot0().block(alloy::eips::BlockId::number(cfg.from_block)).call().await {
        Ok(s) => {
            println!("  sqrtPriceX96: [{}]", if entry_sp == U256::from(s.sqrtPriceX96) { "PASS" } else { "FAIL" });
            println!("  tick:         [{}]", if entry_tick == s.tick.as_i32() { "PASS" } else { "FAIL" });
        }
        Err(e) => println!("  Could not verify: {}", e),
    }

    let from_ts: u64 = {
        use alloy::eips::BlockNumberOrTag;
        prov.get_block_by_number(BlockNumberOrTag::Number(cfg.from_block)).await.ok().flatten().map(|b| b.header.timestamp).unwrap_or(0)
    };
    println!("\nGenerating charts...");
    draw_charts(&snapshots, cfg.from_block, from_ts);

    println!("\n═══════════════════════════════════════════════════════");
    println!("  Strategy Backtesting Report");
    println!("═══════════════════════════════════════════════════════\n");
    println!("  Duration:        {} blocks (~{:.1} days)", cfg.to_block - cfg.from_block, duration_days);
    println!("  Rebalances:      {}\n", rebalance_count);
    println!("─── Entry ────────────────────────────────────────────");
    println!("  {} price:       ${:.2}", cfg.token0_symbol, entry_price);
    println!("  User capital:     ${:.2}", user_capital_usd);
    println!("  Borrowed:         {:.2} {}\n", borrowed_usdc, cfg.token1_symbol);
    println!("─── Exit ─────────────────────────────────────────────");
    println!("  {} price:       ${:.2}", cfg.token0_symbol, exit_price);
    println!("  Position total:   ${:.2}", total_val);
    println!("  Repay borrowed:   -${:.2}", borrowed_usdc);
    println!("  Net to user:      ${:.2}\n", net_val);
    println!("─── PnL (on 1 WETH capital) ────────────────────────");
    println!("  Fees earned:      ${:.2}", cumulative_fees_usd);
    println!("  vs HODL 1 WETH:   {}{:.2}", if vs_hodl >= 0.0 { "+$" } else { "-$" }, vs_hodl.abs());
    println!("  Overall PnL:      {}{:.2} ({:.2}%)", if overall_pnl >= 0.0 { "+$" } else { "-$" }, overall_pnl.abs(), if user_capital_usd > 0.0 { overall_pnl/user_capital_usd*100.0 } else { 0.0 });
    println!("  Fee return:       {:.2}%\n", if user_capital_usd > 0.0 { cumulative_fees_usd/user_capital_usd*100.0 } else { 0.0 });
    println!("═══════════════════════════════════════════════════════");
}
