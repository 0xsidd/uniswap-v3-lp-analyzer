use alloy::primitives::{Address, U256};
use std::collections::HashMap;

use crate::math::full_math;

/// Q128 = 2^128  (FixedPoint128.Q128)
const Q128: U256 = U256::from_limbs([0, 0, 1, 0]); // 1 << 128

/// Position.Info — mirrors the Solidity struct exactly.
#[derive(Clone, Debug, Default)]
pub struct Info {
    /// The amount of liquidity owned by this position.
    pub liquidity: u128,
    /// Fee growth per unit of liquidity as of the last update (token0).
    pub fee_growth_inside_0_last_x128: U256,
    /// Fee growth per unit of liquidity as of the last update (token1).
    pub fee_growth_inside_1_last_x128: U256,
    /// Fees owed to the position owner in token0.
    pub tokens_owed_0: u128,
    /// Fees owed to the position owner in token1.
    pub tokens_owed_1: u128,
}

/// The key type used in the positions mapping.
/// In Solidity this is `keccak256(abi.encodePacked(owner, tickLower, tickUpper))`.
/// We use a tuple instead — the pool module can do the hashing if needed.
pub type PositionKey = (Address, i32, i32);

/// Returns a reference to a position, inserting a default if absent.
/// Mirrors `Position.get`.
pub fn get(
    positions: &mut HashMap<PositionKey, Info>,
    owner: Address,
    tick_lower: i32,
    tick_upper: i32,
) -> &mut Info {
    positions.entry((owner, tick_lower, tick_upper)).or_default()
}

/// Inline equivalent of `LiquidityMath.addDelta(uint128, int128)`.
fn add_delta(x: u128, y: i128) -> u128 {
    if y < 0 {
        let abs = (-y) as u128;
        assert!(abs <= x, "LS");
        x - abs
    } else {
        let result = x + y as u128;
        assert!(result >= x, "LA");
        result
    }
}


/// Credits accumulated fees to a user's position.
///
/// Mirrors `Position.update` exactly:
/// - `liquidity_delta == 0` requires existing liquidity > 0 ("NP").
/// - Fee growth differences use wrapping subtraction (overflow is intentional).
/// - `tokensOwed` accumulation uses wrapping addition (overflow acceptable per Solidity comment).
pub fn update(
    info: &mut Info,
    liquidity_delta: i128,
    fee_growth_inside_0x128: U256,
    fee_growth_inside_1x128: U256,
) {
    let liquidity_before = info.liquidity;

    let liquidity_next = if liquidity_delta == 0 {
        assert!(liquidity_before > 0, "NP");
        liquidity_before
    } else {
        add_delta(liquidity_before, liquidity_delta)
    };

    // Calculate accumulated fees.
    // Fee growth differences wrap intentionally (Solidity unchecked subtraction).
    // Solidity casts the result to uint128 which truncates — we do the same.
    let owed0_full = full_math::mul_div(
        fee_growth_inside_0x128.wrapping_sub(info.fee_growth_inside_0_last_x128),
        U256::from(liquidity_before),
        Q128,
    );
    let tokens_owed_0: u128 = owed0_full.as_limbs()[0] as u128 | ((owed0_full.as_limbs()[1] as u128) << 64);

    let owed1_full = full_math::mul_div(
        fee_growth_inside_1x128.wrapping_sub(info.fee_growth_inside_1_last_x128),
        U256::from(liquidity_before),
        Q128,
    );
    let tokens_owed_1: u128 = owed1_full.as_limbs()[0] as u128 | ((owed1_full.as_limbs()[1] as u128) << 64);

    // Update the position.
    if liquidity_delta != 0 {
        info.liquidity = liquidity_next;
    }
    info.fee_growth_inside_0_last_x128 = fee_growth_inside_0x128;
    info.fee_growth_inside_1_last_x128 = fee_growth_inside_1x128;
    if tokens_owed_0 > 0 || tokens_owed_1 > 0 {
        // Wrapping add — overflow acceptable per Solidity comment.
        info.tokens_owed_0 = info.tokens_owed_0.wrapping_add(tokens_owed_0);
        info.tokens_owed_1 = info.tokens_owed_1.wrapping_add(tokens_owed_1);
    }
}
