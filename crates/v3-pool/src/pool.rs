use alloy::primitives::{Address, I256, U256};
use std::collections::HashMap;

use crate::math::{
    full_math, liquidity_math, sqrt_price_math, swap_math, tick_math, FIXED_POINT_128_Q128,
};
use crate::oracle;
use crate::position;
use crate::tick;
use crate::tick_bitmap;

// ── Sub-structs ────────────────────────────────────────────────────────────────

/// Mirrors Solidity `Slot0`.
#[derive(Clone, Debug)]
pub struct Slot0 {
    pub sqrt_price_x96: U256,
    pub tick: i32,
    pub observation_index: u16,
    pub observation_cardinality: u16,
    pub observation_cardinality_next: u16,
    pub fee_protocol: u8,
    pub unlocked: bool,
}

impl Default for Slot0 {
    fn default() -> Self {
        Self {
            sqrt_price_x96: U256::ZERO,
            tick: 0,
            observation_index: 0,
            observation_cardinality: 0,
            observation_cardinality_next: 0,
            fee_protocol: 0,
            unlocked: false,
        }
    }
}

/// Mirrors Solidity `ProtocolFees`.
#[derive(Clone, Debug, Default)]
pub struct ProtocolFees {
    pub token0: u128,
    pub token1: u128,
}

// ── Internal structs for swap ──────────────────────────────────────────────────

struct SwapCache {
    fee_protocol: u8,
    liquidity_start: u128,
    block_timestamp: u32,
    tick_cumulative: i64,
    seconds_per_liquidity_cumulative_x128: U256,
    computed_latest_observation: bool,
}

struct SwapState {
    amount_specified_remaining: I256,
    amount_calculated: I256,
    sqrt_price_x96: U256,
    tick: i32,
    fee_growth_global_x128: U256,
    protocol_fee: u128,
    liquidity: u128,
}

struct StepComputations {
    sqrt_price_start_x96: U256,
    tick_next: i32,
    initialized: bool,
    sqrt_price_next_x96: U256,
    amount_in: U256,
    amount_out: U256,
    fee_amount: U256,
}

// ── Pool ───────────────────────────────────────────────────────────────────────

/// UniswapV3Pool — full state replica for deterministic replay.
pub struct UniswapV3Pool {
    // ── Immutable config ──
    pub fee: u32,
    pub tick_spacing: i32,
    pub max_liquidity_per_tick: u128,

    // ── Mutable state ──
    pub slot0: Slot0,
    pub fee_growth_global_0_x128: U256,
    pub fee_growth_global_1_x128: U256,
    pub protocol_fees: ProtocolFees,
    pub liquidity: u128,
    pub ticks: HashMap<i32, tick::Info>,
    pub tick_bitmap: HashMap<i16, U256>,
    pub positions: HashMap<position::PositionKey, position::Info>,
    pub observations: Vec<oracle::Observation>,
}

impl UniswapV3Pool {
    // ── Constructor ────────────────────────────────────────────────────────

    pub fn new(fee: u32, tick_spacing: i32) -> Self {
        Self {
            fee,
            tick_spacing,
            max_liquidity_per_tick: tick::tick_spacing_to_max_liquidity_per_tick(tick_spacing),
            slot0: Slot0::default(),
            fee_growth_global_0_x128: U256::ZERO,
            fee_growth_global_1_x128: U256::ZERO,
            protocol_fees: ProtocolFees::default(),
            liquidity: 0,
            ticks: HashMap::new(),
            tick_bitmap: HashMap::new(),
            positions: HashMap::new(),
            observations: Vec::new(),
        }
    }

    // ── initialize ─────────────────────────────────────────────────────────

    pub fn initialize(&mut self, sqrt_price_x96: U256) {
        assert!(self.slot0.sqrt_price_x96 == U256::ZERO, "AI");

        let tick = tick_math::get_tick_at_sqrt_ratio(sqrt_price_x96);

        let (cardinality, cardinality_next) = oracle::initialize(&mut self.observations, 0);

        self.slot0 = Slot0 {
            sqrt_price_x96,
            tick,
            observation_index: 0,
            observation_cardinality: cardinality,
            observation_cardinality_next: cardinality_next,
            fee_protocol: 0,
            unlocked: true,
        };
    }

    // ── Internal: check ticks ──────────────────────────────────────────────

    fn check_ticks(tick_lower: i32, tick_upper: i32) {
        assert!(tick_lower < tick_upper, "TLU");
        assert!(tick_lower >= tick_math::MIN_TICK, "TLM");
        assert!(tick_upper <= tick_math::MAX_TICK, "TUM");
    }

    // ── Internal: _updatePosition ──────────────────────────────────────────

    fn update_position(
        &mut self,
        owner: Address,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
        tick_current: i32,
        block_timestamp: u32,
    ) {
        let fg0 = self.fee_growth_global_0_x128;
        let fg1 = self.fee_growth_global_1_x128;

        let mut flipped_lower = false;
        let mut flipped_upper = false;

        if liquidity_delta != 0 {
            // observe to get oracle accumulators
            let (tick_cumulative, seconds_per_liquidity_cumulative_x128) = {
                let (tcs, spls) = oracle::observe(
                    &self.observations,
                    block_timestamp,
                    &[0],
                    self.slot0.tick,
                    self.slot0.observation_index,
                    self.liquidity,
                    self.slot0.observation_cardinality,
                );
                (tcs[0], spls[0])
            };

            flipped_lower = tick::update(
                &mut self.ticks,
                tick_lower,
                tick_current,
                liquidity_delta,
                fg0,
                fg1,
                seconds_per_liquidity_cumulative_x128,
                tick_cumulative,
                block_timestamp,
                false,
                self.max_liquidity_per_tick,
            );

            flipped_upper = tick::update(
                &mut self.ticks,
                tick_upper,
                tick_current,
                liquidity_delta,
                fg0,
                fg1,
                seconds_per_liquidity_cumulative_x128,
                tick_cumulative,
                block_timestamp,
                true,
                self.max_liquidity_per_tick,
            );

            if flipped_lower {
                tick_bitmap::flip_tick(&mut self.tick_bitmap, tick_lower, self.tick_spacing);
            }
            if flipped_upper {
                tick_bitmap::flip_tick(&mut self.tick_bitmap, tick_upper, self.tick_spacing);
            }
        }

        let (fee_growth_inside_0, fee_growth_inside_1) = tick::get_fee_growth_inside(
            &self.ticks,
            tick_lower,
            tick_upper,
            tick_current,
            fg0,
            fg1,
        );

        let pos = position::get(&mut self.positions, owner, tick_lower, tick_upper);
        position::update(pos, liquidity_delta, fee_growth_inside_0, fee_growth_inside_1);

        // clear tick data that is no longer needed
        if liquidity_delta < 0 {
            if flipped_lower {
                tick::clear(&mut self.ticks, tick_lower);
            }
            if flipped_upper {
                tick::clear(&mut self.ticks, tick_upper);
            }
        }
    }

    // ── Internal: _modifyPosition ──────────────────────────────────────────

    fn modify_position(
        &mut self,
        owner: Address,
        tick_lower: i32,
        tick_upper: i32,
        liquidity_delta: i128,
        block_timestamp: u32,
    ) -> (I256, I256) {
        Self::check_ticks(tick_lower, tick_upper);

        let slot0_tick = self.slot0.tick;
        let slot0_sqrt_price = self.slot0.sqrt_price_x96;

        self.update_position(
            owner,
            tick_lower,
            tick_upper,
            liquidity_delta,
            slot0_tick,
            block_timestamp,
        );

        let mut amount0 = I256::ZERO;
        let mut amount1 = I256::ZERO;

        if liquidity_delta != 0 {
            if slot0_tick < tick_lower {
                // current tick is below the passed range
                amount0 = sqrt_price_math::get_amount0_delta_signed(
                    tick_math::get_sqrt_ratio_at_tick(tick_lower),
                    tick_math::get_sqrt_ratio_at_tick(tick_upper),
                    liquidity_delta,
                );
            } else if slot0_tick < tick_upper {
                // current tick is inside the passed range
                let liquidity_before = self.liquidity;

                // write an oracle entry
                let (obs_idx, obs_card) = oracle::write(
                    &mut self.observations,
                    self.slot0.observation_index,
                    block_timestamp,
                    slot0_tick,
                    liquidity_before,
                    self.slot0.observation_cardinality,
                    self.slot0.observation_cardinality_next,
                );
                self.slot0.observation_index = obs_idx;
                self.slot0.observation_cardinality = obs_card;

                amount0 = sqrt_price_math::get_amount0_delta_signed(
                    slot0_sqrt_price,
                    tick_math::get_sqrt_ratio_at_tick(tick_upper),
                    liquidity_delta,
                );
                amount1 = sqrt_price_math::get_amount1_delta_signed(
                    tick_math::get_sqrt_ratio_at_tick(tick_lower),
                    slot0_sqrt_price,
                    liquidity_delta,
                );

                self.liquidity = liquidity_math::add_delta(liquidity_before, liquidity_delta);
            } else {
                // current tick is above the passed range
                amount1 = sqrt_price_math::get_amount1_delta_signed(
                    tick_math::get_sqrt_ratio_at_tick(tick_lower),
                    tick_math::get_sqrt_ratio_at_tick(tick_upper),
                    liquidity_delta,
                );
            }
        }

        (amount0, amount1)
    }

    // ── mint ───────────────────────────────────────────────────────────────

    /// Adds liquidity. Returns (amount0, amount1) owed to the pool.
    pub fn mint(
        &mut self,
        owner: Address,
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
        block_timestamp: u32,
    ) -> (I256, I256) {
        assert!(self.slot0.unlocked, "LOK");
        assert!(amount > 0, "mint amount must be > 0");

        let liquidity_delta = amount as i128;
        let (amount0, amount1) =
            self.modify_position(owner, tick_lower, tick_upper, liquidity_delta, block_timestamp);

        (amount0, amount1)
    }

    // ── burn ───────────────────────────────────────────────────────────────

    /// Removes liquidity. Returns (amount0, amount1) owed to the position owner.
    pub fn burn(
        &mut self,
        owner: Address,
        tick_lower: i32,
        tick_upper: i32,
        amount: u128,
        block_timestamp: u32,
    ) -> (U256, U256) {
        assert!(self.slot0.unlocked, "LOK");

        let liquidity_delta = -(amount as i128);
        let (amount0_int, amount1_int) =
            self.modify_position(owner, tick_lower, tick_upper, liquidity_delta, block_timestamp);

        // amount0 = uint256(-amount0Int), amount1 = uint256(-amount1Int)
        let amount0 = (-amount0_int).into_raw();
        let amount1 = (-amount1_int).into_raw();

        if amount0 > U256::ZERO || amount1 > U256::ZERO {
            let pos = position::get(&mut self.positions, owner, tick_lower, tick_upper);
            pos.tokens_owed_0 = pos.tokens_owed_0.wrapping_add(amount0.to::<u128>());
            pos.tokens_owed_1 = pos.tokens_owed_1.wrapping_add(amount1.to::<u128>());
        }

        (amount0, amount1)
    }

    // ── collect ────────────────────────────────────────────────────────────

    /// Withdraws accumulated fees from a position.
    pub fn collect(
        &mut self,
        owner: Address,
        tick_lower: i32,
        tick_upper: i32,
        amount0_requested: u128,
        amount1_requested: u128,
    ) -> (u128, u128) {
        assert!(self.slot0.unlocked, "LOK");

        let pos = position::get(&mut self.positions, owner, tick_lower, tick_upper);

        let amount0 = if amount0_requested > pos.tokens_owed_0 {
            pos.tokens_owed_0
        } else {
            amount0_requested
        };
        let amount1 = if amount1_requested > pos.tokens_owed_1 {
            pos.tokens_owed_1
        } else {
            amount1_requested
        };

        if amount0 > 0 {
            pos.tokens_owed_0 -= amount0;
        }
        if amount1 > 0 {
            pos.tokens_owed_1 -= amount1;
        }

        (amount0, amount1)
    }

    // ── swap ───────────────────────────────────────────────────────────────

    /// The main swap function. Returns (amount0, amount1) — positive means the pool received,
    /// negative means the pool paid out.
    pub fn swap(
        &mut self,
        zero_for_one: bool,
        amount_specified: I256,
        sqrt_price_limit_x96: U256,
        block_timestamp: u32,
    ) -> (I256, I256) {
        assert!(amount_specified != I256::ZERO, "AS");

        let slot0_start = self.slot0.clone();
        assert!(slot0_start.unlocked, "LOK");

        if zero_for_one {
            assert!(
                sqrt_price_limit_x96 < slot0_start.sqrt_price_x96
                    && sqrt_price_limit_x96 > tick_math::MIN_SQRT_RATIO,
                "SPL"
            );
        } else {
            assert!(
                sqrt_price_limit_x96 > slot0_start.sqrt_price_x96
                    && sqrt_price_limit_x96 < tick_math::MAX_SQRT_RATIO,
                "SPL"
            );
        }

        self.slot0.unlocked = false;

        let mut cache = SwapCache {
            liquidity_start: self.liquidity,
            block_timestamp,
            fee_protocol: if zero_for_one {
                slot0_start.fee_protocol % 16
            } else {
                slot0_start.fee_protocol >> 4
            },
            seconds_per_liquidity_cumulative_x128: U256::ZERO,
            tick_cumulative: 0,
            computed_latest_observation: false,
        };

        let exact_input = amount_specified > I256::ZERO;

        let mut state = SwapState {
            amount_specified_remaining: amount_specified,
            amount_calculated: I256::ZERO,
            sqrt_price_x96: slot0_start.sqrt_price_x96,
            tick: slot0_start.tick,
            fee_growth_global_x128: if zero_for_one {
                self.fee_growth_global_0_x128
            } else {
                self.fee_growth_global_1_x128
            },
            protocol_fee: 0,
            liquidity: cache.liquidity_start,
        };

        // ── swap loop ──────────────────────────────────────────────────────
        while state.amount_specified_remaining != I256::ZERO
            && state.sqrt_price_x96 != sqrt_price_limit_x96
        {
            let mut step = StepComputations {
                sqrt_price_start_x96: state.sqrt_price_x96,
                tick_next: 0,
                initialized: false,
                sqrt_price_next_x96: U256::ZERO,
                amount_in: U256::ZERO,
                amount_out: U256::ZERO,
                fee_amount: U256::ZERO,
            };

            (step.tick_next, step.initialized) =
                tick_bitmap::next_initialized_tick_within_one_word(
                    &self.tick_bitmap,
                    state.tick,
                    self.tick_spacing,
                    zero_for_one,
                );

            // clamp to min/max tick
            if step.tick_next < tick_math::MIN_TICK {
                step.tick_next = tick_math::MIN_TICK;
            } else if step.tick_next > tick_math::MAX_TICK {
                step.tick_next = tick_math::MAX_TICK;
            }

            step.sqrt_price_next_x96 = tick_math::get_sqrt_ratio_at_tick(step.tick_next);

            // compute swap step — choose target price
            let sqrt_ratio_target = if zero_for_one {
                if step.sqrt_price_next_x96 < sqrt_price_limit_x96 {
                    sqrt_price_limit_x96
                } else {
                    step.sqrt_price_next_x96
                }
            } else {
                if step.sqrt_price_next_x96 > sqrt_price_limit_x96 {
                    sqrt_price_limit_x96
                } else {
                    step.sqrt_price_next_x96
                }
            };

            (
                state.sqrt_price_x96,
                step.amount_in,
                step.amount_out,
                step.fee_amount,
            ) = swap_math::compute_swap_step(
                state.sqrt_price_x96,
                sqrt_ratio_target,
                state.liquidity,
                state.amount_specified_remaining,
                self.fee,
            );

            if exact_input {
                // amountSpecifiedRemaining -= (amountIn + feeAmount).toInt256()
                let consumed =
                    I256::try_from(step.amount_in + step.fee_amount).expect("safe cast");
                state.amount_specified_remaining =
                    state.amount_specified_remaining.wrapping_sub(consumed);
                // amountCalculated -= amountOut.toInt256()
                let out_i = I256::try_from(step.amount_out).expect("safe cast");
                state.amount_calculated = state.amount_calculated.wrapping_sub(out_i);
            } else {
                // amountSpecifiedRemaining += amountOut.toInt256()
                let out_i = I256::try_from(step.amount_out).expect("safe cast");
                state.amount_specified_remaining =
                    state.amount_specified_remaining.wrapping_add(out_i);
                // amountCalculated += (amountIn + feeAmount).toInt256()
                let in_plus_fee =
                    I256::try_from(step.amount_in + step.fee_amount).expect("safe cast");
                state.amount_calculated = state.amount_calculated.wrapping_add(in_plus_fee);
            }

            // protocol fee
            if cache.fee_protocol > 0 {
                let delta = step.fee_amount / U256::from(cache.fee_protocol);
                step.fee_amount -= delta;
                state.protocol_fee += delta.to::<u128>();
            }

            // update global fee tracker
            if state.liquidity > 0 {
                state.fee_growth_global_x128 = state.fee_growth_global_x128.wrapping_add(
                    full_math::mul_div(
                        step.fee_amount,
                        FIXED_POINT_128_Q128,
                        U256::from(state.liquidity),
                    ),
                );
            }

            // shift tick if we reached the next price
            if state.sqrt_price_x96 == step.sqrt_price_next_x96 {
                if step.initialized {
                    if !cache.computed_latest_observation {
                        let (tcs, spls) = oracle::observe(
                            &self.observations,
                            cache.block_timestamp,
                            &[0],
                            slot0_start.tick,
                            slot0_start.observation_index,
                            cache.liquidity_start,
                            slot0_start.observation_cardinality,
                        );
                        cache.tick_cumulative = tcs[0];
                        cache.seconds_per_liquidity_cumulative_x128 = spls[0];
                        cache.computed_latest_observation = true;
                    }

                    let liquidity_net = tick::cross(
                        &mut self.ticks,
                        step.tick_next,
                        if zero_for_one {
                            state.fee_growth_global_x128
                        } else {
                            self.fee_growth_global_0_x128
                        },
                        if zero_for_one {
                            self.fee_growth_global_1_x128
                        } else {
                            state.fee_growth_global_x128
                        },
                        cache.seconds_per_liquidity_cumulative_x128,
                        cache.tick_cumulative,
                        cache.block_timestamp,
                    );

                    let liquidity_net = if zero_for_one {
                        -liquidity_net
                    } else {
                        liquidity_net
                    };

                    state.liquidity = liquidity_math::add_delta(state.liquidity, liquidity_net);
                }

                state.tick = if zero_for_one {
                    step.tick_next - 1
                } else {
                    step.tick_next
                };
            } else if state.sqrt_price_x96 != step.sqrt_price_start_x96 {
                // recompute tick
                state.tick = tick_math::get_tick_at_sqrt_ratio(state.sqrt_price_x96);
            }
        }

        // ── post-loop: update slot0 ────────────────────────────────────────
        if state.tick != slot0_start.tick {
            let (obs_idx, obs_card) = oracle::write(
                &mut self.observations,
                slot0_start.observation_index,
                cache.block_timestamp,
                slot0_start.tick,
                cache.liquidity_start,
                slot0_start.observation_cardinality,
                slot0_start.observation_cardinality_next,
            );
            self.slot0.sqrt_price_x96 = state.sqrt_price_x96;
            self.slot0.tick = state.tick;
            self.slot0.observation_index = obs_idx;
            self.slot0.observation_cardinality = obs_card;
        } else {
            self.slot0.sqrt_price_x96 = state.sqrt_price_x96;
        }

        // update liquidity if it changed
        if cache.liquidity_start != state.liquidity {
            self.liquidity = state.liquidity;
        }

        // update fee growth global and protocol fees
        if zero_for_one {
            self.fee_growth_global_0_x128 = state.fee_growth_global_x128;
            if state.protocol_fee > 0 {
                self.protocol_fees.token0 =
                    self.protocol_fees.token0.wrapping_add(state.protocol_fee);
            }
        } else {
            self.fee_growth_global_1_x128 = state.fee_growth_global_x128;
            if state.protocol_fee > 0 {
                self.protocol_fees.token1 =
                    self.protocol_fees.token1.wrapping_add(state.protocol_fee);
            }
        }

        // compute final amounts
        let (amount0, amount1) = if zero_for_one == exact_input {
            (
                amount_specified.wrapping_sub(state.amount_specified_remaining),
                state.amount_calculated,
            )
        } else {
            (
                state.amount_calculated,
                amount_specified.wrapping_sub(state.amount_specified_remaining),
            )
        };

        self.slot0.unlocked = true;
        (amount0, amount1)
    }

    // ── replay_swap ────────────────────────────────────────────────────────

    /// Replays a swap using event data for exact fee reproduction.
    ///
    /// Instead of guessing exactInput vs exactOutput (which affects fee rounding),
    /// this walks from the current price to the event's final price, computing fees
    /// at each tick crossing identically to on-chain, and derives the last step's
    /// fee from the known total input amount (from the event).
    #[allow(clippy::too_many_arguments)]
    pub fn replay_swap(
        &mut self,
        zero_for_one: bool,
        amount0: I256,
        amount1: I256,
        event_sqrt_price_x96: U256,
        event_tick: i32,
        event_liquidity: u128,
        block_timestamp: u32,
    ) {
        let slot0_start = self.slot0.clone();
        assert!(slot0_start.unlocked, "LOK");
        self.slot0.unlocked = false;

        let fee_protocol_val = if zero_for_one {
            slot0_start.fee_protocol % 16
        } else {
            slot0_start.fee_protocol >> 4
        };

        // Total input from the event (the positive/input side)
        let total_input = if zero_for_one {
            amount0.into_raw()
        } else {
            amount1.into_raw()
        };

        let mut current_sqrt_price = slot0_start.sqrt_price_x96;
        let mut current_tick = slot0_start.tick;
        let mut current_liquidity = self.liquidity;
        let mut fee_growth_global = if zero_for_one {
            self.fee_growth_global_0_x128
        } else {
            self.fee_growth_global_1_x128
        };
        let mut total_protocol_fee: u128 = 0;
        let mut total_consumed = U256::ZERO;

        // Cache oracle accumulators (computed once on first initialized tick cross)
        let mut cache_computed = false;
        let mut cache_tick_cumulative: i64 = 0;
        let mut cache_seconds_per_liq: U256 = U256::ZERO;

        while current_sqrt_price != event_sqrt_price_x96 {
            // Find next initialized tick
            let (tick_next_raw, initialized) =
                tick_bitmap::next_initialized_tick_within_one_word(
                    &self.tick_bitmap,
                    current_tick,
                    self.tick_spacing,
                    zero_for_one,
                );
            let tick_next = tick_next_raw.clamp(tick_math::MIN_TICK, tick_math::MAX_TICK);
            let sqrt_price_at_tick = tick_math::get_sqrt_ratio_at_tick(tick_next);

            // Determine step target: don't overshoot event price
            let (target_sqrt_price, reached_tick) = if zero_for_one {
                if sqrt_price_at_tick < event_sqrt_price_x96 {
                    (event_sqrt_price_x96, false)
                } else {
                    (sqrt_price_at_tick, true)
                }
            } else {
                if sqrt_price_at_tick > event_sqrt_price_x96 {
                    (event_sqrt_price_x96, false)
                } else {
                    (sqrt_price_at_tick, true)
                }
            };

            let is_last_step = target_sqrt_price == event_sqrt_price_x96;

            // Compute amountIn for this step
            let amount_in = if zero_for_one {
                sqrt_price_math::get_amount0_delta(
                    target_sqrt_price,
                    current_sqrt_price,
                    current_liquidity,
                    true,
                )
            } else {
                sqrt_price_math::get_amount1_delta(
                    current_sqrt_price,
                    target_sqrt_price,
                    current_liquidity,
                    true,
                )
            };

            // Compute fee:
            // - For intermediate steps (reaching a tick): standard formula (same in both modes)
            // - For the last step: derive from remaining input = total_input - consumed_so_far - amountIn
            //   This exactly recovers the on-chain fee regardless of exactInput vs exactOutput
            let fee_amount = if is_last_step {
                let remaining = total_input.saturating_sub(total_consumed);
                remaining.saturating_sub(amount_in)
            } else {
                full_math::mul_div_rounding_up(
                    amount_in,
                    U256::from(self.fee),
                    U256::from(1_000_000u32 - self.fee),
                )
            };

            total_consumed = total_consumed + amount_in + fee_amount;

            // Protocol fee deduction
            let mut step_fee_for_growth = fee_amount;
            if fee_protocol_val > 0 {
                let delta = step_fee_for_growth / U256::from(fee_protocol_val);
                step_fee_for_growth = step_fee_for_growth - delta;
                total_protocol_fee += delta.as_limbs()[0] as u128
                    | ((delta.as_limbs()[1] as u128) << 64);
            }

            // Update fee growth
            if current_liquidity > 0 && step_fee_for_growth > U256::ZERO {
                fee_growth_global = fee_growth_global.wrapping_add(full_math::mul_div(
                    step_fee_for_growth,
                    FIXED_POINT_128_Q128,
                    U256::from(current_liquidity),
                ));
            }

            // Cross tick if we reached an initialized tick
            if reached_tick && initialized {
                if !cache_computed {
                    let (tcs, spls) = oracle::observe(
                        &self.observations,
                        block_timestamp,
                        &[0],
                        slot0_start.tick,
                        slot0_start.observation_index,
                        self.liquidity,
                        slot0_start.observation_cardinality,
                    );
                    cache_tick_cumulative = tcs[0];
                    cache_seconds_per_liq = spls[0];
                    cache_computed = true;
                }

                let liquidity_net = tick::cross(
                    &mut self.ticks,
                    tick_next,
                    if zero_for_one {
                        fee_growth_global
                    } else {
                        self.fee_growth_global_0_x128
                    },
                    if zero_for_one {
                        self.fee_growth_global_1_x128
                    } else {
                        fee_growth_global
                    },
                    cache_seconds_per_liq,
                    cache_tick_cumulative,
                    block_timestamp,
                );

                let liquidity_net = if zero_for_one {
                    -liquidity_net
                } else {
                    liquidity_net
                };
                current_liquidity = liquidity_math::add_delta(current_liquidity, liquidity_net);
            }

            // Advance position
            current_sqrt_price = target_sqrt_price;
            if reached_tick {
                current_tick = if zero_for_one {
                    tick_next - 1
                } else {
                    tick_next
                };
            } else {
                current_tick = tick_math::get_tick_at_sqrt_ratio(current_sqrt_price);
            }
        }

        // Write oracle observation
        if current_tick != slot0_start.tick {
            let (obs_idx, obs_card) = oracle::write(
                &mut self.observations,
                slot0_start.observation_index,
                block_timestamp,
                slot0_start.tick,
                self.liquidity,
                slot0_start.observation_cardinality,
                slot0_start.observation_cardinality_next,
            );
            self.slot0.observation_index = obs_idx;
            self.slot0.observation_cardinality = obs_card;
        }

        // Set final state from event
        self.slot0.sqrt_price_x96 = event_sqrt_price_x96;
        self.slot0.tick = event_tick;
        self.slot0.unlocked = true;
        self.liquidity = event_liquidity;

        // Update fee growth and protocol fees
        if zero_for_one {
            self.fee_growth_global_0_x128 = fee_growth_global;
            if total_protocol_fee > 0 {
                self.protocol_fees.token0 =
                    self.protocol_fees.token0.wrapping_add(total_protocol_fee);
            }
        } else {
            self.fee_growth_global_1_x128 = fee_growth_global;
            if total_protocol_fee > 0 {
                self.protocol_fees.token1 =
                    self.protocol_fees.token1.wrapping_add(total_protocol_fee);
            }
        }
    }

    // ── flash ──────────────────────────────────────────────────────────────

    /// Flash loan fee accrual. No actual token transfer — just updates fee growth.
    /// `amount0` and `amount1` are the flash-borrowed amounts; fees are computed
    /// as `ceil(amount * fee / 1e6)` and assumed to be paid in full.
    pub fn flash(&mut self, amount0: U256, amount1: U256, _block_timestamp: u32) {
        assert!(self.slot0.unlocked, "LOK");
        let _liquidity = self.liquidity;
        assert!(_liquidity > 0, "L");

        let one_million = U256::from(1_000_000u64);
        let fee_u = U256::from(self.fee);

        let fee0 = full_math::mul_div_rounding_up(amount0, fee_u, one_million);
        let fee1 = full_math::mul_div_rounding_up(amount1, fee_u, one_million);

        // Assume fees are paid in full (paid0 = fee0, paid1 = fee1).
        if fee0 > U256::ZERO {
            let fee_protocol0 = self.slot0.fee_protocol % 16;
            let fees0 = if fee_protocol0 == 0 {
                U256::ZERO
            } else {
                fee0 / U256::from(fee_protocol0)
            };
            if fees0.to::<u128>() > 0 {
                self.protocol_fees.token0 =
                    self.protocol_fees.token0.wrapping_add(fees0.to::<u128>());
            }
            self.fee_growth_global_0_x128 = self.fee_growth_global_0_x128.wrapping_add(
                full_math::mul_div(fee0 - fees0, FIXED_POINT_128_Q128, U256::from(_liquidity)),
            );
        }
        if fee1 > U256::ZERO {
            let fee_protocol1 = self.slot0.fee_protocol >> 4;
            let fees1 = if fee_protocol1 == 0 {
                U256::ZERO
            } else {
                fee1 / U256::from(fee_protocol1)
            };
            if fees1.to::<u128>() > 0 {
                self.protocol_fees.token1 =
                    self.protocol_fees.token1.wrapping_add(fees1.to::<u128>());
            }
            self.fee_growth_global_1_x128 = self.fee_growth_global_1_x128.wrapping_add(
                full_math::mul_div(fee1 - fees1, FIXED_POINT_128_Q128, U256::from(_liquidity)),
            );
        }
    }

    /// Flash replay using actual paid amounts from the event.
    /// On-chain, fee growth is based on `paid0`/`paid1` (what the flasher actually paid),
    /// not the computed minimum fee. Using the event's paid values reproduces exact fee growth.
    pub fn flash_with_paid(&mut self, paid0: U256, paid1: U256) {
        assert!(self.slot0.unlocked, "LOK");
        let liq = self.liquidity;
        assert!(liq > 0, "L");

        if paid0 > U256::ZERO {
            let fee_protocol0 = self.slot0.fee_protocol % 16;
            let fees0 = if fee_protocol0 == 0 {
                U256::ZERO
            } else {
                paid0 / U256::from(fee_protocol0)
            };
            if fees0 > U256::ZERO {
                let f = fees0.as_limbs()[0] as u128 | ((fees0.as_limbs()[1] as u128) << 64);
                self.protocol_fees.token0 = self.protocol_fees.token0.wrapping_add(f);
            }
            self.fee_growth_global_0_x128 = self.fee_growth_global_0_x128.wrapping_add(
                full_math::mul_div(paid0 - fees0, FIXED_POINT_128_Q128, U256::from(liq)),
            );
        }
        if paid1 > U256::ZERO {
            let fee_protocol1 = self.slot0.fee_protocol >> 4;
            let fees1 = if fee_protocol1 == 0 {
                U256::ZERO
            } else {
                paid1 / U256::from(fee_protocol1)
            };
            if fees1 > U256::ZERO {
                let f = fees1.as_limbs()[0] as u128 | ((fees1.as_limbs()[1] as u128) << 64);
                self.protocol_fees.token1 = self.protocol_fees.token1.wrapping_add(f);
            }
            self.fee_growth_global_1_x128 = self.fee_growth_global_1_x128.wrapping_add(
                full_math::mul_div(paid1 - fees1, FIXED_POINT_128_Q128, U256::from(liq)),
            );
        }
    }

    // ── increase_observation_cardinality_next ───────────────────────────────

    pub fn increase_observation_cardinality_next(&mut self, observation_cardinality_next: u16) {
        assert!(self.slot0.unlocked, "LOK");

        let old = self.slot0.observation_cardinality_next;
        let new = oracle::grow(&mut self.observations, old, observation_cardinality_next);
        self.slot0.observation_cardinality_next = new;
    }

    // ── set_fee_protocol ───────────────────────────────────────────────────

    pub fn set_fee_protocol(&mut self, fee_protocol0: u8, fee_protocol1: u8) {
        assert!(self.slot0.unlocked, "LOK");
        assert!(
            (fee_protocol0 == 0 || (fee_protocol0 >= 4 && fee_protocol0 <= 10))
                && (fee_protocol1 == 0 || (fee_protocol1 >= 4 && fee_protocol1 <= 10)),
            "invalid fee protocol"
        );
        self.slot0.fee_protocol = fee_protocol0 + (fee_protocol1 << 4);
    }
}
