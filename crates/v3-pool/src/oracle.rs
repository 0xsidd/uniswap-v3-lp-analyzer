use alloy::primitives::U256;

/// Maximum oracle array size (matches Solidity `Observation[65535]`).
pub const MAX_OBSERVATIONS: usize = 65535;

/// Oracle Observation — mirrors the Solidity struct exactly.
#[derive(Clone, Copy, Debug, Default)]
pub struct Observation {
    /// Block timestamp of the observation (uint32).
    pub block_timestamp: u32,
    /// Tick accumulator: tick * time elapsed since pool was first initialized (int56 in Solidity).
    pub tick_cumulative: i64,
    /// Seconds per liquidity: seconds elapsed / max(1, liquidity) since pool init (uint160 in Solidity).
    pub seconds_per_liquidity_cumulative_x128: U256,
    /// Whether the observation is initialized.
    pub initialized: bool,
}

// ── Private helpers ──────────────────────────────────────────────────────────

/// Transforms a previous observation into a new observation.
/// `block_timestamp` must be >= `last.block_timestamp`, safe for 0 or 1 overflow of uint32.
fn transform(last: &Observation, block_timestamp: u32, tick: i32, liquidity: u128) -> Observation {
    let delta = block_timestamp.wrapping_sub(last.block_timestamp);
    let effective_liquidity = if liquidity > 0 { liquidity } else { 1 };

    Observation {
        block_timestamp,
        tick_cumulative: last
            .tick_cumulative
            .wrapping_add((tick as i64).wrapping_mul(delta as i64)),
        seconds_per_liquidity_cumulative_x128: last.seconds_per_liquidity_cumulative_x128
            + ((U256::from(delta) << 128) / U256::from(effective_liquidity)),
        initialized: true,
    }
}

/// Comparator for 32-bit timestamps. Safe for 0 or 1 overflows.
/// `a` and `b` must be chronologically before or equal to `time`.
/// Returns true if `a` is chronologically <= `b`.
fn lte(time: u32, a: u32, b: u32) -> bool {
    if a <= time && b <= time {
        return a <= b;
    }
    let a_adjusted: u64 = if a > time { a as u64 } else { a as u64 + (1u64 << 32) };
    let b_adjusted: u64 = if b > time { b as u64 } else { b as u64 + (1u64 << 32) };
    a_adjusted <= b_adjusted
}

/// Binary search within the circular oracle array.
/// Returns `(before_or_at, at_or_after)`.
fn binary_search(
    observations: &[Observation],
    time: u32,
    target: u32,
    index: u16,
    cardinality: u16,
) -> (Observation, Observation) {
    let card = cardinality as usize;
    let mut l: usize = (index as usize + 1) % card; // oldest observation
    let mut r: usize = l + card - 1; // newest observation

    let before_or_at;
    let at_or_after;

    loop {
        let i = (l + r) / 2;
        let candidate = observations[i % card];

        // landed on uninitialized slot — search higher (more recently)
        if !candidate.initialized {
            l = i + 1;
            continue;
        }

        let next = observations[(i + 1) % card];
        let target_at_or_after = lte(time, candidate.block_timestamp, target);

        if target_at_or_after && lte(time, target, next.block_timestamp) {
            before_or_at = candidate;
            at_or_after = next;
            break;
        }

        if !target_at_or_after {
            r = i - 1;
        } else {
            l = i + 1;
        }
    }

    (before_or_at, at_or_after)
}

/// Returns surrounding observations for a given target timestamp.
fn get_surrounding_observations(
    observations: &[Observation],
    time: u32,
    target: u32,
    tick: i32,
    index: u16,
    liquidity: u128,
    cardinality: u16,
) -> (Observation, Observation) {
    let card = cardinality as usize;

    // optimistically set before to the newest observation
    let mut before_or_at = observations[index as usize];

    if lte(time, before_or_at.block_timestamp, target) {
        if before_or_at.block_timestamp == target {
            // same block — atOrAfter is irrelevant
            return (before_or_at, Observation::default());
        } else {
            let at_or_after = transform(&before_or_at, target, tick, liquidity);
            return (before_or_at, at_or_after);
        }
    }

    // set before to the oldest observation
    before_or_at = observations[(index as usize + 1) % card];
    if !before_or_at.initialized {
        before_or_at = observations[0];
    }

    assert!(
        lte(time, before_or_at.block_timestamp, target),
        "OLD"
    );

    binary_search(observations, time, target, index, cardinality)
}

/// Fetches accumulator values at a single `seconds_ago` offset.
fn observe_single(
    observations: &[Observation],
    time: u32,
    seconds_ago: u32,
    tick: i32,
    index: u16,
    liquidity: u128,
    cardinality: u16,
) -> (i64, U256) {
    if seconds_ago == 0 {
        let mut last = observations[index as usize];
        if last.block_timestamp != time {
            last = transform(&last, time, tick, liquidity);
        }
        return (last.tick_cumulative, last.seconds_per_liquidity_cumulative_x128);
    }

    let target = time.wrapping_sub(seconds_ago);

    let (before_or_at, at_or_after) =
        get_surrounding_observations(observations, time, target, tick, index, liquidity, cardinality);

    if target == before_or_at.block_timestamp {
        return (
            before_or_at.tick_cumulative,
            before_or_at.seconds_per_liquidity_cumulative_x128,
        );
    } else if target == at_or_after.block_timestamp {
        return (
            at_or_after.tick_cumulative,
            at_or_after.seconds_per_liquidity_cumulative_x128,
        );
    } else {
        // Interpolation between the two surrounding observations.
        let observation_time_delta =
            at_or_after.block_timestamp.wrapping_sub(before_or_at.block_timestamp);
        let target_delta = target.wrapping_sub(before_or_at.block_timestamp);

        // tick_cumulative: Solidity uses int56 division (truncation toward zero).
        // (atOrAfter.tickCumulative - beforeOrAt.tickCumulative) / observationTimeDelta * targetDelta
        let tick_cumul_diff =
            at_or_after.tick_cumulative.wrapping_sub(before_or_at.tick_cumulative);
        // Solidity int56 division truncates toward zero — Rust i64 division does the same.
        let tick_cumulative = before_or_at.tick_cumulative
            + (tick_cumul_diff / observation_time_delta as i64) * target_delta as i64;

        // secondsPerLiquidityCumulativeX128: uint160 interpolation
        let spl_diff = at_or_after
            .seconds_per_liquidity_cumulative_x128
            .wrapping_sub(before_or_at.seconds_per_liquidity_cumulative_x128);
        let seconds_per_liq = before_or_at.seconds_per_liquidity_cumulative_x128
            + ((spl_diff * U256::from(target_delta)) / U256::from(observation_time_delta));

        (tick_cumulative, seconds_per_liq)
    }
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Initialize the oracle array by writing the first slot.
/// Returns `(cardinality, cardinality_next)` — both 1.
pub fn initialize(observations: &mut Vec<Observation>, time: u32) -> (u16, u16) {
    observations.clear();
    observations.resize(MAX_OBSERVATIONS, Observation::default());
    observations[0] = Observation {
        block_timestamp: time,
        tick_cumulative: 0,
        seconds_per_liquidity_cumulative_x128: U256::ZERO,
        initialized: true,
    };
    (1, 1)
}

/// Writes an oracle observation to the array.
///
/// Returns `(index_updated, cardinality_updated)`.
///
/// If the current block already has an observation (`last.blockTimestamp == blockTimestamp`),
/// returns `(index, cardinality)` unchanged (early return).
#[allow(clippy::too_many_arguments)]
pub fn write(
    observations: &mut [Observation],
    index: u16,
    block_timestamp: u32,
    tick: i32,
    liquidity: u128,
    cardinality: u16,
    cardinality_next: u16,
) -> (u16, u16) {
    let last = observations[index as usize];

    // early return if already written this block
    if last.block_timestamp == block_timestamp {
        return (index, cardinality);
    }

    // bump cardinality if conditions are met
    let cardinality_updated =
        if cardinality_next > cardinality && index == (cardinality - 1) {
            cardinality_next
        } else {
            cardinality
        };

    let index_updated = (index + 1) % cardinality_updated;
    observations[index_updated as usize] =
        transform(&last, block_timestamp, tick, liquidity);

    (index_updated, cardinality_updated)
}

/// Prepares the oracle array to store up to `next` observations.
///
/// Returns the new `cardinality_next`.
pub fn grow(observations: &mut [Observation], current: u16, next: u16) -> u16 {
    assert!(current > 0, "I");
    if next <= current {
        return current;
    }
    // Store a non-zero blockTimestamp in each new slot to prevent fresh SSTOREs in swaps.
    // `initialized` remains false so the data won't be used.
    for i in current..next {
        observations[i as usize].block_timestamp = 1;
    }
    next
}

/// Returns the accumulator values as of each `seconds_ago` offset.
///
/// Returns `(tick_cumulatives, seconds_per_liquidity_cumulative_x128s)`.
pub fn observe(
    observations: &[Observation],
    time: u32,
    seconds_agos: &[u32],
    tick: i32,
    index: u16,
    liquidity: u128,
    cardinality: u16,
) -> (Vec<i64>, Vec<U256>) {
    assert!(cardinality > 0, "I");

    let mut tick_cumulatives = Vec::with_capacity(seconds_agos.len());
    let mut seconds_per_liq_cumulatives = Vec::with_capacity(seconds_agos.len());

    for &sec_ago in seconds_agos {
        let (tc, spl) =
            observe_single(observations, time, sec_ago, tick, index, liquidity, cardinality);
        tick_cumulatives.push(tc);
        seconds_per_liq_cumulatives.push(spl);
    }

    (tick_cumulatives, seconds_per_liq_cumulatives)
}
