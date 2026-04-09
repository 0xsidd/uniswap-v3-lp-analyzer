pub mod full_math;
pub mod liquidity_math;
pub mod sqrt_price_math;
pub mod swap_math;
pub mod tick_math;
pub mod unsafe_math;

// FixedPoint constants
use alloy::primitives::U256;

/// FixedPoint96: Q96 = 2^96
pub const FIXED_POINT_96_Q96: U256 = U256::from_limbs([0, 0x100000000, 0, 0]);
/// FixedPoint96: RESOLUTION = 96
pub const FIXED_POINT_96_RESOLUTION: u8 = 96;

/// FixedPoint128: Q128 = 2^128
pub const FIXED_POINT_128_Q128: U256 = U256::from_limbs([0, 0, 1, 0]);
