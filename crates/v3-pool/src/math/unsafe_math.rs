use alloy::primitives::U256;

/// Returns ceil(x / y)
/// Panics if y == 0
pub fn div_rounding_up(x: U256, y: U256) -> U256 {
    let quotient = x / y;
    let remainder = x % y;
    if remainder > U256::ZERO {
        quotient + U256::from(1)
    } else {
        quotient
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_div_rounding_up() {
        assert_eq!(div_rounding_up(U256::from(10), U256::from(3)), U256::from(4));
        assert_eq!(div_rounding_up(U256::from(9), U256::from(3)), U256::from(3));
        assert_eq!(div_rounding_up(U256::from(0), U256::from(3)), U256::from(0));
    }
}
