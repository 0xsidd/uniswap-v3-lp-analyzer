use alloy::primitives::U256;
use std::collections::HashMap;

// ── Helpers (BitMath equivalents) ────────────────────────────────────────────

/// Returns the index of the most significant bit of `x`.
/// Panics if `x` is zero (matches Solidity `require(x > 0)`).
fn most_significant_bit(x: U256) -> u8 {
    assert!(!x.is_zero(), "BitMath: zero has no MSB");
    // U256 bit_len() returns the number of bits needed, so MSB index = bit_len - 1.
    (x.bit_len() - 1) as u8
}

/// Returns the index of the least significant bit of `x`.
/// Panics if `x` is zero.
fn least_significant_bit(x: U256) -> u8 {
    assert!(!x.is_zero(), "BitMath: zero has no LSB");
    x.trailing_zeros() as u8
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Computes the position in the mapping where the initialized bit for a tick lives.
///
/// Returns `(word_pos, bit_pos)` — mirrors `TickBitmap.position`.
///
/// `word_pos` is `int16(tick >> 8)` and `bit_pos` is `uint8(tick % 256)`.
///
/// For negative ticks, Rust's `>>` on signed integers performs arithmetic shift (sign-extending),
/// which matches Solidity's behaviour for `int24 >> 8`.
/// Rust's `%` can return a negative remainder for negative dividends; casting to `u8` gives the
/// correct modular result identical to Solidity's `uint8(tick % 256)`.
pub fn position(tick: i32) -> (i16, u8) {
    let word_pos = (tick >> 8) as i16;
    let bit_pos = (tick % 256) as u8; // wraps correctly for negative values
    (word_pos, bit_pos)
}

/// Flips the initialized state for a given tick.
/// Panics if `tick % tick_spacing != 0`.
pub fn flip_tick(bitmap: &mut HashMap<i16, U256>, tick: i32, tick_spacing: i32) {
    assert!(tick % tick_spacing == 0, "tick not on spacing");
    let (word_pos, bit_pos) = position(tick / tick_spacing);
    let mask = U256::from(1u64) << bit_pos;
    let word = bitmap.entry(word_pos).or_insert(U256::ZERO);
    *word ^= mask;
}

/// Returns the next initialized tick contained in the same word (or adjacent word) as `tick`.
///
/// `lte`:
/// - `true`  → search to the left (less than or equal)
/// - `false` → search to the right (greater than)
///
/// Returns `(next, initialized)`.
pub fn next_initialized_tick_within_one_word(
    bitmap: &HashMap<i16, U256>,
    tick: i32,
    tick_spacing: i32,
    lte: bool,
) -> (i32, bool) {
    // Solidity: int24 compressed = tick / tickSpacing;
    // For negative ticks that don't divide evenly, round towards negative infinity.
    let mut compressed = tick / tick_spacing;
    if tick < 0 && tick % tick_spacing != 0 {
        compressed -= 1; // round towards negative infinity
    }

    if lte {
        let (word_pos, bit_pos) = position(compressed);
        // all the 1s at or to the right of the current bitPos
        let mask = (U256::from(1u64) << bit_pos) - U256::from(1u64) + (U256::from(1u64) << bit_pos);
        let word = bitmap.get(&word_pos).copied().unwrap_or(U256::ZERO);
        let masked = word & mask;

        let initialized = !masked.is_zero();
        let next = if initialized {
            (compressed - (bit_pos as i32 - most_significant_bit(masked) as i32)) * tick_spacing
        } else {
            (compressed - bit_pos as i32) * tick_spacing
        };
        (next, initialized)
    } else {
        // start from the word of the next tick
        let (word_pos, bit_pos) = position(compressed + 1);
        // all the 1s at or to the left of the bitPos
        let mask = !(((U256::from(1u64) << bit_pos) - U256::from(1u64)));
        let word = bitmap.get(&word_pos).copied().unwrap_or(U256::ZERO);
        let masked = word & mask;

        let initialized = !masked.is_zero();
        let next = if initialized {
            (compressed + 1 + (least_significant_bit(masked) as i32 - bit_pos as i32)) * tick_spacing
        } else {
            (compressed + 1 + (u8::MAX as i32 - bit_pos as i32)) * tick_spacing
        };
        (next, initialized)
    }
}
