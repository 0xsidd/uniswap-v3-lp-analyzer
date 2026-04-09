/// Add a signed liquidity delta to liquidity and panic if it overflows or underflows.
///
/// Equivalent to Solidity: `addDelta(uint128 x, int128 y) -> uint128`
pub fn add_delta(x: u128, y: i128) -> u128 {
    if y < 0 {
        let neg_y = (-y) as u128;
        assert!(neg_y <= x, "LS");
        x - neg_y
    } else {
        let z = x + (y as u128);
        assert!(z >= x, "LA");
        z
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_add_delta_positive() {
        assert_eq!(add_delta(100, 50), 150);
    }

    #[test]
    fn test_add_delta_negative() {
        assert_eq!(add_delta(100, -50), 50);
    }

    #[test]
    #[should_panic(expected = "LS")]
    fn test_add_delta_underflow() {
        add_delta(50, -51);
    }
}
