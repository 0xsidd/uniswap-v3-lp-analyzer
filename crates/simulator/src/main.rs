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
const SIM_LIMIT: Address = Address::new([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,3,0xDE,0xAD,0xBE,0xEF]);
const SIM_LIMIT_B: Address = Address::new([0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,4,0xDE,0xAD,0xBE,0xEF]);

// ── Config ──────────────────────────────────────────────────────────────────
#[derive(Debug, Deserialize)]
struct SimConfig {
    pool_address: String, mongo_uri: String, db_name: String,
    fee: u32, tick_spacing: i32,
    genesis_block: u64, from_block: u64, to_block: u64,
    deposit_weth: String,
    token0_decimals: u8, token1_decimals: u8,
    token0_symbol: String, token1_symbol: String,
    wide_range_pct: f64, base_range_pct: f64, limit_order_pct: f64,
    wide_alloc_pct: f64,
    rebalance_price_pct: f64, rebalance_interval_blocks: u64,
    write_csv: bool,
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
fn liquidity_from_amount0(a: U256, sp: U256, tl: i32, tu: i32, ct: i32) -> u128 {
    let q96 = U256::from(1) << 96;
    let su = tick_math::get_sqrt_ratio_at_tick(tu);
    if ct < tl { let sl = tick_math::get_sqrt_ratio_at_tick(tl); full_math::mul_div(full_math::mul_div(a, sl, q96), su, su - sl).to::<u128>() }
    else if ct < tu { full_math::mul_div(full_math::mul_div(a, sp, q96), su, su - sp).to::<u128>() }
    else { 0 }
}
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
///
/// Formula:
///   W_val = WETH_qty × price,  U_val = USDC_qty
///   Active_Pool = 2 × min(W_val, U_val)
///   Wide  = wide_alloc% × Active_Pool
///   Base  = (100 - wide_alloc)% × Active_Pool
///   Limit = ALL remaining tokens (single-sided, whichever token dominates)
fn deploy_positions(
    pool: &mut UniswapV3Pool, total_t0: U256, total_t1: U256,
    cfg: &SimConfig, wide_alloc: f64, dec0: f64, dec1: f64, ts: u32,
) -> (SimPosition, SimPosition, Option<SimPosition>, Option<SimPosition>) {
    let price = sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals);
    let ct = pool.slot0.tick; let sp = pool.slot0.sqrt_price_x96;

    // Geometric (tick-symmetric) ranges so V3 gives ~50/50 token split
    let wide_factor = 1.0 + cfg.wide_range_pct / 100.0;
    let wide_tl = price_to_tick(price / wide_factor, cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
    let wide_tu = price_to_tick(price * wide_factor, cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
    let base_factor = 1.0 + cfg.base_range_pct / 100.0;
    let base_tl = price_to_tick(price / base_factor, cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
    let base_tu = price_to_tick(price * base_factor, cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);

    let t0f = total_t0.to_string().parse::<f64>().unwrap();
    let t1f = total_t1.to_string().parse::<f64>().unwrap();
    let w_val = t0f / dec0 * price;
    let u_val = t1f / dec1;

    // ── Step 1: Active pool = 2 × min(W_val, U_val) ───────────────────────
    let active_pool = 2.0 * w_val.min(u_val);

    // ── Step 2: Wide + Base sized by total USD value ───────────────────────
    // V3 positions have a fixed token ratio per range — we can't force 50/50.
    // Size each position by its total USD budget, let V3 determine the split.
    let wide_budget = active_pool * wide_alloc / 100.0;
    let base_budget = active_pool * (100.0 - wide_alloc) / 100.0;

    let ref_l: u128 = 1_000_000_000_000;
    let usd_per_liq = |tl: i32, tu: i32| -> f64 {
        let (a0, a1) = amounts_for_liquidity(ref_l, sp, tl, tu, ct);
        (a0.to_string().parse::<f64>().unwrap() / dec0 * price + a1.to_string().parse::<f64>().unwrap() / dec1) / ref_l as f64
    };
    let wupl = usd_per_liq(wide_tl, wide_tu);
    let bupl = usd_per_liq(base_tl, base_tu);

    let mut wliq = if wupl > 0.0 { (wide_budget / wupl) as u128 } else { 0 };
    let mut bliq = if bupl > 0.0 { (base_budget / bupl) as u128 } else { 0 };

    // Safety scale — clamp to total available tokens
    let (w0, w1) = amounts_for_liquidity(wliq, sp, wide_tl, wide_tu, ct);
    let (b0, b1) = amounts_for_liquidity(bliq, sp, base_tl, base_tu, ct);
    let need0 = w0 + b0; let need1 = w1 + b1;
    let s0 = if need0 > total_t0 { t0f / need0.to_string().parse::<f64>().unwrap() } else { 1.0 };
    let s1 = if need1 > total_t1 { t1f / need1.to_string().parse::<f64>().unwrap() } else { 1.0 };
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

    // ── Step 3: Limit = ALL remaining tokens ───────────────────────────────
    // After wide+base, both tokens may have remainders:
    //   - The "excess" token from active_pool imbalance (|W_val - U_val|)
    //   - V3 ratio leakage (positions don't consume tokens in 50:50 value)
    // Deploy everything remaining into a single-sided limit order.
    // Choose direction based on which remaining token has more USD value.
    let (w0, w1) = amounts_for_liquidity(wliq, sp, wide_tl, wide_tu, ct);
    let (b0, b1) = amounts_for_liquidity(bliq, sp, base_tl, base_tu, ct);
    let remaining_t0 = total_t0.saturating_sub(w0 + b0);
    let remaining_t1 = total_t1.saturating_sub(w1 + b1);
    let rem0_val = remaining_t0.to_string().parse::<f64>().unwrap() / dec0 * price;
    let rem1_val = remaining_t1.to_string().parse::<f64>().unwrap() / dec1;

    let mut limit: Option<SimPosition> = None;
    let mut limit_b: Option<SimPosition> = None;
    let limit_pct = cfg.limit_order_pct / 100.0;
    let ts_i = cfg.tick_spacing;
    let aligned = if ct >= 0 { (ct / ts_i) * ts_i } else { ((ct - ts_i + 1) / ts_i) * ts_i };
    let ct_above = if aligned <= ct { aligned + ts_i } else { aligned };
    let ct_below = if aligned >= ct { aligned - ts_i } else { aligned };

    // Limit A: excess token (single-sided)
    // Limit B: non-excess remainder (single-sided, opposite direction)
    let above_tl = price_to_tick(price, cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing).max(ct_above);
    let above_tu = price_to_tick(price * (1.0 + limit_pct), cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing).max(above_tl + ts_i);
    let below_tl = price_to_tick(price / (1.0 + limit_pct), cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing);
    let below_tu = price_to_tick(price, cfg.token0_decimals, cfg.token1_decimals, cfg.tick_spacing).min(ct_below);

    // Deploy WETH remainder above price (token0-only)
    let mint_above = |pool: &mut UniswapV3Pool, amt: U256, owner: Address| -> Option<SimPosition> {
        if amt == U256::ZERO || above_tl >= above_tu { return None; }
        let mut liq = liquidity_from_amount0(amt, sp, above_tl, above_tu, ct);
        liq = cap(pool, above_tl, above_tu, liq);
        if liq > 0 { pool.mint(owner, above_tl, above_tu, liq, ts);
            Some(SimPosition { owner, tick_lower: above_tl, tick_upper: above_tu, liquidity: liq })
        } else { None }
    };
    // Deploy USDC remainder below price (token1-only)
    let mint_below = |pool: &mut UniswapV3Pool, amt: U256, owner: Address| -> Option<SimPosition> {
        if amt == U256::ZERO || below_tl >= below_tu { return None; }
        let mut liq = liquidity_from_amount1(amt, sp, below_tl, below_tu, ct);
        liq = cap(pool, below_tl, below_tu, liq);
        if liq > 0 { pool.mint(owner, below_tl, below_tu, liq, ts);
            Some(SimPosition { owner, tick_lower: below_tl, tick_upper: below_tu, liquidity: liq })
        } else { None }
    };

    if rem0_val >= rem1_val {
        // More excess in WETH → primary limit above, secondary limit below for leftover USDC
        limit = mint_above(&mut *pool, remaining_t0, SIM_LIMIT);
        limit_b = mint_below(&mut *pool, remaining_t1, SIM_LIMIT_B);
    } else {
        // More excess in USDC → primary limit below, secondary limit above for leftover WETH
        limit = mint_below(&mut *pool, remaining_t1, SIM_LIMIT);
        limit_b = mint_above(&mut *pool, remaining_t0, SIM_LIMIT_B);
    }

    let wide = SimPosition { owner: SIM_WIDE, tick_lower: wide_tl, tick_upper: wide_tu, liquidity: wliq };
    let base = SimPosition { owner: SIM_BASE, tick_lower: base_tl, tick_upper: base_tu, liquidity: bliq };
    (wide, base, limit, limit_b)
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
            // Drive pool to the event's ending price, not by replaying input volume.
            // Our pool has extra liquidity (our LP positions), so the same input moves
            // price less. Instead, we use a large input and let the price limit stop
            // the swap at exactly the on-chain ending price. This ensures:
            //   1. Zero price divergence (pool always matches on-chain)
            //   2. Correct fee distribution (volume through each tick reflects real prices)
            //   3. Our positions earn their proportional share at each tick
            let lim = event_sp;
            let pp = pool.slot0.sqrt_price_x96;
            if (z && pp > lim) || (!z && pp < lim) {
                let max_amt = I256::try_from(U256::from(u128::MAX >> 1)).unwrap();
                pool.swap(z, max_amt, lim, *ts);
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
#[allow(dead_code)]
struct Snapshot {
    block: u64, entry_price: f64, current_price: f64,
    cumulative_fees_usd: f64, net_position_value_usd: f64, fee_return_pct: f64,
    wide_t0: f64, wide_t1: f64, wide_val: f64,
    base_t0: f64, base_t1: f64, base_val: f64,
    limit_t0: f64, limit_t1: f64, limit_val: f64,
    total_t0: f64, total_t1: f64, total_val: f64,
    rebalance_count: u32,
}

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
    dotenvy::dotenv().ok();
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
    println!("  Alloc:           {:.0}% wide / {:.0}% base", cfg.wide_alloc_pct, 100.0 - cfg.wide_alloc_pct);
    println!("  Rebal trigger:   >{:.0}% price OR limit>25% OR ({}blk AND limit>10%)\n", cfg.rebalance_price_pct, cfg.rebalance_interval_blocks);

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

    let (mut wide, mut base, mut limit, mut limit_b) = deploy_positions(
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
    // Live display state
    let (mut live_wt0, mut live_wt1) = (0.0f64, 0.0f64);
    let (mut live_bt0, mut live_bt1) = (0.0, 0.0);
    let (mut live_lt0, mut live_lt1) = (0.0, 0.0);
    let (mut live_tot0, mut live_tot1) = (0.0, 0.0);
    let mut live_price = entry_price;
    let mut live_fees = 0.0f64;
    let mut dash_lines: u16 = 0;
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

            // Use POOL price for rebalance decisions (our positions sit in the pool)
            let pool_price_now = sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals);

            let price_move = ((pool_price_now - last_rebal_price) / last_rebal_price).abs() * 100.0;
            let blocks_since = bn.saturating_sub(last_rebal_block);

            // Compute limit % of portfolio for rebalance trigger
            let limit_pct_of_tv = {
                let pos_val = |pos: &Option<SimPosition>| -> f64 {
                    pos.as_ref().map_or(0.0, |p| {
                        let l = pool.positions.get(&(p.owner, p.tick_lower, p.tick_upper)).map_or(0, |pp| pp.liquidity);
                        if l == 0 { return 0.0; }
                        let (a0, a1) = amounts_for_liquidity(l, pool.slot0.sqrt_price_x96, p.tick_lower, p.tick_upper, pool.slot0.tick);
                        a0.to_string().parse::<f64>().unwrap() / dec0 * pool_price_now + a1.to_string().parse::<f64>().unwrap() / dec1
                    })
                };
                let lv = pos_val(&limit) + pos_val(&limit_b);
                let w_liq = pool.positions.get(&(wide.owner, wide.tick_lower, wide.tick_upper)).map_or(0, |p| p.liquidity);
                let b_liq = pool.positions.get(&(base.owner, base.tick_lower, base.tick_upper)).map_or(0, |p| p.liquidity);
                let (wa0, wa1) = amounts_for_liquidity(w_liq, pool.slot0.sqrt_price_x96, wide.tick_lower, wide.tick_upper, pool.slot0.tick);
                let (ba0, ba1) = amounts_for_liquidity(b_liq, pool.slot0.sqrt_price_x96, base.tick_lower, base.tick_upper, pool.slot0.tick);
                let wbv = (wa0+ba0).to_string().parse::<f64>().unwrap() / dec0 * pool_price_now
                       + (wa1+ba1).to_string().parse::<f64>().unwrap() / dec1;
                let tv = wbv + lv;
                if tv > 0.0 { lv / tv * 100.0 } else { 0.0 }
            };

            // Rebalance triggers:
            //   1. Price moved ±X% from last rebalance
            //   2. Limit > 25% of portfolio AND price moved ≥5% (avoid churning when structurally imbalanced)
            //   3. Every N blocks unconditionally
            let should_rebalance = price_move >= cfg.rebalance_price_pct
                || (limit_pct_of_tv >= 25.0 && price_move >= 5.0)
                || blocks_since >= cfg.rebalance_interval_blocks;

            if should_rebalance && c > 1 && blocks_since > 0 {
                // Capture pending fees from feeGrowthInside BEFORE burning
                let (pf0_w, pf1_w) = pending_fees_raw(&pool, &wide);
                let (pf0_b, pf1_b) = pending_fees_raw(&pool, &base);
                let (pf0_l, pf1_l) = limit.as_ref().map_or((0,0), |l| pending_fees_raw(&pool, l));
                let (pf0_lb, pf1_lb) = limit_b.as_ref().map_or((0,0), |l| pending_fees_raw(&pool, l));
                cumulative_fees_t0 += pf0_w + pf0_b + pf0_l + pf0_lb;
                cumulative_fees_t1 += pf1_w + pf1_b + pf1_l + pf1_lb;

                // Burn all — tokens_owed includes principal + fees
                let (t0w, t1w) = burn_and_collect(&mut pool, &wide, ts);
                let (t0b, t1b) = burn_and_collect(&mut pool, &base, ts);
                let (t0l, t1l) = if let Some(ref l) = limit { burn_and_collect(&mut pool, l, ts) } else { (0,0) };
                let (t0lb, t1lb) = if let Some(ref l) = limit_b { burn_and_collect(&mut pool, l, ts) } else { (0,0) };

                let tot_t0 = U256::from(t0w + t0b + t0l + t0lb);
                let tot_t1 = U256::from(t1w + t1b + t1l + t1lb);
                let rv = tot_t0.to_string().parse::<f64>().unwrap() / dec0 * pool_price_now
                       + tot_t1.to_string().parse::<f64>().unwrap() / dec1;
                eprintln!("REBAL #{} blk={} price={:.2} val=${:.2} t0={:.6} t1={:.2} pm={:.1}% lim%={:.1}% bs={}",
                    rebalance_count+1, bn, pool_price_now, rv,
                    tot_t0.to_string().parse::<f64>().unwrap() / dec0,
                    tot_t1.to_string().parse::<f64>().unwrap() / dec1,
                    price_move, limit_pct_of_tv, blocks_since);

                let (nw, nb, nl, nlb) = deploy_positions(
                    &mut pool, tot_t0, tot_t1, &cfg, cfg.wide_alloc_pct, dec0, dec1, ts,
                );
                wide = nw; base = nb; limit = nl; limit_b = nlb;
                last_rebal_price = pool_price_now;
                last_rebal_block = bn;
                rebalance_count += 1;
            }

            // Snapshot every 1000 blocks
            if bn.saturating_sub(last_snap) >= 1000 || c == n3 {
                last_snap = bn;
                let pool_price = sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals);

                // Per-position token amounts
                let pos_tokens = |p: &SimPosition| -> (f64, f64) {
                    let liq = pool.positions.get(&(p.owner, p.tick_lower, p.tick_upper)).map_or(0, |pos| pos.liquidity);
                    if liq == 0 { return (0.0, 0.0); }
                    let (a0, a1) = amounts_for_liquidity(liq, pool.slot0.sqrt_price_x96, p.tick_lower, p.tick_upper, pool.slot0.tick);
                    let owed = pool.positions.get(&(p.owner, p.tick_lower, p.tick_upper)).map_or((0u128,0u128), |p| (p.tokens_owed_0, p.tokens_owed_1));
                    (
                        (a0.to_string().parse::<f64>().unwrap() + owed.0 as f64) / dec0,
                        (a1.to_string().parse::<f64>().unwrap() + owed.1 as f64) / dec1,
                    )
                };

                let (wt0, wt1) = pos_tokens(&wide);
                let (bt0, bt1) = pos_tokens(&base);
                let (lt0, lt1) = limit.as_ref().map_or((0.0, 0.0), |l| pos_tokens(l));
                let (lbt0, lbt1) = limit_b.as_ref().map_or((0.0, 0.0), |l| pos_tokens(l));
                let (lt0, lt1) = (lt0 + lbt0, lt1 + lbt1);
                let tot0 = wt0 + bt0 + lt0;
                let tot1 = wt1 + bt1 + lt1;

                let total_val = tot0 * pool_price + tot1;
                let net_val = total_val - borrowed_usdc;

                let (pf0_w, pf1_w) = pending_fees_raw(&pool, &wide);
                let (pf0_b, pf1_b) = pending_fees_raw(&pool, &base);
                let (pf0_l, pf1_l) = limit.as_ref().map_or((0,0), |l| pending_fees_raw(&pool, l));
                let (pf0_lb, pf1_lb) = limit_b.as_ref().map_or((0,0), |l| pending_fees_raw(&pool, l));
                let total_fees_t0 = cumulative_fees_t0 + pf0_w + pf0_b + pf0_l + pf0_lb;
                let total_fees_t1 = cumulative_fees_t1 + pf1_w + pf1_b + pf1_l + pf1_lb;
                let fees = total_fees_t0 as f64 / dec0 * pool_price + total_fees_t1 as f64 / dec1;
                let fee_ret = if user_capital_usd > 0.0 { fees / user_capital_usd * 100.0 } else { 0.0 };

                snapshots.push(Snapshot {
                    block: bn, entry_price, current_price: pool_price,
                    cumulative_fees_usd: fees, net_position_value_usd: net_val, fee_return_pct: fee_ret,
                    wide_t0: wt0, wide_t1: wt1, wide_val: wt0 * pool_price + wt1,
                    base_t0: bt0, base_t1: bt1, base_val: bt0 * pool_price + bt1,
                    limit_t0: lt0, limit_t1: lt1, limit_val: lt0 * pool_price + lt1,
                    total_t0: tot0, total_t1: tot1, total_val: total_val,
                    rebalance_count,
                });

                // Store for live display
                live_wt0 = wt0; live_wt1 = wt1;
                live_bt0 = bt0; live_bt1 = bt1;
                live_lt0 = lt0; live_lt1 = lt1;
                live_tot0 = tot0; live_tot1 = tot1;
                live_price = pool_price;
                live_fees = fees;
            }

            // Live dashboard every 500 events
            if c%500==0||c==n3 {
                use std::io::Write;
                if dash_lines > 0 { print!("\x1b[{}A", dash_lines); }
                let mut lines: u16 = 0;

                let pct = c as f64 / n3 as f64;
                let bw = 30usize;
                let filled = (pct * bw as f64).round() as usize;
                let elapsed_ms = t.elapsed().as_millis();
                let eta = if pct > 0.001 { progress::format_duration(((elapsed_ms as f64 / pct) * (1.0 - pct)).round() as u128) } else { "...".into() };

                print!("\x1b[2K  {}{} {:.1}%  {}/{}  elapsed: {}  ETA: {}\n",
                    "\u{2588}".repeat(filled), "\u{2591}".repeat(bw - filled),
                    pct * 100.0, c, n3, progress::format_duration(elapsed_ms), eta);
                lines += 1;

                print!("\x1b[2K  \x1b[33mentry ${:.0}\x1b[0m \u{2192} \x1b[36mnow ${:.0}\x1b[0m  |  fees \x1b[32m${:.2}\x1b[0m  |  rebal: {}\n",
                    entry_price, live_price, live_fees, rebalance_count);
                lines += 1;

                // Bar helper
                let bar = |val: f64, max: f64, w: usize, color: &str| -> String {
                    let p = if max > 0.0 { (val/max).min(1.0) } else { 0.0 };
                    let f = (p * w as f64).round() as usize;
                    format!("{}{}\x1b[0m{}", color, "\u{2588}".repeat(f), " ".repeat(w.saturating_sub(f)))
                };

                let p = live_price;
                let wide_usd  = live_wt0 * p + live_wt1;
                let base_usd  = live_bt0 * p + live_bt1;
                let limit_usd = live_lt0 * p + live_lt1;
                let total_usd = live_tot0 * p + live_tot1;
                let max_usd = wide_usd.max(base_usd).max(limit_usd).max(0.01);
                let bw = 16;

                print!("\x1b[2K  \x1b[1mTotal  ${:<10.2}\x1b[0m  \x1b[36m${:.0}({})\x1b[0m + \x1b[32m${:.0}({})\x1b[0m\n",
                    total_usd, live_tot0 * p, cfg.token0_symbol, live_tot1, cfg.token1_symbol);
                lines += 1;

                print!("\x1b[2K  Wide   {} \x1b[33m${:<10.2}\x1b[0m  \x1b[36m${:.0}({})\x1b[0m + \x1b[32m${:.0}({})\x1b[0m\n",
                    bar(wide_usd, max_usd, bw, "\x1b[33m"), wide_usd, live_wt0 * p, cfg.token0_symbol, live_wt1, cfg.token1_symbol);
                lines += 1;

                print!("\x1b[2K  Base   {} \x1b[33m${:<10.2}\x1b[0m  \x1b[36m${:.0}({})\x1b[0m + \x1b[32m${:.0}({})\x1b[0m\n",
                    bar(base_usd, max_usd, bw, "\x1b[33m"), base_usd, live_bt0 * p, cfg.token0_symbol, live_bt1, cfg.token1_symbol);
                lines += 1;

                print!("\x1b[2K  Limit  {} \x1b[33m${:<10.2}\x1b[0m  \x1b[36m${:.0}({})\x1b[0m + \x1b[32m${:.0}({})\x1b[0m\n",
                    bar(limit_usd, max_usd, bw, "\x1b[33m"), limit_usd, live_lt0 * p, cfg.token0_symbol, live_lt1, cfg.token1_symbol);
                lines += 1;


                std::io::stdout().flush().ok();
                dash_lines = lines;
            }
        }
        // Clear dashboard
        if dash_lines > 0 {
            print!("\x1b[{}A", dash_lines);
            for _ in 0..dash_lines { print!("\x1b[2K\n"); }
            print!("\x1b[{}A", dash_lines);
        }
        println!("  Done: {} events, {} rebalances, {} snapshots\n", c, rebalance_count, snapshots.len());
    }

    // ── Phase 4: Exit ───────────────────────────────────────────────────────
    let pool_price = sqrt_price_to_human(pool.slot0.sqrt_price_x96, cfg.token0_decimals, cfg.token1_decimals);

    // Capture final pending fees before burning
    let (pf0_w, pf1_w) = pending_fees_raw(&pool, &wide);
    let (pf0_b, pf1_b) = pending_fees_raw(&pool, &base);
    let (pf0_l, pf1_l) = limit.as_ref().map_or((0,0), |l| pending_fees_raw(&pool, l));
    let (pf0_lb, pf1_lb) = limit_b.as_ref().map_or((0,0), |l| pending_fees_raw(&pool, l));
    cumulative_fees_t0 += pf0_w + pf0_b + pf0_l + pf0_lb;
    cumulative_fees_t1 += pf1_w + pf1_b + pf1_l + pf1_lb;

    let (t0w, t1w) = burn_and_collect(&mut pool, &wide, ts);
    let (t0b, t1b) = burn_and_collect(&mut pool, &base, ts);
    let (t0l, t1l) = if let Some(ref l) = limit { burn_and_collect(&mut pool, l, ts) } else { (0,0) };
    let (t0lb, t1lb) = if let Some(ref l) = limit_b { burn_and_collect(&mut pool, l, ts) } else { (0,0) };
    let total_t0 = t0w + t0b + t0l + t0lb;
    let total_t1 = t1w + t1b + t1l + t1lb;
    let total_t0_h = total_t0 as f64 / dec0;
    let total_t1_h = total_t1 as f64 / dec1;

    // Use on-chain exit price for valuation
    let rpc_url: url::Url = std::env::var("RPC_URL").expect("RPC_URL not set in .env").parse().unwrap();
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

    // ── Write CSV ──────────────────────────────────────────────────────────
    if cfg.write_csv {
        let csv_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("simulation_data.csv");
        let mut wtr = std::fs::File::create(&csv_path).expect("failed to create CSV");
        use std::io::Write as IoWrite;
        writeln!(wtr, "price,wide_weth_val,wide_usdc_val,base_weth_val,base_usdc_val,limit_weth_val,limit_usdc_val,rebalance_count").unwrap();
        for s in &snapshots {
            writeln!(wtr, "{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{:.2},{}",
                s.current_price,
                s.wide_t0 * s.current_price, s.wide_t1,
                s.base_t0 * s.current_price, s.base_t1,
                s.limit_t0 * s.current_price, s.limit_t1,
                s.rebalance_count,
            ).unwrap();
        }
        println!("  Saved: {}", csv_path.display());
    }

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
