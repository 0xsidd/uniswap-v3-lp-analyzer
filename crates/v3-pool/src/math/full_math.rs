use alloy::primitives::U256;

/// Calculates floor(a * b / denominator) with full 512-bit precision.
/// Panics if result overflows U256 or denominator == 0.
///
/// This is a direct port of Uniswap V3's FullMath.mulDiv.
pub fn mul_div(a: U256, b: U256, denominator: U256) -> U256 {
    // 512-bit multiply [prod1 prod0] = a * b
    // prod0 = (a * b) mod 2^256  (wrapping mul)
    // prod1 = high 256 bits
    let product = a.widening_mul::<256, 4, 512, 8>(b);

    let prod0 = U256::from_limbs(product.into_limbs()[..4].try_into().unwrap());
    let prod1 = U256::from_limbs(product.into_limbs()[4..].try_into().unwrap());

    // Handle non-overflow cases, 256 by 256 division
    if prod1 == U256::ZERO {
        assert!(denominator > U256::ZERO, "division by zero");
        return prod0 / denominator;
    }

    // Make sure the result is less than 2**256.
    // Also prevents denominator == 0
    assert!(denominator > prod1, "result overflows uint256");

    // 512 by 256 division.
    // Make division exact by subtracting the remainder from [prod1 prod0]
    let remainder = a.mul_mod(b, denominator);

    // Subtract 256 bit number from 512 bit number
    // prod1 -= (remainder > prod0) as borrow
    let (prod0, borrow) = prod0.overflowing_sub(remainder);
    let prod1 = if borrow {
        prod1.wrapping_sub(U256::from(1))
    } else {
        prod1
    };

    // Factor powers of two out of denominator
    // twos = (-denominator) & denominator  (lowest set bit)
    let neg_denom = U256::ZERO.wrapping_sub(denominator);
    let twos = neg_denom & denominator;

    // Divide denominator by power of two
    let denominator = denominator / twos;

    // Divide prod0 by the factors of two
    let prod0 = prod0 / twos;

    // Shift in bits from prod1 into prod0.
    // twos_inv = (0 - twos) / twos + 1 = 2^256 / twos
    let twos_inv = U256::ZERO.wrapping_sub(twos) / twos + U256::from(1);
    let prod0 = prod0 | prod1.wrapping_mul(twos_inv);

    // Invert denominator mod 2**256
    // Newton-Raphson: start with seed correct for 4 bits
    let mut inv = U256::from(3).wrapping_mul(denominator) ^ U256::from(2);

    // Double precision each iteration: 4 -> 8 -> 16 -> 32 -> 64 -> 128 -> 256
    inv = inv.wrapping_mul(U256::from(2).wrapping_sub(denominator.wrapping_mul(inv)));
    inv = inv.wrapping_mul(U256::from(2).wrapping_sub(denominator.wrapping_mul(inv)));
    inv = inv.wrapping_mul(U256::from(2).wrapping_sub(denominator.wrapping_mul(inv)));
    inv = inv.wrapping_mul(U256::from(2).wrapping_sub(denominator.wrapping_mul(inv)));
    inv = inv.wrapping_mul(U256::from(2).wrapping_sub(denominator.wrapping_mul(inv)));
    inv = inv.wrapping_mul(U256::from(2).wrapping_sub(denominator.wrapping_mul(inv)));

    prod0.wrapping_mul(inv)
}

/// Calculates ceil(a * b / denominator) with full precision.
/// Panics if result overflows U256 or denominator == 0.
pub fn mul_div_rounding_up(a: U256, b: U256, denominator: U256) -> U256 {
    let result = mul_div(a, b, denominator);
    if a.mul_mod(b, denominator) > U256::ZERO {
        assert!(result < U256::MAX, "result overflows uint256");
        result + U256::from(1)
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mul_div_simple() {
        assert_eq!(
            mul_div(U256::from(500), U256::from(10), U256::from(50)),
            U256::from(100)
        );
    }

    #[test]
    fn test_mul_div_rounding() {
        // 999 * 1000 / 1000 = 999
        assert_eq!(
            mul_div(U256::from(999), U256::from(1000), U256::from(1000)),
            U256::from(999)
        );
    }

    #[test]
    fn test_mul_div_rounding_up() {
        // 1 * 1 / 3 = 0 floor, 1 ceil
        assert_eq!(
            mul_div_rounding_up(U256::from(1), U256::from(1), U256::from(3)),
            U256::from(1)
        );
    }

    #[test]
    fn test_mul_div_large() {
        // Test with values that would overflow 256 bits in intermediate
        let max = U256::MAX;
        let result = mul_div(max, max, max);
        assert_eq!(result, max);
    }

    #[test]
    #[should_panic]
    fn test_mul_div_zero_denominator() {
        mul_div(U256::from(1), U256::from(1), U256::ZERO);
    }
}
