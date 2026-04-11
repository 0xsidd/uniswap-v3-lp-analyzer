# Uniswap V3 LP Analyzer

A high-fidelity Uniswap V3 LP strategy backtester written in Rust. Replays real on-chain events through a bit-for-bit accurate pool replica to simulate LP strategies with real minting, rebalancing, and fee tracking.

## Architecture

```
                          MongoDB
                            |
     config.toml ──> event-catcher ──> events collection
                                            |
                                            v
     replay_config.toml ──> replay ──> verify pool state (PASS/FAIL)
                                            |
                                            v
     sim_config.toml ──> simulator ──> backtest results + charts
```

The project has 6 crates:

| Crate | Type | Purpose |
|-------|------|---------|
| `config` | lib | Config file loader |
| `common` | lib | Event parsing, retry logic, progress bars |
| `v3-pool` | lib | Uniswap V3 pool state machine (exact Solidity replica) |
| `event-catcher` | bin | Index on-chain pool events to MongoDB |
| `replay` | bin | Verify replay accuracy against on-chain state |
| `simulator` | bin | Backtest LP strategies with real position minting |

## Prerequisites

- Rust (stable)
- MongoDB running locally on port 27017
- An RPC endpoint (Alchemy, Infura, etc.) for the target chain

## Quick Start

### 1. Configure environment

Create `.env` in the project root:

```env
RPC_URL=https://arb-mainnet.g.alchemy.com/v2/YOUR_KEY
```

### 2. Index events

Edit `config.toml`:

```toml
pool_address = "0xC6962004f452bE9203591991D15f6b388e09E8D0"
from_block = 99174111
to_block = 450573739
batch_size = 10000
mongo_uri = "mongodb://localhost:27017"
db_name = "azoth_simulation"
```

Run the event catcher:

```bash
cargo run --release -p event-catcher
```

This indexes all pool events (Swap, Mint, Burn, Collect, Flash, etc.) into MongoDB. It's resumable — run it again to continue from where it stopped.

### 3. Verify replay accuracy

Edit `crates/replay/replay_config.toml`:

```toml
pool_address = "0xC6962004f452bE9203591991D15f6b388e09E8D0"
from_block = 99174111
verify_at_block = 150174111
mongo_uri = "mongodb://localhost:27017"
db_name = "azoth_simulation"
fee = 500
tick_spacing = 10
```

```bash
cargo run --release -p replay
```

Expected output:
```
--- Comparison ---
  sqrtPriceX96:          [PASS]
  tick:                  [PASS]
  liquidity:             [PASS]
  feeGrowthGlobal0X128:  [PASS]
  feeGrowthGlobal1X128:  [PASS]
  feeProtocol:           [PASS]

All core state fields match on-chain state!
```

### 4. Run a backtest

Edit `crates/simulator/sim_config.toml`:

```toml
pool_address = "0xC6962004f452bE9203591991D15f6b388e09E8D0"
mongo_uri = "mongodb://localhost:27017"
db_name = "azoth_simulation"
fee = 500
tick_spacing = 10

genesis_block = 99174111
from_block = 170084110
to_block = 220084110

deposit_weth = "1000000000000000000"

token0_decimals = 18
token1_decimals = 6
token0_symbol = "WETH"
token1_symbol = "USDC"

wide_range_pct = 70
base_range_pct = 35
limit_order_pct = 10
wide_alloc_pct = 10
rebalance_price_pct = 20
rebalance_interval_blocks = 345600

write_csv = false
```

```bash
cargo run --release -p simulator
```

This produces a report like:

```
  Duration:         50000000 blocks (~144.7 days)
  Rebalances:       148

  WETH price:       $2553.37 → $3688.94
  Fees earned:      $1061.76
  vs HODL 1 WETH:   -$60.33
  Overall PnL:      +$1075.24 (42.11%)
  Fee return:       41.58%

  Price Divergence:
    Avg: 0.00 bps
    Max: 0.31 bps
```

Plus three charts saved to `crates/simulator/`:
- `chart_fees.png` — Cumulative fees over time
- `chart_position_value.png` — Net position value
- `chart_fee_return.png` — Fee return percentage

---

## How the Simulation Works

### Overview

The simulator answers the question: "If I had deployed this LP strategy at block X, what would my returns be at block Y?" It does this by:

1. Building a **full Rust replica** of the Uniswap V3 pool (bit-for-bit identical to Solidity)
2. Replaying every historical event from MongoDB into this replica
3. **Actually minting** your LP positions into the pool (modifying its liquidity distribution)
4. Letting the pool's own swap math compute your fee earnings from every trade

### Phase 1: Exact Warmup (genesis_block → from_block)

**Goal:** Reconstruct the pool's exact state at `from_block`.

Every event from pool creation to your entry block is replayed with **state correction**. After each swap, the pool's `sqrtPriceX96`, `tick`, and `liquidity` are overwritten with the on-chain event values:

```rust
pool.swap(zero_for_one, amount, sqrt_price_limit, timestamp);
pool.slot0.sqrt_price_x96 = event.sqrtPriceX96;  // force correct
pool.slot0.tick = event.tick;                      // force correct
pool.liquidity = event.liquidity;                  // force correct
```

This is necessary because during warmup we're building the pool's tick/position/fee state — any tiny floating-point drift would compound over millions of events. State correction ensures the pool at `from_block` is **bit-for-bit identical** to the real on-chain pool.

**Why not just start from `from_block`?** Because the pool's internal state (tick bitmap, fee growth per tick, position accounting) has no single-block snapshot API. You must replay from genesis to build it.

### Phase 2: Deploy Strategy (at from_block)

**Goal:** Mint your LP positions into the pool.

The user provides 1 WETH. The simulator borrows an equal USD value of USDC to create a 50/50 portfolio. This models the real-world setup where you'd swap half your capital.

**Position deployment follows the passive rebalancing strategy:**

```
Active Pool = 2 × min(WETH_value, USDC_value)

Wide  = 10% of Active Pool, range: [P/1.70, P×1.70]   (±70%, geometric)
Base  = 90% of Active Pool, range: [P/1.35, P×1.35]   (±35%, geometric)
Limit = Remaining tokens, deployed as single-sided limit orders
```

**Geometric ranges** (dividing/multiplying by the factor) are used instead of arithmetic (subtracting/adding) because Uniswap V3 operates in log-price (tick) space. Geometric ranges are symmetric in tick space, which produces a ~50/50 token split. Arithmetic ranges (`P × 0.30` to `P × 1.70`) create asymmetric tick distances that demand far more of one token than the other.

**Liquidity calculation:** For each range, a reference liquidity is used to compute the USD value per unit of liquidity at the current price. The target liquidity is then `budget / usd_per_unit_liquidity`.

**Limit orders:** After deploying wide + base, any remaining WETH goes into a single-sided position above the current price (sell-the-rip), and any remaining USDC goes below (buy-the-dip). Two separate limit positions (`SIM_LIMIT` and `SIM_LIMIT_B`) ensure no tokens are left undeployed.

### Phase 3: Volume-Based Replay (from_block → to_block)

**Goal:** Simulate how your positions would have performed against real market activity.

Events are replayed **without state correction**. The pool runs its own math with your positions included.

**Swap handling — the critical detail:**

```rust
// Use event's final price as the swap limit
let lim = event_sqrtPriceX96;
pool.swap(zero_for_one, input_amount, lim, timestamp);
```

The swap uses the same **input volume** from the on-chain event, but caps the price movement at the **on-chain event's final price**. This is essential because:

- Our pool has **extra liquidity** (your LP positions). More liquidity means the same input volume moves the price less.
- Without the cap, the same input would be fully consumed but would barely move the price — our pool would diverge from reality.
- With the cap, the swap stops at the real pool's final price. Any excess input volume that our extra liquidity would have absorbed is discarded. This models reality: that extra liquidity reduces slippage for traders, and the portion of volume our positions capture is accurately reflected.

**Result:** Pool price stays aligned with on-chain (< 1 bps divergence), and our positions earn their proportional share of fees.

**Mint/Burn:** The `liquidity` value from the event is used directly (not token amounts). This preserves the exact liquidity distribution of all other LPs.

**Price divergence** is tracked at every swap: `|(simulated_price - onchain_price) / onchain_price|`. With the price-capped swap approach, this is typically < 0.5 bps.

### Phase 4: Exit

All positions are burned and tokens collected. The exit price is fetched from on-chain RPC for accurate USD valuation.

PnL calculation:
```
Position Total = recovered_WETH × exit_price + recovered_USDC
Net to User    = Position Total - Borrowed USDC
Overall PnL    = Net to User - Initial Capital (1 WETH × entry_price)
vs HODL        = Net to User - (1 WETH × exit_price)
```

### Rebalancing

During Phase 3, the simulator checks three conditions after each event:

1. **Price threshold:** Has the pool price moved more than `rebalance_price_pct` (default 20%) from the last rebalance?
2. **Limit imbalance:** Is the limit order > 25% of the portfolio AND price moved at least 5%? (The 5% gate prevents churning when the portfolio is structurally one-sided.)
3. **Time interval:** Have `rebalance_interval_blocks` (default 345,600 = ~1 day on Arbitrum) blocks passed?

Guard: no rebalance within the same block (`blocks_since > 0`).

When a rebalance triggers:
1. Pending fees are captured from `feeGrowthInside` (accumulated into `cumulative_fees_t0/t1`)
2. All 4 positions (wide, base, limit, limit_b) are burned and tokens collected
3. Recovered tokens (principal + fees) are redeployed with the same 10/90 wide/base split, centered on the current pool price
4. Any token remainders go into new limit orders

### Fee Tracking

Fees are tracked via the pool's `feeGrowthInside` mechanism — the same math Solidity uses:

```
feeGrowthInside = feeGrowthGlobal - feeGrowthBelow - feeGrowthAbove

pending_fees = (feeGrowthInside - position.feeGrowthInsideLast) * position.liquidity / 2^128
```

This is:
- **Price-independent** — fees accumulate regardless of token price
- **Monotonically increasing** — fees can never decrease
- **Exact** — same math as on-chain Solidity contract

Fees are captured at two points:
- Before each rebalance (so they're not lost when positions are burned)
- At final exit

### Inside a V3 Swap — How Your Positions Earn Fees

When `pool.swap()` executes, it runs a tick-by-tick loop:

1. Find the next initialized tick in the swap direction (via tick bitmap)
2. Compute how much input is needed to reach that tick (or the price limit)
3. `compute_swap_step()`: consume the lesser of (remaining input, input to next tick)
4. Compute fee: `fee = input_consumed × fee_rate / (1 - fee_rate)`
5. Accumulate: `feeGrowthGlobal += fee × 2^128 / active_liquidity`
6. If the tick boundary is reached, call `tick.cross()` which adds/subtracts `liquidityNet` from active liquidity
7. Repeat until input is exhausted or price limit is reached

Your positions earn fees at **step 5**. The fee for each micro-step is divided by `active_liquidity` — the total liquidity from all LPs (including you) whose ranges contain the current tick. Your share is `your_liquidity / active_liquidity` for each step where you're in range.

With the price cap (`lim = event_sp`), the swap may stop before consuming all input. This means slightly less total fee is collected per swap than in the real pool. However, the fee per unit of liquidity (`feeGrowthGlobal` increment) is accurate because both the fee and the liquidity denominator reflect the real pool state at that price.

### Key Approximations and Limitations

1. **Price-capped swaps discard excess volume.** In reality, our extra liquidity would absorb more of the swap, reducing slippage for the trader. The discarded volume represents a slight fee undercount — we're being conservative. For a 1 WETH position in a multi-billion dollar pool, the extra liquidity is negligible and this approximation is very accurate.

2. **Other LPs' reactions are not modeled.** In reality, if our extra liquidity changes the pool dynamics, other LPs might adjust their positions differently. We replay their exact historical mint/burn/collect actions regardless.

3. **MEV is not modeled.** Sandwich attacks, JIT liquidity, and other MEV strategies that occur in real blocks are replayed as-is. Our positions would be subject to these in reality but the sim treats them as regular events.

4. **Rebalance gas costs are not deducted.** On Arbitrum (~$0.10-$0.50 per rebalance), 148 rebalances would cost ~$15-$75, which is small relative to position size but nonzero.

5. **Block timestamps are synthetic.** The sim increments a counter per new block number for the pool's timestamp field, not using real block timestamps. This only affects oracle observations (TWAP), which the strategy doesn't use.

---

## The LP Strategy: Passive Rebalancing

The simulator implements a **zero-swap passive rebalancing strategy** based on a Wide / Base / Limit architecture. See `rabalance.md` for the full specification.

### Core Principle

Never swap tokens explicitly. Deploy what can be balanced 50/50 into earning positions, and route surplus tokens into single-sided limit orders that act as natural mean-reversion trades.

### Position Layout

| Position | Capital Share | Range | Role |
|----------|-------------|-------|------|
| Wide | 10% | ±70% (geometric) | Safety net, always-earning buffer |
| Base | 90% | ±35% (geometric) | Primary fee generator |
| Limit A | Excess token | ±10%, single-sided | Passive rebalance order |
| Limit B | Non-excess remainder | ±10%, single-sided opposite | Prevents token leakage |

### How Passive Rebalancing Works

1. At deployment, both tokens are balanced 50/50 by USD value
2. As price moves, V3 naturally converts tokens within the positions
3. At rebalance, recovered tokens may be imbalanced (e.g., more USDC than WETH after a price rise)
4. The balanced portion goes into Wide + Base; the excess goes into a Limit order
5. If price reverses, the Limit order fills — passively rebalancing back toward 50/50
6. No DEX swap needed — zero swap fees, zero slippage, zero MEV exposure

---

## Adapting for Different Chains

### Supported Chains

Any EVM chain with Uniswap V3 (or fork) deployments:
- Ethereum mainnet
- Arbitrum
- Optimism
- Polygon
- Base
- BSC (PancakeSwap V3)

### Steps

1. **Set RPC** in `.env`:
   ```env
   RPC_URL=https://mainnet.infura.io/v3/YOUR_KEY
   ```

2. **Update `config.toml`** with pool address and block range:
   ```toml
   pool_address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"  # ETH/USDC on mainnet
   from_block = 12370000
   to_block = 19000000
   ```

3. **Update `sim_config.toml`**:
   ```toml
   pool_address = "0x8ad599c3A0ff1De082011EFDDc58f1908eb6e6D8"
   fee = 3000          # 0.30% fee tier
   tick_spacing = 60   # Corresponds to 0.30% fee
   
   token0_decimals = 18   # WETH
   token1_decimals = 6    # USDC
   token0_symbol = "WETH"
   token1_symbol = "USDC"
   ```

4. **Adjust block time** for duration calculations:
   - Arbitrum: ~0.25s/block
   - Ethereum: ~12s/block
   - Polygon: ~2s/block

   Currently hardcoded as `0.25` in the simulator. Search for `0.25` in `main.rs` to change.

### Fee Tiers and Tick Spacing

| Fee Tier | Fee (bps) | Tick Spacing | Typical Pairs |
|----------|-----------|--------------|---------------|
| 100 | 0.01% | 1 | Stablecoin pairs |
| 500 | 0.05% | 10 | Most pairs |
| 3000 | 0.30% | 60 | Volatile pairs |
| 10000 | 1.00% | 200 | Exotic pairs |

Set `fee` and `tick_spacing` in your config to match the pool.

---

## Writing Custom Strategies

The strategy logic lives in the `deploy_positions` function in `crates/simulator/src/main.rs`. To create a custom strategy:

### 1. Define Tick Ranges

Ranges use geometric (multiplicative) factors for tick symmetry:

```rust
let factor = 1.0 + range_pct / 100.0;
let tick_lower = price_to_tick(price / factor, ...);
let tick_upper = price_to_tick(price * factor, ...);
```

You could instead:
- Use fixed tick ranges
- Base ranges on historical volatility
- Use asymmetric ranges (wider below, tighter above)
- Add more than 4 positions

### 2. Customize Allocation

Currently allocation is by target USD value:

```rust
let usd_per_liq = |tl, tu| { /* compute USD per unit liquidity */ };
let wide_liq = (wide_budget / wide_upl) as u128;
let base_liq = (base_budget / base_upl) as u128;
```

### 3. Customize Rebalance Triggers

Current triggers (in the Phase 3 loop):

```rust
let should_rebalance = price_move >= cfg.rebalance_price_pct
    || (limit_pct_of_tv >= 25.0 && price_move >= 5.0)
    || blocks_since >= cfg.rebalance_interval_blocks;
```

### 4. Add New Position Types

The simulator uses sentinel addresses for each position:

```rust
const SIM_WIDE:    Address = Address::new([..., 0x01, 0xDE, 0xAD, 0xBE, 0xEF]);
const SIM_BASE:    Address = Address::new([..., 0x02, 0xDE, 0xAD, 0xBE, 0xEF]);
const SIM_LIMIT:   Address = Address::new([..., 0x03, 0xDE, 0xAD, 0xBE, 0xEF]);
const SIM_LIMIT_B: Address = Address::new([..., 0x04, 0xDE, 0xAD, 0xBE, 0xEF]);
```

Add more addresses for additional positions. Each position is independently tracked for fees, burn, and rebalance.

---

## The v3-pool Crate

A complete Rust replica of the Uniswap V3 pool contract. Verified to produce **bit-for-bit identical state** to on-chain Solidity over 1M+ events.

### Modules

| Module | Solidity Equivalent | Key Functions |
|--------|-------------------|---------------|
| `math/full_math` | FullMath.sol | `mul_div`, `mul_div_rounding_up` (512-bit precision) |
| `math/tick_math` | TickMath.sol | `get_sqrt_ratio_at_tick`, `get_tick_at_sqrt_ratio` |
| `math/sqrt_price_math` | SqrtPriceMath.sol | `get_amount0_delta`, `get_amount1_delta`, `get_next_sqrt_price_from_input` |
| `math/swap_math` | SwapMath.sol | `compute_swap_step` |
| `math/liquidity_math` | LiquidityMath.sol | `add_delta` |
| `tick` | Tick.sol | `update`, `cross`, `get_fee_growth_inside` |
| `tick_bitmap` | TickBitmap.sol | `flip_tick`, `next_initialized_tick_within_one_word` |
| `position` | Position.sol | `get`, `update` (fee accrual) |
| `oracle` | Oracle.sol | `initialize`, `write`, `observe` (TWAP) |
| `pool` | UniswapV3Pool.sol | `initialize`, `mint`, `burn`, `swap`, `collect`, `flash` |

### Important: Do Not Modify

The `v3-pool` crate is verified against on-chain state. Any modification will break the replay verification. If you need custom pool behavior, extend it in the simulator crate instead.

---

## Key Concepts

### sqrtPriceX96

The pool stores price as `sqrt(price) * 2^96` in Q64.96 fixed-point format.

```
Human price = (sqrtPriceX96 / 2^96)^2 * 10^(token0_decimals - token1_decimals)
```

For WETH/USDC (18/6 decimals): multiply by `10^12`.

### Ticks

Each tick represents a 0.01% price change: `price = 1.0001^tick`.

```
tick -201000 ~ $1850 WETH/USDC
tick -200000 ~ $2040 WETH/USDC
```

Ticks must be aligned to `tick_spacing` (10 for 0.05% fee tier).

### Fee Growth

Fees are tracked as cumulative growth per unit of liquidity in Q128 format:

```
feeGrowthGlobal += (feeAmount * 2^128) / activeLiquidity
```

Position fees = `(feeGrowthInside_now - feeGrowthInside_last) * position_liquidity / 2^128`

This is monotonically increasing and price-independent — the correct way to measure LP fee earnings.

---

## Project Structure

```
sim-rust/
├── .env                          # RPC endpoint (gitignored)
├── .gitignore
├── Cargo.toml                    # Workspace manifest
├── config.toml                   # Event-catcher config
├── rabalance.md                  # Passive rebalancing strategy spec
├── crates/
│   ├── config/                   # Config loader library
│   ├── common/                   # Shared: events, retry, progress
│   ├── v3-pool/                  # Uniswap V3 pool replica
│   │   └── src/
│   │       ├── math/             # FullMath, TickMath, SqrtPriceMath, SwapMath
│   │       ├── tick.rs           # Tick state management
│   │       ├── tick_bitmap.rs    # Initialized tick tracking
│   │       ├── position.rs       # LP position accounting
│   │       ├── oracle.rs         # TWAP oracle
│   │       └── pool.rs          # Main pool state machine
│   ├── event-catcher/            # Binary: index events to MongoDB
│   ├── replay/                   # Binary: verify replay accuracy
│   │   └── replay_config.toml
│   └── simulator/                # Binary: LP strategy backtester
│       ├── sim_config.toml
│       ├── chart_fees.png        # Generated
│       ├── chart_position_value.png
│       └── chart_fee_return.png
```

## License

Internal use.
