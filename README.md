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
to_block = 200084110

deposit_weth = "1000000000000000000"

token0_decimals = 18
token1_decimals = 6
token0_symbol = "WETH"
token1_symbol = "USDC"

wide_range_pct = 70
base_range_pct = 35
limit_order_pct = 10
wide_alloc_pct = 10
rebal_wide_alloc_pct = 50
rebalance_price_pct = 20
rebalance_interval_blocks = 345600
```

```bash
cargo run --release -p simulator
```

This produces a report like:

```
  Duration:         30000000 blocks (~86.8 days)
  Rebalances:       86

  WETH price:       $2553.37 → $3522.46
  Fees earned:      $29.01
  vs HODL 1 WETH:   +$29.81
  Fee return:       1.14%

  Price Divergence:
    Avg: 0.01 bps
    Max: 0.06 bps
```

Plus three charts saved to `crates/simulator/`:
- `chart_fees.png` — Cumulative fees over time
- `chart_position_value.png` — Net position value
- `chart_fee_return.png` — Fee return percentage

---

## How the Simulation Works

### Phase 1: Exact Warmup (genesis → from_block)

Replays every event with **state correction** — after each swap, the pool's `sqrtPriceX96`, `tick`, and `liquidity` are forced to match the on-chain event values. This ensures the pool state at `from_block` is bit-for-bit identical to on-chain.

### Phase 2: Deploy Strategy (at from_block)

Your LP positions are **actually minted** into the pool using `pool.mint()`. This modifies the pool's tick state (`liquidityGross`, `liquidityNet`) and adds your liquidity to the active range.

The default strategy deploys two positions:

- **Wide** (±70% from current price): Captures volume across a broad range. Gets 10% of capital at initial deployment.
- **Base** (±35% from current price): Concentrated for higher fee capture. Gets 90% of capital.

Liquidity is computed by target USD allocation:
```
usd_per_unit_liquidity = amounts_for_liquidity(1e12, sqrtPrice, tickLower, tickUpper) / 1e12
target_liquidity = target_usd_value / usd_per_unit_liquidity
```

If total token consumption exceeds the budget, both positions are scaled down proportionally.

### Phase 3: Volume-Based Replay (from_block → to_block)

Events are replayed **without state correction**. The pool runs naturally with your positions minted in.

**Swaps:** The same input volume from the on-chain event is fed into the pool. The pool computes its own output and resulting price using its own liquidity (which includes yours). Your positions earn their proportional share of fees from every swap that passes through their range.

```
On-chain:  3 WETH input → pool with 5e18 liquidity → price moves X bps
Simulated: 3 WETH input → pool with 5.000242e18 liquidity → price moves X-0.01 bps
```

**Mint/Burn:** The `liquidity` value from the event is used directly (not token amounts). This preserves the exact liquidity distribution of all other LPs.

**Price divergence** is tracked at every swap: `|(simulated_price - onchain_price) / onchain_price|`. For a 1 WETH position in a multi-billion dollar pool, this is typically < 0.1 bps.

### Phase 4: Exit

All positions are burned and tokens collected. The exit price is fetched from on-chain RPC for accurate USD valuation. Fees are computed from `feeGrowthInside` — the pool's internal fee accounting mechanism that is monotonically increasing and independent of price movement.

### Rebalancing

During Phase 3, the simulator checks two conditions after each event:

1. **Price threshold:** Has the price moved more than `rebalance_price_pct` (default 20%) from the last rebalance?
2. **Time interval:** Have `rebalance_interval_blocks` (default 345,600 = ~1 day on Arbitrum) blocks passed?

When either triggers:
1. Pending fees are captured from `feeGrowthInside` (accumulated into `cumulative_fees_t0/t1`)
2. All positions are burned and tokens collected
3. Recovered tokens (principal + fees) are redeployed at new tick ranges centered on the current price
4. At rebalance, allocation shifts to `rebal_wide_alloc_pct` (default 50/50)

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

Currently ranges are computed as ± percentage from current price:

```rust
let wide_tl = price_to_tick(price * (1.0 - cfg.wide_range_pct / 100.0), ...);
let wide_tu = price_to_tick(price * (1.0 + cfg.wide_range_pct / 100.0), ...);
```

You could instead:
- Use fixed tick ranges
- Base ranges on historical volatility
- Use asymmetric ranges (wider below, tighter above)
- Add more than 2 positions (3-leg, 4-leg strategies)

### 2. Customize Allocation

Currently allocation is by target USD value:

```rust
let usd_per_liq = |tl, tu| { /* compute USD per unit liquidity */ };
let wide_liq = (total_usd * wide_alloc / 100.0 / wide_upl) as u128;
let base_liq = (total_usd * (100.0 - wide_alloc) / 100.0 / base_upl) as u128;
```

You could allocate based on:
- Expected fee tier yield per range
- Capital efficiency ratios
- Risk-adjusted returns

### 3. Customize Rebalance Triggers

Current triggers (in the Phase 3 loop):

```rust
let price_move = ((onchain_price - last_rebal_price) / last_rebal_price).abs() * 100.0;
let blocks_since = bn.saturating_sub(last_rebal_block);

if price_move >= cfg.rebalance_price_pct || blocks_since >= cfg.rebalance_interval_blocks {
    // rebalance...
}
```

You could add:
- **IL threshold:** Rebalance when impermanent loss exceeds X%
- **Fee accumulation:** Rebalance when collected fees reach X USD
- **Volatility-based:** Tighter rebalance in volatile markets, wider in calm
- **Conditional:** Only rebalance in one direction (e.g., only widen on price drop)

### 4. Add New Position Types

The simulator uses sentinel addresses for each position:

```rust
const SIM_WIDE: Address = Address::new([..., 0x01, 0xDE, 0xAD, 0xBE, 0xEF]);
const SIM_BASE: Address = Address::new([..., 0x02, 0xDE, 0xAD, 0xBE, 0xEF]);
const SIM_LIMIT: Address = Address::new([..., 0x03, 0xDE, 0xAD, 0xBE, 0xEF]);
```

Add more addresses for additional positions. Each position is independently tracked for fees, burn, and rebalance.

### 5. Example: Simple Single-Range Strategy

```rust
fn deploy_positions(...) -> (...) {
    let tl = price_to_tick(price * 0.9, ...);  // -10%
    let tu = price_to_tick(price * 1.1, ...);  // +10%
    
    // All capital in one position
    let liq = (total_usd / usd_per_liq(tl, tu)) as u128;
    pool.mint(SIM_BASE, tl, tu, liq, ts);
    
    let base = SimPosition { owner: SIM_BASE, tick_lower: tl, tick_upper: tu, liquidity: liq };
    let wide = SimPosition { owner: SIM_WIDE, tick_lower: 0, tick_upper: 0, liquidity: 0 }; // unused
    (wide, base, None, idle_t0, idle_t1)
}
```

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
tick -201000 ≈ $1850 WETH/USDC
tick -200000 ≈ $2040 WETH/USDC
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
│   │       └── pool.rs           # Main pool state machine
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
