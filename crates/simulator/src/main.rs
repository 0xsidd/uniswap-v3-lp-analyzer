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

// ── Sentinel addresses ──────────────────────────────────────────────────────
const SIM_WIDE: Address = Address::new([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,1,0xDE,0xAD,0xBE,0xEF]);
const SIM_BASE: Address = Address::new([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,2,0xDE,0xAD,0xBE,0xEF]);
#[allow(dead_code)]
const SIM_LIMIT: Address = Address::new([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,3,0xDE,0xAD,0xBE,0xEF]);

// ── Config ──────────────────────────────────────────────────────────────────
#[derive(Debug, Deserialize)]
struct SimConfig {
    rpc: String, pool_address: String, mongo_uri: String, db_name: String,
    fee: u32, tick_spacing: i32,
    genesis_block: u64, from_block: u64, to_block: u64,
    deposit_weth: String,
    token0_decimals: u8, token1_decimals: u8,
    token0_symbol: String, token1_symbol: String,
    wide_range_pct: f64, base_range_pct: f64, limit_order_pct: f64,
    wide_alloc_pct: f64, rebal_wide_alloc_pct: f64,
    rebalance_price_pct: f64, rebalance_interval_blocks: u64,
}
impl SimConfig {
    fn load() -> Self {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("sim_config.toml");
        toml::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
    }
}

sol! {
    #[sol(rpc)]
    contract IUniswapV3Pool {
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked);
        function liquidity() external view returns (uint128);
    }
}

// ── Helpers ─────────────────────────────────────────────────────────────────
fn arg_str<'a>(d: &'a Document, k: &str) -> &'a str { d.get_str(k).unwrap() }
fn arg_u256(d: &Document, k: &str) -> U256 { U256::from_str(arg_str(d, k)).unwrap() }
fn arg_i256(d: &Document, k: &str) -> I256 { I256::from_dec_str(arg_str(d, k)).unwrap() }
fn arg_i32(d: &Document, k: &str) -> i32 { arg_str(d, k).parse().unwrap() }
fn arg_u128(d: &Document, k: &str) -> u128 { arg_str(d, k).parse().unwrap() }
fn arg_u8(d: &Document, k: &str) -> u8 { arg_str(d, k).parse().unwrap() }
fn arg_u16(d: &Document, k: &str) -> u16 { arg_str(d, k).parse().unwrap() }
fn arg_address(d: &Document, k: &str) -> Address { Address::from_str(arg_str(d, k)).unwrap() }

fn sqrt_price_to_human(sp: U256, d0: u8, d1: u8) -> f64 {
    let s: f64 = sp.to_string().parse().unwrap();
    (s / 2f64.powi(96)).powi(2) * 10f64.powi(d0 as i32 - d1 as i32)
}
fn price_to_tick(price: f64, d0: u8, d1: u8, ts: i32) -> i32 {
    let t = (( price / 10f64.powi(d0 as i32 - d1 as i32)).ln() / 1.0001f64.ln()).round() as i32;
    (t / ts) * ts
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
#[allow(dead_code)]
fn liquidity_from_amount0(a: U256, sp: U256, tl: i32, tu: i32, ct: i32) -> u128 {
    let q96 = U256::from(1) << 96;
    let su = tick_math::get_sqrt_ratio_at_tick(tu);
    if ct < tl { let sl = tick_math::get_sqrt_ratio_at_tick(tl); full_math::mul_div(full_math::mul_div(a, sl, q96), su, su - sl).to::<u128>() }
    else if ct < tu { full_math::mul_div(full_math::mul_div(a, sp, q96), su, su - sp).to::<u128>() }
    else { 0 }
}
#[allow(dead_code)]
fn liquidity_from_amount1(a: U256, sp: U256, tl: i32, tu: i32, ct: i32) -> u128 {
    let q96 = U256::from(1) << 96;
    let sl = tick_math::get_sqrt_ratio_at_tick(tl);
    let su = tick_math::get_sqrt_ratio_at_tick(tu);
    if ct >= tu { full_math::mul_div(a, q96, su - sl).to::<u128>() }
    else if ct >= tl { full_math::mul_div(a, q96, sp - sl).to::<u128>() }
    else { 0 }
}

// ── Position tracking ───────────────────────────────────────────────────────
#[derive(Clone, Debug)]
struct SimPosition { owner: Address, tick_lower: i32, tick_upper: i32, liquidity: u128 }

/// Compute pending (unrealized) fees for a position from feeGrowthInside.
/// This is price-independent and monotonically increasing.
fn pending_fees_raw(pool: &UniswapV3Pool, pos: &SimPosition) -> (u128, u128) {
    let liq = pool.positions.get(&(pos.owner, pos.tick_lower, pos.tick_upper)).map_or(0u128, |p| p.liquidity);
    if liq == 0 { return (0, 0); }
    let (fg0, fg1) = tick::get_fee_growth_inside(
        &pool.ticks, pos.tick_lower, pos.tick_upper, pool.slot0.tick,
        pool.fee_growth_global_0_x128, pool.fee_growth_global_1_x128,
    );
    let p = pool.positions.get(&(pos.owner, pos.tick_lower, pos.tick_upper)).unwrap();
    // wrapping_sub is correct here — same range, same position, fee growth only increases
    let d0 = fg0.wrapping_sub(p.fee_growth_inside_0_last_x128);
    let d1 = fg1.wrapping_sub(p.fee_growth_inside_1_last_x128);
    let f0 = full_math::mul_div(d0, U256::from(liq), FIXED_POINT_128_Q128);
    let f1 = full_math::mul_div(d1, U256::from(liq), FIXED_POINT_128_Q128);
    let f0_128 = f0.as_limbs()[0] as u128 | ((f0.as_limbs()[1] as u128) << 64);
    let f1_128 = f1.as_limbs()[0] as u128 | ((f1.as_limbs()[1] as u128) << 64);
    // Also include any already-materialized fees sitting in tokens_owed
    (f0_128 + p.tokens_owed_0, f1_128 + p.tokens_owed_1)
}

fn burn_and_collect(pool: &mut UniswapV3Pool, pos: &SimPosition, ts: u32) -> (u128, u128) {
    let liq = pool.positions.get(&(pos.owner, pos.tick_lower, pos.tick_upper)).map_or(0, |p| p.liquidity);
    if liq == 0 { return (0, 0); }
    pool.burn(pos.owner, pos.tick_lower, pos.tick_upper, liq, ts);
    let p = pool.positions.get(&(pos.owner, pos.tick_lower, pos.tick_upper)).unwrap();
    let (o0, o1) = (p.tokens_owed_0, p.tokens_owed_1);
    pool.collect(pos.owner, pos.tick_lower, pos.tick_upper, u128::MAX, u128::MAX);
    (o0, o1)
}

/// Deploy wide + base + limit into pool (REAL MINT).
fn deploy_positions(
    pool: &mut UniswapV3Pool, total_t0: U256, total_t1: U256,
    cfg: &SimConfig, wide_alloc: f64, dec0: f64, dec1: f64, ts: u32,
) -> (SimPosition, SimPosition, Option<SimPosition>, U256, U256) {
    let price = sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals);
    let ct = pool.slot0.tick; let sp = pool.slot0.sqrt_price_x96;

    let wide_tl = price_to_tick(price * (1.0 - cfg.wide_range_pct / 100.0), cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
    let wide_tu = price_to_tick(price * (1.0 + cfg.wide_range_pct / 100.0), cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
    let base_tl = price_to_tick(price * (1.0 - cfg.base_range_pct / 100.0), cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
    let base_tu = price_to_tick(price * (1.0 + cfg.base_range_pct / 100.0), cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);

    let t0f = total_t0.to_string().parse::<f64>().unwrap();
    let t1f = total_t1.to_string().parse::<f64>().unwrap();
    let total_usd = t0f / dec0 * price + t1f / dec1;

    // USD per unit liquidity for each range
    let usd_per_liq = |tl: i32, tu: i32| -> f64 {
        let l: u128 = 1_000_000_000_000;
        let (a0, a1) = amounts_for_liquidity(l, sp, tl, tu, ct);
        (a0.to_string().parse::<f64>().unwrap() / dec0 * price + a1.to_string().parse::<f64>().unwrap() / dec1) / l as f64
    };

    let wupl = usd_per_liq(wide_tl, wide_tu);
    let bupl = usd_per_liq(base_tl, base_tu);

    let mut wliq = if wupl > 0.0 { (total_usd * wide_alloc / 100.0 / wupl) as u128 } else { 0 };
    let mut bliq = if bupl > 0.0 { (total_usd * (100.0 - wide_alloc) / 100.0 / bupl) as u128 } else { 0 };

    // Scale if exceeding token budget
    let (w0, w1) = amounts_for_liquidity(wliq, sp, wide_tl, wide_tu, ct);
    let (b0, b1) = amounts_for_liquidity(bliq, sp, base_tl, base_tu, ct);
    let need0 = w0 + b0; let need1 = w1 + b1;
    let s0 = if need0 > total_t0 { total_t0.to_string().parse::<f64>().unwrap() / need0.to_string().parse::<f64>().unwrap() } else { 1.0 };
    let s1 = if need1 > total_t1 { total_t1.to_string().parse::<f64>().unwrap() / need1.to_string().parse::<f64>().unwrap() } else { 1.0 };
    let scale = s0.min(s1).min(1.0);
    if scale < 1.0 { wliq = (wliq as f64 * scale) as u128; bliq = (bliq as f64 * scale) as u128; }

    // Cap to maxLiquidityPerTick
    let max_l = pool.max_liquidity_per_tick;
    let cap = |pool: &UniswapV3Pool, tl: i32, tu: i32, l: u128| -> u128 {
        let hl = max_l.saturating_sub(pool.ticks.get(&tl).map_or(0, |t| t.liquidity_gross));
        let hu = max_l.saturating_sub(pool.ticks.get(&tu).map_or(0, |t| t.liquidity_gross));
        l.min(hl).min(hu)
    };

    wliq = cap(pool, wide_tl, wide_tu, wliq);
    if wliq > 0 { pool.mint(SIM_WIDE, wide_tl, wide_tu, wliq, ts); }
    bliq = cap(pool, base_tl, base_tu, bliq);
    if bliq > 0 { pool.mint(SIM_BASE, base_tl, base_tu, bliq, ts); }

    let (w0, w1) = amounts_for_liquidity(wliq, sp, wide_tl, wide_tu, ct);
    let (b0, b1) = amounts_for_liquidity(bliq, sp, base_tl, base_tu, ct);
    let excess_t0 = total_t0.saturating_sub(w0 + b0);
    let excess_t1 = total_t1.saturating_sub(w1 + b1);
    let _ex0_usd = excess_t0.to_string().parse::<f64>().unwrap() / dec0 * price;
    let _ex1_usd = excess_t1.to_string().parse::<f64>().unwrap() / dec1;

    // No limit order for now — excess tokens stay idle
    let limit: Option<SimPosition> = None;
    let final_idle_t0 = excess_t0;
    let final_idle_t1 = excess_t1;

    let wide = SimPosition { owner: SIM_WIDE, tick_lower: wide_tl, tick_upper: wide_tu, liquidity: wliq };
    let base = SimPosition { owner: SIM_BASE, tick_lower: base_tl, tick_upper: base_tu, liquidity: bliq };
    (wide, base, limit, final_idle_t0, final_idle_t1)
}

// ── Phase 1: exact replay with state correction ─────────────────────────────
fn process_event_p1(pool: &mut UniswapV3Pool, doc: &Document, ts: &mut u32, lb: &mut i64) {
    let name = doc.get_str("eventName").unwrap();
    let args = doc.get_document("args").unwrap();
    let bn = doc.get_i64("blockNumber").unwrap_or(0);
    if bn != *lb { *ts += 1; *lb = bn; }
    match name {
        "Initialize" => { pool.initialize(arg_u256(args, "sqrtPriceX96")); }
        "Mint" => { pool.mint(arg_address(args, "owner"), arg_i32(args, "tickLower"), arg_i32(args, "tickUpper"), arg_u128(args, "amount"), *ts); }
        "Burn" => { pool.burn(arg_address(args, "owner"), arg_i32(args, "tickLower"), arg_i32(args, "tickUpper"), arg_u128(args, "amount"), *ts); }
        "Swap" => {
            let a0 = arg_i256(args, "amount0");
            let z = a0 > I256::ZERO;
            let amt = if z { a0 } else { arg_i256(args, "amount1") };
            let lim = if z { tick_math::MIN_SQRT_RATIO + U256::from(1) } else { tick_math::MAX_SQRT_RATIO - U256::from(1) };
            pool.swap(z, amt, lim, *ts);
            pool.slot0.sqrt_price_x96 = arg_u256(args, "sqrtPriceX96");
            pool.slot0.tick = arg_i32(args, "tick");
            pool.liquidity = arg_u128(args, "liquidity");
        }
        "Collect" => { pool.collect(arg_address(args, "owner"), arg_i32(args, "tickLower"), arg_i32(args, "tickUpper"), arg_u128(args, "amount0"), arg_u128(args, "amount1")); }
        "Flash" => { pool.flash_with_paid(arg_u256(args, "paid0"), arg_u256(args, "paid1")); }
        "SetFeeProtocol" => { pool.set_fee_protocol(arg_u8(args, "feeProtocol0New"), arg_u8(args, "feeProtocol1New")); }
        "IncreaseObservationCardinalityNext" => { pool.increase_observation_cardinality_next(arg_u16(args, "observationCardinalityNextNew")); }
        "CollectProtocol" => {
            let (a0, a1) = (arg_u128(args, "amount0"), arg_u128(args, "amount1"));
            if a0 > 0 { pool.protocol_fees.token0 = pool.protocol_fees.token0.saturating_sub(a0); }
            if a1 > 0 { pool.protocol_fees.token1 = pool.protocol_fees.token1.saturating_sub(a1); }
        }
        _ => {}
    }
}

// ── Phase 3: volume-based replay, NO state correction ───────────────────────
// Swaps: same input volume, pool computes own output & price
// Mint/Burn: use LIQUIDITY from event (not token amounts)
// Result: pool self-consistent with our positions minted in
fn process_event_p3(pool: &mut UniswapV3Pool, doc: &Document, ts: &mut u32, lb: &mut i64) -> Option<U256> {
    let name = doc.get_str("eventName").unwrap();
    let args = doc.get_document("args").unwrap();
    let bn = doc.get_i64("blockNumber").unwrap_or(0);
    if bn != *lb { *ts += 1; *lb = bn; }
    match name {
        "Mint" => { pool.mint(arg_address(args, "owner"), arg_i32(args, "tickLower"), arg_i32(args, "tickUpper"), arg_u128(args, "amount"), *ts); }
        "Burn" => { pool.burn(arg_address(args, "owner"), arg_i32(args, "tickLower"), arg_i32(args, "tickUpper"), arg_u128(args, "amount"), *ts); }
        "Swap" => {
            let a0 = arg_i256(args, "amount0");
            let event_sp = arg_u256(args, "sqrtPriceX96");
            let z = a0 > I256::ZERO;
            let amt = if z { a0 } else { arg_i256(args, "amount1") };
            let lim = if z { tick_math::MIN_SQRT_RATIO + U256::from(1) } else { tick_math::MAX_SQRT_RATIO - U256::from(1) };
            // Safety: check swap direction is valid at current pool price
            let pp = pool.slot0.sqrt_price_x96;
            if (z && pp > lim) || (!z && pp < lim) {
                if amt != I256::ZERO { pool.swap(z, amt, lim, *ts); }
            }
            return Some(event_sp);
        }
        "Collect" => { pool.collect(arg_address(args, "owner"), arg_i32(args, "tickLower"), arg_i32(args, "tickUpper"), arg_u128(args, "amount0"), arg_u128(args, "amount1")); }
        "Flash" => { pool.flash_with_paid(arg_u256(args, "paid0"), arg_u256(args, "paid1")); }
        "SetFeeProtocol" => { pool.set_fee_protocol(arg_u8(args, "feeProtocol0New"), arg_u8(args, "feeProtocol1New")); }
        "IncreaseObservationCardinalityNext" => { pool.increase_observation_cardinality_next(arg_u16(args, "observationCardinalityNextNew")); }
        "CollectProtocol" => {
            let (a0, a1) = (arg_u128(args, "amount0"), arg_u128(args, "amount1"));
            if a0 > 0 { pool.protocol_fees.token0 = pool.protocol_fees.token0.saturating_sub(a0); }
            if a1 > 0 { pool.protocol_fees.token1 = pool.protocol_fees.token1.saturating_sub(a1); }
        }
        "Initialize" => {}
        _ => {}
    }
    None
}

// ── Snapshot ─────────────────────────────────────────────────────────────────
#[derive(Clone)]
struct Snapshot { block: u64, cumulative_fees_usd: f64, net_position_value_usd: f64, fee_return_pct: f64 }

// ── Charting ────────────────────────────────────────────────────────────────
fn draw_charts(snapshots: &[Snapshot], from_block: u64, from_ts: u64) {
    if snapshots.len() < 2 { return; }
    let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let bt = 0.25f64;
    let b2t = |b: u64| -> f64 { from_ts as f64 + (b as f64 - from_block as f64) * bt };
    let fd = |t: f64| chrono::DateTime::from_timestamp(t as i64, 0).unwrap().naive_utc().format("%b %d").to_string();
    let fdt = |t: f64| chrono::DateTime::from_timestamp(t as i64, 0).unwrap().naive_utc().format("%b %d %H:%M").to_string();
    let xn = b2t(from_block); let xx = b2t(snapshots.last().unwrap().block);
    let ud = (xx-xn) < 3.0*86400.0;
    let xf = move |v:&f64| if ud { fdt(*v) } else { fd(*v) };
    for (nm, ti, gy, yf) in [
        ("chart_fees.png","Cumulative Fees (USD)",Box::new(|s:&Snapshot|s.cumulative_fees_usd) as Box<dyn Fn(&Snapshot)->f64>,Box::new(|v:&f64|format!("${:.2}",v)) as Box<dyn Fn(&f64)->String>),
        ("chart_position_value.png","Net Position Value",Box::new(|s:&Snapshot|s.net_position_value_usd),Box::new(|v:&f64|format!("${:.0}",v))),
        ("chart_fee_return.png","Fee Return (%)",Box::new(|s:&Snapshot|s.fee_return_pct),Box::new(|v:&f64|format!("{:.2}%",v))),
    ] {
        let p = base.join(nm); let path = p.to_str().unwrap();
        let vs:Vec<f64> = snapshots.iter().map(|s|gy(s)).collect();
        let (yl,yh)=(vs.iter().cloned().fold(f64::INFINITY,f64::min),vs.iter().cloned().fold(f64::NEG_INFINITY,f64::max));
        let m=(yh-yl).abs()*0.1;
        let root = BitMapBackend::new(path,(2400,1000)).into_drawing_area();
        root.fill(&RGBColor(18,18,24)).unwrap();
        let mut c = ChartBuilder::on(&root).caption(ti,("sans-serif",40).into_font().color(&WHITE))
            .margin(40).x_label_area_size(65).y_label_area_size(90)
            .build_cartesian_2d(xn..xx,(yl-m).min(0.0)..(yh+m).max(0.01)).unwrap();
        c.configure_mesh().bold_line_style(RGBColor(40,40,50)).light_line_style(RGBColor(30,30,38))
            .axis_style(RGBColor(120,120,140)).label_style(("sans-serif",18).into_font().color(&RGBColor(180,180,200)))
            .x_label_formatter(&xf).y_label_formatter(&*yf).x_desc("Date").draw().unwrap();
        c.draw_series(AreaSeries::new(snapshots.iter().map(|s|(b2t(s.block),gy(s))),(yl-m).min(0.0),RGBColor(0,220,160).mix(0.2))
            .border_style(ShapeStyle::from(RGBColor(0,220,160)).stroke_width(2))).unwrap();
        root.present().unwrap();
        println!("  Saved: {}",path);
    }
}

// ── Main ────────────────────────────────────────────────────────────────────
#[tokio::main]
async fn main() {
    let cfg = SimConfig::load();
    let deposit_amount_0 = U256::from_str(&cfg.deposit_weth).unwrap();
    let dec0 = 10f64.powi(cfg.token0_decimals as i32);
    let dec1 = 10f64.powi(cfg.token1_decimals as i32);

    println!("═══════════════════════════════════════════════════════");
    println!("  Uniswap V3 LP Strategy Backtester (Real Mint)");
    println!("═══════════════════════════════════════════════════════\n");
    println!("  Pool:            {}/{} ({:.2}%)", cfg.token0_symbol, cfg.token1_symbol, cfg.fee as f64 / 10_000.0);
    println!("  Genesis:         {}", cfg.genesis_block);
    println!("  LP entry:        {}", cfg.from_block);
    println!("  LP exit:         {}", cfg.to_block);
    println!("  Wide: ±{:.0}%  Base: ±{:.0}%  Limit: ±{:.0}%", cfg.wide_range_pct, cfg.base_range_pct, cfg.limit_order_pct);
    println!("  Init alloc:      {:.0}% wide / {:.0}% base", cfg.wide_alloc_pct, 100.0 - cfg.wide_alloc_pct);
    println!("  Rebal alloc:     {:.0}% wide / {:.0}% base", cfg.rebal_wide_alloc_pct, 100.0 - cfg.rebal_wide_alloc_pct);
    println!("  Rebal trigger:   >{:.0}% price OR every {} blocks\n", cfg.rebalance_price_pct, cfg.rebalance_interval_blocks);

    let mongo = MongoClient::with_uri_str(&cfg.mongo_uri).await.unwrap();
    let events_col = mongo.database(&cfg.db_name).collection::<Document>("events");
    let mut pool = UniswapV3Pool::new(cfg.fee, cfg.tick_spacing);
    let mut ts: u32 = 0; let mut lb: i64 = 0;

    // ── Phase 1: exact warmup ───────────────────────────────────────────────
    println!("Phase 1: Warming up (exact replay)...");
    let f1 = doc!{"blockNumber":{"$gte":cfg.genesis_block as i64,"$lt":cfg.from_block as i64}};
    let sort = doc!{"blockNumber":1,"transactionIndex":1,"logIndex":1};
    let n1 = events_col.count_documents(f1.clone()).await.unwrap();
    println!("  {} events\n", n1);
    if n1 > 0 {
        let mut cur = events_col.find(f1).with_options(FindOptions::builder().sort(sort.clone()).build()).await.unwrap();
        let t = Instant::now(); let mut c: u64 = 0;
        while let Some(r) = cur.next().await { process_event_p1(&mut pool, &r.unwrap(), &mut ts, &mut lb); c+=1; if c%500==0||c==n1 { progress::render("warmup",c,n1,"",t); } }
        println!("\n  Done: {} events in {}\n", c, progress::format_duration(t.elapsed().as_millis()));
    }

    // ── Phase 2: deploy strategy (REAL MINT) ────────────────────────────────
    println!("Phase 2: Deploying strategy (real mint)...");
    let entry_sp = pool.slot0.sqrt_price_x96;
    let entry_tick = pool.slot0.tick;
    let entry_price = sqrt_price_to_human(entry_sp, cfg.token0_decimals, cfg.token1_decimals);
    let user_capital_usd = deposit_amount_0.to_string().parse::<f64>().unwrap() / dec0 * entry_price;
    let initial_usdc = U256::from((entry_price * dec1) as u128);
    let borrowed_usdc = initial_usdc.to_string().parse::<f64>().unwrap() / dec1;

    let (mut wide, mut base, mut limit, mut idle_t0, mut idle_t1) = deploy_positions(
        &mut pool, deposit_amount_0, initial_usdc, &cfg, cfg.wide_alloc_pct, dec0, dec1, ts,
    );
    let mut rebalance_count: u32 = 0;
    // Track cumulative fees from feeGrowthInside (price-independent, monotonic)
    let mut cumulative_fees_t0: u128 = 0;
    let mut cumulative_fees_t1: u128 = 0;

    println!("  {} price:       ${:.2}", cfg.token0_symbol, entry_price);
    println!("  User capital:     ${:.2}", user_capital_usd);
    println!("  Borrowed:         {:.2} {}", borrowed_usdc, cfg.token1_symbol);
    println!("  Wide:  [{}, {}] liq={}", wide.tick_lower, wide.tick_upper, wide.liquidity);
    println!("  Base:  [{}, {}] liq={}", base.tick_lower, base.tick_upper, base.liquidity);
    if let Some(ref l) = limit { println!("  Limit: [{}, {}] liq={}", l.tick_lower, l.tick_upper, l.liquidity); }
    println!();

    // ── Phase 3: volume-based replay (NO state correction) ──────────────────
    println!("Phase 3: Replaying (volume-based, real mint)...");
    let f3 = doc!{"blockNumber":{"$gte":cfg.from_block as i64,"$lte":cfg.to_block as i64}};
    let n3 = events_col.count_documents(f3.clone()).await.unwrap();
    println!("  {} events\n", n3);

    let mut snapshots: Vec<Snapshot> = Vec::new();
    let mut last_snap: u64 = 0;
    let mut last_rebal_price = entry_price;
    let mut last_rebal_block = cfg.from_block;
    let mut max_div_bps: f64 = 0.0;
    let mut total_div: f64 = 0.0;
    let mut swap_count: u64 = 0;

    if n3 > 0 {
        let mut cur = events_col.find(f3).with_options(FindOptions::builder().sort(sort.clone()).build()).await.unwrap();
        let t = Instant::now(); let mut c: u64 = 0;

        while let Some(r) = cur.next().await {
            let doc = r.unwrap();
            let bn = doc.get_i64("blockNumber").unwrap_or(0) as u64;
            let event_sp = process_event_p3(&mut pool, &doc, &mut ts, &mut lb);
            c += 1;

            // Track divergence
            if let Some(esp) = event_sp {
                swap_count += 1;
                let pp = pool.slot0.sqrt_price_x96.to_string().parse::<f64>().unwrap();
                let ep = esp.to_string().parse::<f64>().unwrap();
                if ep > 0.0 { let d = ((pp-ep)/ep).abs()*10000.0; max_div_bps = max_div_bps.max(d); total_div += d; }
            }

            // Use on-chain price for rebalance decisions
            let onchain_price = event_sp.map_or(
                sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals),
                |esp| sqrt_price_to_human(esp, cfg.token0_decimals, cfg.token1_decimals),
            );

            let price_move = ((onchain_price - last_rebal_price) / last_rebal_price).abs() * 100.0;
            let blocks_since = bn.saturating_sub(last_rebal_block);

            if (price_move >= cfg.rebalance_price_pct || blocks_since >= cfg.rebalance_interval_blocks) && c > 1 {
                // Capture pending fees from feeGrowthInside BEFORE burning
                let (pf0_w, pf1_w) = pending_fees_raw(&pool, &wide);
                let (pf0_b, pf1_b) = pending_fees_raw(&pool, &base);
                let (pf0_l, pf1_l) = limit.as_ref().map_or((0,0), |l| pending_fees_raw(&pool, l));
                cumulative_fees_t0 += pf0_w + pf0_b + pf0_l;
                cumulative_fees_t1 += pf1_w + pf1_b + pf1_l;

                // Burn all — tokens_owed includes principal + fees
                let (t0w, t1w) = burn_and_collect(&mut pool, &wide, ts);
                let (t0b, t1b) = burn_and_collect(&mut pool, &base, ts);
                let (t0l, t1l) = if let Some(ref l) = limit { burn_and_collect(&mut pool, l, ts) } else { (0,0) };

                let tot_t0 = U256::from(t0w + t0b + t0l) + idle_t0;
                let tot_t1 = U256::from(t1w + t1b + t1l) + idle_t1;

                let (nw, nb, nl, ni0, ni1) = deploy_positions(
                    &mut pool, tot_t0, tot_t1, &cfg, cfg.rebal_wide_alloc_pct, dec0, dec1, ts,
                );
                wide = nw; base = nb; limit = nl; idle_t0 = ni0; idle_t1 = ni1;
                last_rebal_price = onchain_price;
                last_rebal_block = bn;
                rebalance_count += 1;
            }

            // Snapshot every 1000 blocks
            if bn.saturating_sub(last_snap) >= 1000 || c == n3 {
                last_snap = bn;
                let pool_price = sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals);

                // Position value from pool state (accurate with real positions)
                let pos_val = |p: &SimPosition| -> f64 {
                    let liq = pool.positions.get(&(p.owner, p.tick_lower, p.tick_upper)).map_or(0, |pos| pos.liquidity);
                    if liq == 0 { return 0.0; }
                    let (a0, a1) = amounts_for_liquidity(liq, pool.slot0.sqrt_price_x96, p.tick_lower, p.tick_upper, pool.slot0.tick);
                    let owed = pool.positions.get(&(p.owner, p.tick_lower, p.tick_upper)).map_or((0u128,0u128), |p| (p.tokens_owed_0, p.tokens_owed_1));
                    (a0.to_string().parse::<f64>().unwrap() + owed.0 as f64) / dec0 * pool_price
                        + (a1.to_string().parse::<f64>().unwrap() + owed.1 as f64) / dec1
                };

                let total_val = pos_val(&wide) + pos_val(&base) + limit.as_ref().map_or(0.0, |l| pos_val(l))
                    + idle_t0.to_string().parse::<f64>().unwrap() / dec0 * pool_price
                    + idle_t1.to_string().parse::<f64>().unwrap() / dec1;
                let net_val = total_val - borrowed_usdc;

                // Fees = already collected (cumulative) + pending unrealized from feeGrowthInside
                let (pf0_w, pf1_w) = pending_fees_raw(&pool, &wide);
                let (pf0_b, pf1_b) = pending_fees_raw(&pool, &base);
                let (pf0_l, pf1_l) = limit.as_ref().map_or((0,0), |l| pending_fees_raw(&pool, l));
                let total_fees_t0 = cumulative_fees_t0 + pf0_w + pf0_b + pf0_l;
                let total_fees_t1 = cumulative_fees_t1 + pf1_w + pf1_b + pf1_l;
                let fees = total_fees_t0 as f64 / dec0 * pool_price + total_fees_t1 as f64 / dec1;
                let fee_ret = if user_capital_usd > 0.0 { fees / user_capital_usd * 100.0 } else { 0.0 };

                snapshots.push(Snapshot { block: bn, cumulative_fees_usd: fees, net_position_value_usd: net_val, fee_return_pct: fee_ret });
            }

            if c%500==0||c==n3 { progress::render("sim",c,n3,"",t); }
        }
        println!("\n  Done: {} events, {} rebalances, {} snapshots\n", c, rebalance_count, snapshots.len());
    }

    // ── Phase 4: Exit ───────────────────────────────────────────────────────
    let pool_price = sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals);

    // Capture final pending fees before burning
    let (pf0_w, pf1_w) = pending_fees_raw(&pool, &wide);
    let (pf0_b, pf1_b) = pending_fees_raw(&pool, &base);
    let (pf0_l, pf1_l) = limit.as_ref().map_or((0,0), |l| pending_fees_raw(&pool, l));
    cumulative_fees_t0 += pf0_w + pf0_b + pf0_l;
    cumulative_fees_t1 += pf1_w + pf1_b + pf1_l;

    let (t0w, t1w) = burn_and_collect(&mut pool, &wide, ts);
    let (t0b, t1b) = burn_and_collect(&mut pool, &base, ts);
    let (t0l, t1l) = if let Some(ref l) = limit { burn_and_collect(&mut pool, l, ts) } else { (0,0) };
    let total_t0 = t0w + t0b + t0l + idle_t0.to_string().parse::<u128>().unwrap_or(0);
    let total_t1 = t1w + t1b + t1l + idle_t1.to_string().parse::<u128>().unwrap_or(0);
    let total_t0_h = total_t0 as f64 / dec0;
    let total_t1_h = total_t1 as f64 / dec1;

    // Use on-chain exit price for valuation
    let rpc_url: url::Url = cfg.rpc.parse().unwrap();
    let prov = ProviderBuilder::new().connect_http(rpc_url);
    let contract = IUniswapV3Pool::new(cfg.pool_address.parse::<Address>().unwrap(), &prov);

    println!("Verifying entry state...");
    match contract.slot0().block(alloy::eips::BlockId::number(cfg.from_block)).call().await {
        Ok(s) => {
            println!("  sqrtPriceX96: [{}]", if entry_sp == U256::from(s.sqrtPriceX96) { "PASS" } else { "FAIL" });
            println!("  tick:         [{}]", if entry_tick == s.tick.as_i32() { "PASS" } else { "FAIL" });
        }
        Err(e) => println!("  Could not verify: {}", e),
    }

    let exit_price = match contract.slot0().block(alloy::eips::BlockId::number(cfg.to_block)).call().await {
        Ok(s) => sqrt_price_to_human(U256::from(s.sqrtPriceX96), cfg.token0_decimals, cfg.token1_decimals),
        Err(_) => pool_price,
    };

    // Final fees were already captured in the last snapshot's pending_fees_raw
    // (or at the last rebalance). The burn+collect already materialized them.
    // Use cumulative_fees from the snapshot tracking.
    let total_fees = cumulative_fees_t0 as f64 / dec0 * exit_price + cumulative_fees_t1 as f64 / dec1;

    let total_pos_usd = total_t0_h * exit_price + total_t1_h;
    let net_val = total_pos_usd - borrowed_usdc;
    let overall_pnl = net_val - user_capital_usd;
    let hodl = deposit_amount_0.to_string().parse::<f64>().unwrap() / dec0 * exit_price;
    let vs_hodl = net_val - hodl;
    let duration_days = (cfg.to_block - cfg.from_block) as f64 * 0.25 / 86400.0;
    let avg_div = if swap_count > 0 { total_div / swap_count as f64 } else { 0.0 };

    let from_ts: u64 = {
        use alloy::eips::BlockNumberOrTag;
        prov.get_block_by_number(BlockNumberOrTag::Number(cfg.from_block)).await.ok().flatten().map(|b| b.header.timestamp).unwrap_or(0)
    };
    println!("\nGenerating charts...");
    draw_charts(&snapshots, cfg.from_block, from_ts);

    println!("\n═══════════════════════════════════════════════════════");
    println!("  Strategy Backtesting Report (Real Mint)");
    println!("═══════════════════════════════════════════════════════\n");
    println!("  Duration:         {} blocks (~{:.1} days)", cfg.to_block - cfg.from_block, duration_days);
    println!("  Rebalances:       {}", rebalance_count);
    println!("  Swaps processed:  {}\n", swap_count);
    println!("─── Price Divergence ─────────────────────────────────");
    println!("  Pool price:       ${:.2} (simulated)", pool_price);
    println!("  On-chain price:   ${:.2}", exit_price);
    println!("  Avg divergence:   {:.2} bps", avg_div);
    println!("  Max divergence:   {:.2} bps\n", max_div_bps);
    println!("─── Entry ────────────────────────────────────────────");
    println!("  {} price:       ${:.2}", cfg.token0_symbol, entry_price);
    println!("  User capital:     ${:.2}", user_capital_usd);
    println!("  Borrowed:         {:.2} {}\n", borrowed_usdc, cfg.token1_symbol);
    println!("─── Exit ─────────────────────────────────────────────");
    println!("  {} price:       ${:.2} (on-chain)", cfg.token0_symbol, exit_price);
    println!("  Recovered:        {:.6} {} + {:.2} {}", total_t0_h, cfg.token0_symbol, total_t1_h, cfg.token1_symbol);
    println!("  Position total:   ${:.2}", total_pos_usd);
    println!("  Repay borrowed:   -${:.2}", borrowed_usdc);
    println!("  Net to user:      ${:.2}\n", net_val);
    println!("─── PnL (on 1 WETH capital) ────────────────────────");
    println!("  Fees earned:      ${:.2}", total_fees);
    println!("  vs HODL 1 WETH:   {}{:.2}", if vs_hodl >= 0.0 { "+$" } else { "-$" }, vs_hodl.abs());
    println!("  Overall PnL:      {}{:.2} ({:.2}%)", if overall_pnl >= 0.0 { "+$" } else { "-$" }, overall_pnl.abs(), if user_capital_usd > 0.0 { overall_pnl/user_capital_usd*100.0 } else { 0.0 });
    println!("  Fee return:       {:.2}%\n", if user_capital_usd > 0.0 { total_fees/user_capital_usd*100.0 } else { 0.0 });
    println!("═══════════════════════════════════════════════════════");
}
