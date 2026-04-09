use alloy::primitives::{I256, U256};

use super::full_math;
use super::sqrt_price_math;

/// Computes the result of swapping some amount in, or amount out, given the parameters of the swap.
///
/// Direct port of Uniswap V3 SwapMath.computeSwapStep
///
/// # Arguments
/// * `sqrt_ratio_current_x96` - The current sqrt price of the pool
/// * `sqrt_ratio_target_x96` - The price that cannot be exceeded
/// * `liquidity` - The usable liquidity
/// * `amount_remaining` - How much input or output amount is remaining to be swapped in/out
/// * `fee_pips` - The fee taken from the input amount, in hundredths of a bip (i.e., 1e-6 units)
///
/// # Returns
/// (sqrtRatioNextX96, amountIn, amountOut, feeAmount)
pub fn compute_swap_step(
    sqrt_ratio_current_x96: U256,
    sqrt_ratio_target_x96: U256,
    liquidity: u128,
    amount_remaining: I256,
    fee_pips: u32,
) -> (U256, U256, U256, U256) {
    let zero_for_one = sqrt_ratio_current_x96 >= sqrt_ratio_target_x96;
    let exact_in = amount_remaining >= I256::ZERO;

    let sqrt_ratio_next_x96: U256;
    let mut amount_in: U256;
    let mut amount_out: U256;

    let one_million = U256::from(1_000_000u64);
    let fee_pips_u256 = U256::from(fee_pips);

    if exact_in {
        // uint256(amountRemaining)
        let amount_remaining_u256 = amount_remaining.into_raw();
        let amount_remaining_less_fee =
            full_math::mul_div(amount_remaining_u256, one_million - fee_pips_u256, one_million);

        amount_in = if zero_for_one {
            sqrt_price_math::get_amount0_delta(
                sqrt_ratio_target_x96,
                sqrt_ratio_current_x96,
                liquidity,
                true,
            )
        } else {
            sqrt_price_math::get_amount1_delta(
                sqrt_ratio_current_x96,
                sqrt_ratio_target_x96,
                liquidity,
                true,
            )
        };

        if amount_remaining_less_fee >= amount_in {
            sqrt_ratio_next_x96 = sqrt_ratio_target_x96;
        } else {
            sqrt_ratio_next_x96 = sqrt_price_math::get_next_sqrt_price_from_input(
                sqrt_ratio_current_x96,
                liquidity,
                amount_remaining_less_fee,
                zero_for_one,
            );
        }

        amount_out = U256::ZERO; // will be set below
    } else {
        amount_out = if zero_for_one {
            sqrt_price_math::get_amount1_delta(
                sqrt_ratio_target_x96,
                sqrt_ratio_current_x96,
                liquidity,
                false,
            )
        } else {
            sqrt_price_math::get_amount0_delta(
                sqrt_ratio_current_x96,
                sqrt_ratio_target_x96,
                liquidity,
                false,
            )
        };

        // uint256(-amountRemaining) : negate then convert
        let neg_amount = (-amount_remaining).into_raw();

        if neg_amount >= amount_out {
            sqrt_ratio_next_x96 = sqrt_ratio_target_x96;
        } else {
            sqrt_ratio_next_x96 = sqrt_price_math::get_next_sqrt_price_from_output(
                sqrt_ratio_current_x96,
                liquidity,
                neg_amount,
                zero_for_one,
            );
        }

        amount_in = U256::ZERO; // will be set below
    }

    let max = sqrt_ratio_target_x96 == sqrt_ratio_next_x96;

    // Get the input/output amounts
    if zero_for_one {
        if !(max && exact_in) {
            amount_in = sqrt_price_math::get_amount0_delta(
                sqrt_ratio_next_x96,
                sqrt_ratio_current_x96,
                liquidity,
                true,
            );
        }
        if !(max && !exact_in) {
            amount_out = sqrt_price_math::get_amount1_delta(
                sqrt_ratio_next_x96,
                sqrt_ratio_current_x96,
                liquidity,
                false,
            );
        }
    } else {
        if !(max && exact_in) {
            amount_in = sqrt_price_math::get_amount1_delta(
                sqrt_ratio_current_x96,
                sqrt_ratio_next_x96,
                liquidity,
                true,
            );
        }
        if !(max && !exact_in) {
            amount_out = sqrt_price_math::get_amount0_delta(
                sqrt_ratio_current_x96,
                sqrt_ratio_next_x96,
                liquidity,
                false,
            );
        }
    }

    // Cap the output amount to not exceed the remaining output amount
    if !exact_in {
        let neg_amount = (-amount_remaining).into_raw();
        if amount_out > neg_amount {
            amount_out = neg_amount;
        }
    }

    let fee_amount = if exact_in && sqrt_ratio_next_x96 != sqrt_ratio_target_x96 {
        // Didn't reach the target, so take the remainder of the maximum input as fee
        let amount_remaining_u256 = amount_remaining.into_raw();
        amount_remaining_u256 - amount_in
    } else {
        full_math::mul_div_rounding_up(amount_in, fee_pips_u256, one_million - fee_pips_u256)
    };

    (sqrt_ratio_next_x96, amount_in, amount_out, fee_amount)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_swap_step_exact_in() {
        let sqrt_price = U256::from(1) << 96;
        let sqrt_target = U256::from(2) << 96;
        let liquidity = 1_000_000u128;
        let amount = I256::try_from(1_000_000i64).unwrap();
        let fee = 3000u32; // 0.3%

        let (next_price, amt_in, amt_out, fee_amt) =
            compute_swap_step(sqrt_price, sqrt_target, liquidity, amount, fee);

        assert!(next_price > U256::ZERO);
        assert!(amt_in > U256::ZERO);
        assert!(amt_out > U256::ZERO || amt_in > U256::ZERO);
        assert!(fee_amt >= U256::ZERO);
    }
}
