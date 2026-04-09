use alloy::primitives::{I256, U256};

use super::full_math;
use super::unsafe_math;

/// Q96 = 2^96 = 0x1000000000000000000000000
const Q96: U256 = U256::from_limbs([0, 0x100000000, 0, 0]);
const RESOLUTION: usize = 96;

/// Maximum value of uint160
const MAX_UINT160: U256 = U256::from_limbs([
    0xFFFFFFFFFFFFFFFF,
    0xFFFFFFFFFFFFFFFF,
    0x00000000FFFFFFFF,
    0,
]);

/// Gets the next sqrt price given a delta of token0.
/// Always rounds up.
///
/// Direct port of SqrtPriceMath.getNextSqrtPriceFromAmount0RoundingUp
pub fn get_next_sqrt_price_from_amount0_rounding_up(
    sqrt_px96: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> U256 {
    if amount == U256::ZERO {
        return sqrt_px96;
    }

    let numerator1 = U256::from(liquidity) << RESOLUTION;

    if add {
        // product = amount * sqrtPX96
        let product = amount.wrapping_mul(sqrt_px96);
        // Check overflow: product / amount == sqrtPX96
        if !amount.is_zero() && product / amount == sqrt_px96 {
            let denominator = numerator1 + product;
            if denominator >= numerator1 {
                // Always fits in 160 bits
                return full_math::mul_div_rounding_up(numerator1, sqrt_px96, denominator);
            }
        }
        // Fallback: numerator1 / (numerator1 / sqrtPX96 + amount)
        // divRoundingUp(numerator1, (numerator1 / sqrtPX96).add(amount))
        let div_result = numerator1 / sqrt_px96;
        let sum = checked_add(div_result, amount);
        return unsafe_math::div_rounding_up(numerator1, sum);
    } else {
        // product = amount * sqrtPX96
        let product = amount.wrapping_mul(sqrt_px96);
        // require product / amount == sqrtPX96 && numerator1 > product
        assert!(
            !amount.is_zero() && product / amount == sqrt_px96 && numerator1 > product,
            "sqrt price math: invalid amount0 sub"
        );
        let denominator = numerator1 - product;
        let result = full_math::mul_div_rounding_up(numerator1, sqrt_px96, denominator);
        // Safe-cast to uint160
        assert!(result <= MAX_UINT160, "safe cast to uint160 failed");
        result
    }
}

/// Gets the next sqrt price given a delta of token1.
/// Always rounds down.
///
/// Direct port of SqrtPriceMath.getNextSqrtPriceFromAmount1RoundingDown
pub fn get_next_sqrt_price_from_amount1_rounding_down(
    sqrt_px96: U256,
    liquidity: u128,
    amount: U256,
    add: bool,
) -> U256 {
    if add {
        let quotient = if amount <= MAX_UINT160 {
            (amount << RESOLUTION) / U256::from(liquidity)
        } else {
            full_math::mul_div(amount, Q96, U256::from(liquidity))
        };

        let result = checked_add(sqrt_px96, quotient);
        // toUint160
        assert!(result <= MAX_UINT160, "safe cast to uint160 failed");
        result
    } else {
        let quotient = if amount <= MAX_UINT160 {
            unsafe_math::div_rounding_up(amount << RESOLUTION, U256::from(liquidity))
        } else {
            full_math::mul_div_rounding_up(amount, Q96, U256::from(liquidity))
        };

        assert!(sqrt_px96 > quotient, "sqrt price math: price underflow");
        // Always fits 160 bits
        sqrt_px96 - quotient
    }
}

/// Gets the next sqrt price given an input amount of token0 or token1.
/// Panics if price or liquidity are 0.
pub fn get_next_sqrt_price_from_input(
    sqrt_px96: U256,
    liquidity: u128,
    amount_in: U256,
    zero_for_one: bool,
) -> U256 {
    assert!(sqrt_px96 > U256::ZERO, "sqrt price must be > 0");
    assert!(liquidity > 0, "liquidity must be > 0");

    if zero_for_one {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_px96, liquidity, amount_in, true)
    } else {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_px96, liquidity, amount_in, true)
    }
}

/// Gets the next sqrt price given an output amount of token0 or token1.
/// Panics if price or liquidity are 0.
pub fn get_next_sqrt_price_from_output(
    sqrt_px96: U256,
    liquidity: u128,
    amount_out: U256,
    zero_for_one: bool,
) -> U256 {
    assert!(sqrt_px96 > U256::ZERO, "sqrt price must be > 0");
    assert!(liquidity > 0, "liquidity must be > 0");

    if zero_for_one {
        get_next_sqrt_price_from_amount1_rounding_down(sqrt_px96, liquidity, amount_out, false)
    } else {
        get_next_sqrt_price_from_amount0_rounding_up(sqrt_px96, liquidity, amount_out, false)
    }
}

/// Gets the amount0 delta between two prices (unsigned version).
/// `roundUp` controls rounding direction.
pub fn get_amount0_delta(
    sqrt_ratio_a_x96: U256,
    sqrt_ratio_b_x96: U256,
    liquidity: u128,
    round_up: bool,
) -> U256 {
    let (sqrt_ratio_a_x96, sqrt_ratio_b_x96) = if sqrt_ratio_a_x96 > sqrt_ratio_b_x96 {
        (sqrt_ratio_b_x96, sqrt_ratio_a_x96)
    } else {
        (sqrt_ratio_a_x96, sqrt_ratio_b_x96)
    };

    let numerator1 = U256::from(liquidity) << RESOLUTION;
    let numerator2 = sqrt_ratio_b_x96 - sqrt_ratio_a_x96;

    assert!(sqrt_ratio_a_x96 > U256::ZERO, "sqrtRatioAX96 must be > 0");

    if round_up {
        unsafe_math::div_rounding_up(
            full_math::mul_div_rounding_up(numerator1, numerator2, sqrt_ratio_b_x96),
            sqrt_ratio_a_x96,
        )
    } else {
        full_math::mul_div(numerator1, numerator2, sqrt_ratio_b_x96) / sqrt_ratio_a_x96
    }
}

/// Gets the amount1 delta between two prices (unsigned version).
pub fn get_amount1_delta(
    sqrt_ratio_a_x96: U256,
    sqrt_ratio_b_x96: U256,
    liquidity: u128,
    round_up: bool,
) -> U256 {
    let (sqrt_ratio_a_x96, sqrt_ratio_b_x96) = if sqrt_ratio_a_x96 > sqrt_ratio_b_x96 {
        (sqrt_ratio_b_x96, sqrt_ratio_a_x96)
    } else {
        (sqrt_ratio_a_x96, sqrt_ratio_b_x96)
    };

    let diff = sqrt_ratio_b_x96 - sqrt_ratio_a_x96;

    if round_up {
        full_math::mul_div_rounding_up(U256::from(liquidity), diff, Q96)
    } else {
        full_math::mul_div(U256::from(liquidity), diff, Q96)
    }
}

/// Gets the signed amount0 delta between two prices.
/// If liquidity < 0, rounds down; if >= 0, rounds up.
pub fn get_amount0_delta_signed(
    sqrt_ratio_a_x96: U256,
    sqrt_ratio_b_x96: U256,
    liquidity: i128,
) -> I256 {
    if liquidity < 0 {
        let unsigned =
            get_amount0_delta(sqrt_ratio_a_x96, sqrt_ratio_b_x96, (-liquidity) as u128, false);
        -to_int256(unsigned)
    } else {
        let unsigned =
            get_amount0_delta(sqrt_ratio_a_x96, sqrt_ratio_b_x96, liquidity as u128, true);
        to_int256(unsigned)
    }
}

/// Gets the signed amount1 delta between two prices.
pub fn get_amount1_delta_signed(
    sqrt_ratio_a_x96: U256,
    sqrt_ratio_b_x96: U256,
    liquidity: i128,
) -> I256 {
    if liquidity < 0 {
        let unsigned =
            get_amount1_delta(sqrt_ratio_a_x96, sqrt_ratio_b_x96, (-liquidity) as u128, false);
        -to_int256(unsigned)
    } else {
        let unsigned =
            get_amount1_delta(sqrt_ratio_a_x96, sqrt_ratio_b_x96, liquidity as u128, true);
        to_int256(unsigned)
    }
}

// --- Helper functions ---

/// Checked add for U256 (equivalent to LowGasSafeMath.add)
fn checked_add(x: U256, y: U256) -> U256 {
    let (z, overflow) = x.overflowing_add(y);
    assert!(!overflow, "add overflow");
    z
}

/// SafeCast: uint256 -> int256
fn to_int256(y: U256) -> I256 {
    assert!(
        y < U256::from(1) << 255,
        "safe cast: value doesn't fit in int256"
    );
    I256::try_from(y).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_q96_value() {
        assert_eq!(Q96, U256::from(1) << 96);
    }

    #[test]
    fn test_get_amount0_delta_basic() {
        let sqrt_a = U256::from(1) << 96; // price = 1
        let sqrt_b = U256::from(2) << 96; // price = 4
        let liquidity: u128 = 1_000_000;
        let amount = get_amount0_delta(sqrt_a, sqrt_b, liquidity, true);
        assert!(amount > U256::ZERO);
    }

    #[test]
    fn test_get_amount1_delta_basic() {
        let sqrt_a = U256::from(1) << 96;
        let sqrt_b = U256::from(2) << 96;
        let liquidity: u128 = 1_000_000;
        let amount = get_amount1_delta(sqrt_a, sqrt_b, liquidity, true);
        assert!(amount > U256::ZERO);
    }
}
