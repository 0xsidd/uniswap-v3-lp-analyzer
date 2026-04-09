use alloy::primitives::U256;
use std::collections::HashMap;

/// Tick.Info — mirrors the Solidity struct exactly.
#[derive(Clone, Debug, Default)]
pub struct Info {
    /// Total position liquidity referencing this tick.
    pub liquidity_gross: u128,
    /// Net liquidity added (subtracted) when tick crossed left-to-right (right-to-left).
    pub liquidity_net: i128,
    /// Fee growth per unit of liquidity on the *other* side of this tick (token0).
    pub fee_growth_outside_0x128: U256,
    /// Fee growth per unit of liquidity on the *other* side of this tick (token1).
    pub fee_growth_outside_1x128: U256,
    /// Cumulative tick value on the other side of this tick.
    pub tick_cumulative_outside: i64, // i56 in Solidity, i64 is the smallest Rust signed type that fits
    /// Seconds per unit of liquidity on the other side of this tick.
    pub seconds_per_liquidity_outside_x128: U256, // uint160 in Solidity, stored in U256
    /// Seconds spent on the other side of this tick.
    pub seconds_outside: u32,
    /// True iff the tick is initialized (liquidityGross != 0).
    pub initialized: bool,
}

// ── Constants ────────────────────────────────────────────────────────────────

/// TickMath.MIN_TICK
pub const MIN_TICK: i32 = -887272;
/// TickMath.MAX_TICK
pub const MAX_TICK: i32 = 887272;

// ── Free functions operating on HashMap<i32, Info> ───────────────────────────

/// Derives max liquidity per tick from given tick spacing.
/// Mirrors `Tick.tickSpacingToMaxLiquidityPerTick`.
pub fn tick_spacing_to_max_liquidity_per_tick(tick_spacing: i32) -> u128 {
    let min_tick = (MIN_TICK / tick_spacing) * tick_spacing;
    let max_tick = (MAX_TICK / tick_spacing) * tick_spacing;
    let num_ticks = ((max_tick - min_tick) / tick_spacing + 1) as u32;
    u128::MAX / num_ticks as u128
}

/// Retrieves fee growth data inside a position's tick boundaries.
///
/// All subtractions use wrapping arithmetic (Solidity unchecked uint256 overflow).
pub fn get_fee_growth_inside(
    ticks: &HashMap<i32, Info>,
    tick_lower: i32,
    tick_upper: i32,
    tick_current: i32,
    fee_growth_global_0x128: U256,
    fee_growth_global_1x128: U256,
) -> (U256, U256) {
    let default = Info::default();
    let lower = ticks.get(&tick_lower).unwrap_or(&default);
    let upper = ticks.get(&tick_upper).unwrap_or(&default);

    // fee growth below
    let (fee_growth_below_0, fee_growth_below_1) = if tick_current >= tick_lower {
        (lower.fee_growth_outside_0x128, lower.fee_growth_outside_1x128)
    } else {
        (
            fee_growth_global_0x128.wrapping_sub(lower.fee_growth_outside_0x128),
            fee_growth_global_1x128.wrapping_sub(lower.fee_growth_outside_1x128),
        )
    };

    // fee growth above
    let (fee_growth_above_0, fee_growth_above_1) = if tick_current < tick_upper {
        (upper.fee_growth_outside_0x128, upper.fee_growth_outside_1x128)
    } else {
        (
            fee_growth_global_0x128.wrapping_sub(upper.fee_growth_outside_0x128),
            fee_growth_global_1x128.wrapping_sub(upper.fee_growth_outside_1x128),
        )
    };

    (
        fee_growth_global_0x128
            .wrapping_sub(fee_growth_below_0)
            .wrapping_sub(fee_growth_above_0),
        fee_growth_global_1x128
            .wrapping_sub(fee_growth_below_1)
            .wrapping_sub(fee_growth_above_1),
    )
}

/// Inline equivalent of `LiquidityMath.addDelta(uint128, int128)`.
/// Panics on overflow / underflow (mirrors Solidity `require`).
fn add_delta(x: u128, y: i128) -> u128 {
    if y < 0 {
        let abs = (-y) as u128;
        assert!(abs <= x, "LS"); // liquidity sub underflow
        x - abs
    } else {
        let result = x + y as u128;
        assert!(result >= x, "LA"); // liquidity add overflow
        result
    }
}

/// Updates a tick and returns `true` if the tick was flipped (initialized ↔ uninitialized).
///
/// Mirrors `Tick.update` exactly.
#[allow(clippy::too_many_arguments)]
pub fn update(
    ticks: &mut HashMap<i32, Info>,
    tick: i32,
    tick_current: i32,
    liquidity_delta: i128,
    fee_growth_global_0x128: U256,
    fee_growth_global_1x128: U256,
    seconds_per_liquidity_cumulative_x128: U256,
    tick_cumulative: i64,
    time: u32,
    upper: bool,
    max_liquidity: u128,
) -> bool {
    let info = ticks.entry(tick).or_default();

    let liquidity_gross_before = info.liquidity_gross;
    let liquidity_gross_after = add_delta(liquidity_gross_before, liquidity_delta);

    assert!(liquidity_gross_after <= max_liquidity, "LO");

    let flipped = (liquidity_gross_after == 0) != (liquidity_gross_before == 0);

    if liquidity_gross_before == 0 {
        // Initialize: assume all growth before this tick happened _below_ the tick.
        if tick <= tick_current {
            info.fee_growth_outside_0x128 = fee_growth_global_0x128;
            info.fee_growth_outside_1x128 = fee_growth_global_1x128;
            info.seconds_per_liquidity_outside_x128 = seconds_per_liquidity_cumulative_x128;
            info.tick_cumulative_outside = tick_cumulative;
            info.seconds_outside = time;
        }
        info.initialized = true;
    }

    info.liquidity_gross = liquidity_gross_after;

    // Solidity: upper ? int256(liquidityNet).sub(liquidityDelta).toInt128()
    //                  : int256(liquidityNet).add(liquidityDelta).toInt128()
    // LowGasSafeMath checked add/sub on int256 then SafeCast to int128.
    if upper {
        let result = (info.liquidity_net as i128)
            .checked_sub(liquidity_delta)
            .expect("liquidity_net sub overflow");
        info.liquidity_net = result;
    } else {
        let result = (info.liquidity_net as i128)
            .checked_add(liquidity_delta)
            .expect("liquidity_net add overflow");
        info.liquidity_net = result;
    }

    flipped
}

/// Clears tick data. Mirrors `Tick.clear`.
pub fn clear(ticks: &mut HashMap<i32, Info>, tick: i32) {
    ticks.remove(&tick);
}

/// Transitions to the next tick as needed by price movement.
///
/// All subtractions use wrapping arithmetic (mirrors Solidity unchecked behaviour on
/// `feeGrowthOutside`, `secondsPerLiquidityOutside`, `tickCumulativeOutside`, `secondsOutside`).
///
/// Returns `liquidityNet` for this tick.
pub fn cross(
    ticks: &mut HashMap<i32, Info>,
    tick: i32,
    fee_growth_global_0x128: U256,
    fee_growth_global_1x128: U256,
    seconds_per_liquidity_cumulative_x128: U256,
    tick_cumulative: i64,
    time: u32,
) -> i128 {
    let info = ticks.entry(tick).or_default();
    info.fee_growth_outside_0x128 =
        fee_growth_global_0x128.wrapping_sub(info.fee_growth_outside_0x128);
    info.fee_growth_outside_1x128 =
        fee_growth_global_1x128.wrapping_sub(info.fee_growth_outside_1x128);
    info.seconds_per_liquidity_outside_x128 =
        seconds_per_liquidity_cumulative_x128.wrapping_sub(info.seconds_per_liquidity_outside_x128);
    info.tick_cumulative_outside = tick_cumulative.wrapping_sub(info.tick_cumulative_outside);
    info.seconds_outside = time.wrapping_sub(info.seconds_outside);
    info.liquidity_net
}
