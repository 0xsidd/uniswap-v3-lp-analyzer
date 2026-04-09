use alloy::primitives::{I256, U256};

/// The minimum tick that may be passed to `get_sqrt_ratio_at_tick`
pub const MIN_TICK: i32 = -887272;
/// The maximum tick that may be passed to `get_sqrt_ratio_at_tick`
pub const MAX_TICK: i32 = 887272;

/// The minimum value that can be returned from `get_sqrt_ratio_at_tick`.
/// Equivalent to `get_sqrt_ratio_at_tick(MIN_TICK)`
pub const MIN_SQRT_RATIO: U256 = U256::from_limbs([4295128739, 0, 0, 0]);

/// The maximum value that can be returned from `get_sqrt_ratio_at_tick`.
/// Equivalent to `get_sqrt_ratio_at_tick(MAX_TICK)`
/// 1461446703485210103287273052203988822378723970342
pub const MAX_SQRT_RATIO: U256 = U256::from_limbs([
    0x5d951d5263988d26,
    0xefd1fc6a50648849,
    0xfffd8963,
    0,
]);


/// Calculates sqrt(1.0001^tick) * 2^96 as a Q64.96 fixed-point number.
/// Panics if |tick| > MAX_TICK.
///
/// Direct port of Uniswap V3 TickMath.getSqrtRatioAtTick
pub fn get_sqrt_ratio_at_tick(tick: i32) -> U256 {
    let abs_tick = if tick < 0 {
        (-tick) as u32
    } else {
        tick as u32
    };
    assert!(abs_tick <= MAX_TICK as u32, "T");

    let abs_tick = abs_tick as u64;

    let mut ratio: U256 = if abs_tick & 0x1 != 0 {
        U256::from_str_radix("fffcb933bd6fad37aa2d162d1a594001", 16).unwrap()
    } else {
        U256::from_str_radix("100000000000000000000000000000000", 16).unwrap()
    };

    macro_rules! check_bit {
        ($bit:expr, $magic:expr) => {
            if abs_tick & $bit != 0 {
                ratio = (ratio * U256::from_str_radix($magic, 16).unwrap()) >> 128;
            }
        };
    }

    check_bit!(0x2, "fff97272373d413259a46990580e213a");
    check_bit!(0x4, "fff2e50f5f656932ef12357cf3c7fdcc");
    check_bit!(0x8, "ffe5caca7e10e4e61c3624eaa0941cd0");
    check_bit!(0x10, "ffcb9843d60f6159c9db58835c926644");
    check_bit!(0x20, "ff973b41fa98c081472e6896dfb254c0");
    check_bit!(0x40, "ff2ea16466c96a3843ec78b326b52861");
    check_bit!(0x80, "fe5dee046a99a2a811c461f1969c3053");
    check_bit!(0x100, "fcbe86c7900a88aedcffc83b479aa3a4");
    check_bit!(0x200, "f987a7253ac413176f2b074cf7815e54");
    check_bit!(0x400, "f3392b0822b70005940c7a398e4b70f3");
    check_bit!(0x800, "e7159475a2c29b7443b29c7fa6e889d9");
    check_bit!(0x1000, "d097f3bdfd2022b8845ad8f792aa5825");
    check_bit!(0x2000, "a9f746462d870fdf8a65dc1f90e061e5");
    check_bit!(0x4000, "70d869a156d2a1b890bb3df62baf32f7");
    check_bit!(0x8000, "31be135f97d08fd981231505542fcfa6");
    check_bit!(0x10000, "9aa508b5b7a84e1c677de54f3e99bc9");
    check_bit!(0x20000, "5d6af8dedb81196699c329225ee604");
    check_bit!(0x40000, "2216e584f5fa1ea926041bedfe98");
    check_bit!(0x80000, "48a170391f7dc42444e8fa2");

    if tick > 0 {
        ratio = U256::MAX / ratio;
    }

    // Divide by 1<<32 rounding up to go from Q128.128 to Q128.96
    let remainder = ratio % (U256::from(1) << 32);
    let shift = ratio >> 32;
    if remainder == U256::ZERO {
        shift
    } else {
        shift + U256::from(1)
    }
}

/// Calculates the greatest tick value such that `get_sqrt_ratio_at_tick(tick) <= ratio`.
/// Panics if sqrtPriceX96 < MIN_SQRT_RATIO or sqrtPriceX96 >= MAX_SQRT_RATIO.
///
/// Direct port of Uniswap V3 TickMath.getTickAtSqrtRatio
pub fn get_tick_at_sqrt_ratio(sqrt_price_x96: U256) -> i32 {
    let max_sqrt = MAX_SQRT_RATIO;
    assert!(
        sqrt_price_x96 >= MIN_SQRT_RATIO && sqrt_price_x96 < max_sqrt,
        "R"
    );

    let ratio: U256 = sqrt_price_x96 << 32;

    let mut r = ratio;
    let mut msb: U256 = U256::ZERO;

    // Binary search for MSB (most significant bit)
    // f = (r > 0xFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF) << 7
    {
        let gt = if r > U256::from_str_radix("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF", 16).unwrap() {
            U256::from(1)
        } else {
            U256::ZERO
        };
        let f = gt << 7;
        msb = msb | f;
        r = r >> f;
    }
    {
        let gt = if r > U256::from(0xFFFFFFFFFFFFFFFFu64) {
            U256::from(1)
        } else {
            U256::ZERO
        };
        let f = gt << 6;
        msb = msb | f;
        r = r >> f;
    }
    {
        let gt = if r > U256::from(0xFFFFFFFFu64) {
            U256::from(1)
        } else {
            U256::ZERO
        };
        let f = gt << 5;
        msb = msb | f;
        r = r >> f;
    }
    {
        let gt = if r > U256::from(0xFFFFu64) {
            U256::from(1)
        } else {
            U256::ZERO
        };
        let f = gt << 4;
        msb = msb | f;
        r = r >> f;
    }
    {
        let gt = if r > U256::from(0xFFu64) {
            U256::from(1)
        } else {
            U256::ZERO
        };
        let f = gt << 3;
        msb = msb | f;
        r = r >> f;
    }
    {
        let gt = if r > U256::from(0xFu64) {
            U256::from(1)
        } else {
            U256::ZERO
        };
        let f = gt << 2;
        msb = msb | f;
        r = r >> f;
    }
    {
        let gt = if r > U256::from(0x3u64) {
            U256::from(1)
        } else {
            U256::ZERO
        };
        let f = gt << 1;
        msb = msb | f;
        r = r >> f;
    }
    {
        let gt = if r > U256::from(0x1u64) {
            U256::from(1)
        } else {
            U256::ZERO
        };
        msb = msb | gt;
    }

    let msb_val: u32 = msb.to::<u32>();

    if msb_val >= 128 {
        r = ratio >> (msb_val - 127);
    } else {
        r = ratio << (127 - msb_val);
    }

    // log_2 = (int256(msb) - 128) << 64, but we work with I256
    let mut log_2: I256 = I256::try_from(msb_val as i64 - 128).unwrap() << 64;

    // 14 iterations of squaring
    macro_rules! log2_step {
        ($shift:expr) => {
            // r = (r * r) >> 127
            r = (r.wrapping_mul(r)) >> 127;
            // f = r >> 128 (0 or 1)
            let f: U256 = r >> 128;
            // log_2 |= f << $shift
            let f_i256 = I256::try_from(f).unwrap();
            log_2 = log_2 | (f_i256 << $shift);
            // r >>= f (shift by 0 or 1)
            r = r >> f.as_limbs()[0] as usize;
        };
    }

    log2_step!(63);
    log2_step!(62);
    log2_step!(61);
    log2_step!(60);
    log2_step!(59);
    log2_step!(58);
    log2_step!(57);
    log2_step!(56);
    log2_step!(55);
    log2_step!(54);
    log2_step!(53);
    log2_step!(52);
    log2_step!(51);

    // Last iteration (bit 50), no r >>= f needed after
    {
        r = (r.wrapping_mul(r)) >> 127;
        let f = r >> 128;
        let f_i256 = I256::try_from(f).unwrap();
        log_2 = log_2 | (f_i256 << 50);
    }

    // log_sqrt10001 = log_2 * 255738958999603826347141
    let magic_mul =
        I256::try_from(U256::from_str_radix("255738958999603826347141", 10).unwrap()).unwrap();
    let log_sqrt10001 = log_2.wrapping_mul(magic_mul);

    // tickLow = int24((log_sqrt10001 - 3402992956809132418596140100660247210) >> 128)
    let magic_sub =
        I256::try_from(U256::from_str_radix("3402992956809132418596140100660247210", 10).unwrap())
            .unwrap();
    let tick_low_i256: I256 = (log_sqrt10001 - magic_sub) >> 128;

    // tickHi = int24((log_sqrt10001 + 291339464771989622907027621153398088495) >> 128)
    let magic_add = I256::try_from(
        U256::from_str_radix("291339464771989622907027621153398088495", 10).unwrap(),
    )
    .unwrap();
    let tick_hi_i256: I256 = (log_sqrt10001 + magic_add) >> 128;

    // These values always fit in i32 since they are tick values in [-887272, 887272].
    // Extract as low i64 bits by converting through the two's complement representation.
    let tick_low: i32 = i256_to_i32(tick_low_i256);
    let tick_hi: i32 = i256_to_i32(tick_hi_i256);

    if tick_low == tick_hi {
        tick_low
    } else if get_sqrt_ratio_at_tick(tick_hi) <= sqrt_price_x96 {
        tick_hi
    } else {
        tick_low
    }
}

/// Convert an I256 value that is known to fit in i32 range to i32.
/// Equivalent to Solidity's int24(x) truncation for small values.
fn i256_to_i32(val: I256) -> i32 {
    // I256 stores as two's complement in a U256.
    // For negative values, the high bits are all 1s.
    // We can extract the low 32 bits and sign-extend.
    let raw = val.into_raw();
    let low = raw.as_limbs()[0];
    low as i64 as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_min_tick() {
        let sqrt = get_sqrt_ratio_at_tick(MIN_TICK);
        assert_eq!(sqrt, MIN_SQRT_RATIO);
    }

    #[test]
    fn test_max_tick() {
        let sqrt = get_sqrt_ratio_at_tick(MAX_TICK);
        assert_eq!(sqrt, MAX_SQRT_RATIO);
    }

    #[test]
    fn test_tick_zero() {
        // tick 0 => sqrtPrice = 1.0 * 2^96 = 2^96
        let sqrt = get_sqrt_ratio_at_tick(0);
        let q96 = U256::from(1) << 96;
        assert_eq!(sqrt, q96);
    }

    #[test]
    fn test_roundtrip() {
        for tick in [-887272, -50000, -100, 0, 100, 50000, 887272] {
            let sqrt = get_sqrt_ratio_at_tick(tick);
            if tick == MAX_TICK {
                // max tick's sqrt ratio is at the boundary, can't invert
                continue;
            }
            let recovered = get_tick_at_sqrt_ratio(sqrt);
            assert_eq!(recovered, tick, "roundtrip failed for tick {}", tick);
        }
    }

    #[test]
    #[should_panic(expected = "T")]
    fn test_tick_too_large() {
        get_sqrt_ratio_at_tick(887273);
    }

    #[test]
    #[should_panic(expected = "R")]
    fn test_sqrt_ratio_too_small() {
        get_tick_at_sqrt_ratio(U256::from(4295128738u64));
    }
}
