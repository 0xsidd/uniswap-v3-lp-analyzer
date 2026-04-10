# Passive Rebalancing for Uniswap V3: Wide / Base / Limit Architecture

## Overview

This document describes a **zero-swap passive rebalancing strategy** for concentrated liquidity on Uniswap V3. The strategy deploys capital across three positions:

| Position | Share of Active Liquidity | Range Width | Role |
|----------|--------------------------|-------------|------|
| **Wide** | 10% | ±70% from current price | Safety net / always-earning buffer |
| **Base** | 90% | ±35% from current price | Primary fee generator |
| **Limit** | Excess tokens | ±10%, single-sided | Passive rebalance order |

**Core principle:** Never swap tokens explicitly. Instead, deploy whatever can be balanced 50/50 (in USD terms) into Wide + Base, and route the surplus token into a single-sided Limit order that acts as a natural mean-reversion trade.

---

## 1. Range Definitions

Given a current price `P`:

```
Wide_lower  = P × (1 - 0.70) = 0.30 × P
Wide_upper  = P × (1 + 0.70) = 1.70 × P

Base_lower  = P × (1 - 0.35) = 0.65 × P
Base_upper  = P × (1 + 0.35) = 1.35 × P

Limit_lower / Limit_upper depends on which token is excess:
  If excess USDC → Limit is BELOW price: [P × 0.90, P]        (buy-the-dip)
  If excess WETH → Limit is ABOVE price: [P, P × 1.10]        (sell-the-rip)
```

### Why these widths?

- **Wide ±70%** covers large moves. Even in a 50% crash, your Wide position is still in range earning fees. It's thin (10%) so capital efficiency is low, but it's insurance.
- **Base ±35%** is where the bulk of fees come from. Tight enough to concentrate liquidity, wide enough to avoid constant out-of-range events.
- **Limit ±10%** is narrow and single-sided. It mimics a limit order on a CEX — when price crosses through it, the excess token gets converted to the deficit token passively via the AMM.

---

## 2. The Core Formula

### Inputs

```
P       = current price (USDC per WETH)
W       = WETH quantity held
U       = USDC quantity held
W_val   = W × P                          (WETH value in USDC terms)
TV      = W_val + U                       (total portfolio value in USDC)
```

### Step 1 — Identify the binding constraint

For a 50/50 deployment in each range, both tokens must be equal in USD value. The scarcer token limits how much can go into balanced positions.

```
Binding_val = min(W_val, U)
Active_Pool = 2 × Binding_val
```

### Step 2 — Split Active Pool into Wide and Base

```
Wide_val   = 0.10 × Active_Pool
Base_val   = 0.90 × Active_Pool
```

Each is split 50/50 internally:

```
Wide_WETH  = Wide_val / (2 × P)       Wide_USDC  = Wide_val / 2
Base_WETH  = Base_val / (2 × P)       Base_USDC  = Base_val / 2
```

### Step 3 — Limit order absorbs the excess

```
Excess = |W_val - U|

If W_val < U   →  Limit is USDC-only (below price), value = U - W_val
If W_val > U   →  Limit is WETH-only (above price), value = W_val - U
If W_val == U  →  Limit = $0, perfectly balanced
```

### Summary formula (single expression)

```
Wide  = 0.10 × 2 × min(W×P, U)
Base  = 0.90 × 2 × min(W×P, U)
Limit = |W×P - U|
Total = Wide + Base + Limit = W×P + U  ✓
```

---

## 3. Example 1: Fresh Deployment at $2,000

### Starting state

```
P = $2,000
W = 1.0 WETH
U = $2,000 USDC
W_val = $2,000
TV = $4,000
```

### Calculation

```
Binding   = min($2,000, $2,000) = $2,000
Active    = 2 × $2,000 = $4,000
Wide_val  = 0.10 × $4,000 = $400
Base_val  = 0.90 × $4,000 = $3,600
Excess    = |$2,000 - $2,000| = $0
```

### Ranges at P = $2,000

```
Wide:  [$600, $3,400]        (0.30×2000 to 1.70×2000)
Base:  [$1,300, $2,700]      (0.65×2000 to 1.35×2000)
Limit: not needed
```

### Position breakdown

| Range | Value | WETH ($) | USDC ($) | WETH qty |
|-------|-------|----------|----------|----------|
| Wide  | $400  | $200 (50%) | $200 (50%) | 0.100 |
| Base  | $3,600 | $1,800 (50%) | $1,800 (50%) | 0.900 |
| Limit | $0 | — | — | — |
| **Total** | **$4,000** | **$2,000** | **$2,000** | **1.000** |

Perfectly balanced. No limit order needed.

---

## 4. Example 2: Price Rises to $2,500 (Rebalance Triggered)

### What happens inside the positions as price rises

As ETH price rises from $2,000 → $2,500, Uniswap V3 automatically sells WETH for USDC within both the Wide and Base ranges. The tighter the range, the more aggressively it sells. After the move, your holdings shift.

### Post-move state (approximate, accounting for IL + fees)

```
P = $2,500
W = 0.80 WETH       (sold 0.20 WETH as price rose)
U = $2,400 USDC     (received USDC from sales + fee income)
W_val = 0.80 × $2,500 = $2,000
TV = $4,400
```

### Calculation

```
Binding   = min($2,000, $2,400) = $2,000
Active    = 2 × $2,000 = $4,000
Wide_val  = 0.10 × $4,000 = $400
Base_val  = 0.90 × $4,000 = $3,600
Excess    = $2,400 - $2,000 = $400 USDC  →  USDC-only limit below price
```

### New ranges at P = $2,500

```
Wide:  [$750, $4,250]
Base:  [$1,625, $3,375]
Limit: [$2,250, $2,500]     (single-sided USDC, buy-the-dip)
```

### Position breakdown after rebalance

| Range | Value | WETH ($) | USDC ($) | WETH qty |
|-------|-------|----------|----------|----------|
| Wide  | $400 | $200 (50%) | $200 (50%) | 0.080 |
| Base  | $3,600 | $1,800 (50%) | $1,800 (50%) | 0.720 |
| Limit | $400 | $0 | $400 (100%) | — |
| **Total** | **$4,400** | **$2,000** | **$2,400** | **0.800** |

### What the limit order does

The $400 USDC sitting in the [$2,250, $2,500] range is a **buy order**. If price drops back below $2,500 and moves through this range, the AMM converts that USDC into WETH at prices between $2,250–$2,500. This passively rebalances you back toward 50/50 without any explicit swap.

---

## 5. Example 3: Price Drops to $1,500 (Opposite Direction)

### Starting from Example 1's deployment at $2,000

As price falls, Uniswap sells USDC for WETH (you accumulate more WETH).

### Post-move state

```
P = $1,500
W = 1.30 WETH       (accumulated WETH as price fell)
U = $1,450 USDC     (sold USDC into the pool + fees)
W_val = 1.30 × $1,500 = $1,950
TV = $3,400
```

### Calculation

```
Binding   = min($1,950, $1,450) = $1,450
Active    = 2 × $1,450 = $2,900
Wide_val  = 0.10 × $2,900 = $290
Base_val  = 0.90 × $2,900 = $2,610
Excess    = $1,950 - $1,450 = $500 WETH  →  WETH-only limit above price
```

### New ranges at P = $1,500

```
Wide:  [$450, $2,550]
Base:  [$975, $2,025]
Limit: [$1,500, $1,650]     (single-sided WETH, sell-the-rip)
```

### Position breakdown

| Range | Value | WETH ($) | USDC ($) | WETH qty |
|-------|-------|----------|----------|----------|
| Wide  | $290 | $145 (50%) | $145 (50%) | 0.097 |
| Base  | $2,610 | $1,305 (50%) | $1,305 (50%) | 0.870 |
| Limit | $500 | $500 (100%) | $0 | 0.333 |
| **Total** | **$3,400** | **$1,950** | **$1,450** | **1.300** |

The Limit is now a **sell order** — 0.333 WETH sitting above current price. If price recovers into the $1,500–$1,650 range, that WETH gets sold for USDC, passively rebalancing back toward 50/50.

---

## 6. Example 4: Large Move — Price Doubles to $4,000

### Post-move state (starting from $2,000 deployment)

A large move causes more aggressive IL. Base range ($1,300–$2,700) goes fully out of range at $4,000 — all its WETH has been sold to USDC.

```
P = $4,000
W = 0.40 WETH       (significant WETH sold off)
U = $3,200 USDC     (accumulated large USDC balance)
W_val = 0.40 × $4,000 = $1,600
TV = $4,800
```

### Calculation

```
Binding   = min($1,600, $3,200) = $1,600
Active    = 2 × $1,600 = $3,200
Wide_val  = 0.10 × $3,200 = $320
Base_val  = 0.90 × $3,200 = $2,880
Excess    = $3,200 - $1,600 = $1,600 USDC
```

### New ranges at P = $4,000

```
Wide:  [$1,200, $6,800]
Base:  [$2,600, $5,400]
Limit: [$3,600, $4,000]     (single-sided USDC, buy-the-dip)
```

### Position breakdown

| Range | Value | WETH ($) | USDC ($) | WETH qty |
|-------|-------|----------|----------|----------|
| Wide  | $320 | $160 (50%) | $160 (50%) | 0.040 |
| Base  | $2,880 | $1,440 (50%) | $1,440 (50%) | 0.360 |
| Limit | $1,600 | $0 | $1,600 (100%) | — |
| **Total** | **$4,800** | **$1,600** | **$3,200** | **0.400** |

### Key observation

The Limit order is now **33% of the portfolio** ($1,600 / $4,800). This is the strategy screaming: "the portfolio is heavily imbalanced." The large USDC limit order below price is a massive buy-the-dip bet. If ETH retraces even slightly (into $3,600–$4,000), that USDC starts converting to WETH.

**Rule of thumb:** If Limit exceeds ~30% of portfolio, consider whether the move is structural (in which case you may want to manually swap some) or temporary (in which case the limit order does its job).

---

## 7. Example 5: Volatile Chop — $2,000 → $2,500 → $2,100

This shows how the strategy self-heals in choppy markets.

### Phase A: Deploy at $2,000

Same as Example 1. Perfect 50/50, no limit order.

### Phase B: Price rises to $2,500

Same as Example 2:

```
W = 0.80 WETH, U = $2,400
Limit = $400 USDC below price [$2,250, $2,500]
```

### Phase C: Price drops back to $2,100

The Limit order at [$2,250, $2,500] gets **partially filled** as price passes through it on the way down. Say ~60% fills:

```
Limit filled: $240 USDC → 0.102 WETH (avg price ~$2,350)
Remaining limit: $160 USDC unfilled

New holdings:
W = 0.80 + 0.102 = 0.902 WETH
U = $2,400 - $240 + fees ≈ $2,190 USDC  (some USDC also returned from Base/Wide)
```

Actual rebalance at $2,100:

```
P = $2,100
W_val = 0.902 × $2,100 = $1,894
TV = $1,894 + $2,190 = $4,084

Binding = min($1,894, $2,190) = $1,894
Active  = $3,788
Excess  = $2,190 - $1,894 = $296 USDC
```

| Range | Value | WETH ($) | USDC ($) | WETH qty |
|-------|-------|----------|----------|----------|
| Wide  | $378.8 | $189 | $189 | 0.090 |
| Base  | $3,409.2 | $1,705 | $1,705 | 0.812 |
| Limit | $296 | $0 | $296 | — |
| **Total** | **$4,084** | **$1,894** | **$2,190** | **0.902** |

Notice: The limit order shrank from $400 → $296 — the strategy **self-healed** during the chop. If price returns to $2,000, the remaining excess gets even smaller.

---

## 8. When to Trigger a Rebalance

The strategy is passive, but you still need to **withdraw and redeploy** positions periodically. Triggers:

### Price-based trigger

```
If current_price is outside [last_rebalance_price × 0.80, last_rebalance_price × 1.20]:
    → Rebalance
```

A ±20% move from last deployment price means the Base range (±35%) is getting close to one-sided. Rebalance to re-center.

### Imbalance-based trigger

```
If Limit_value / Total_value > 0.25:
    → Rebalance (portfolio is >25% idle in limit order)
```

### Time-based trigger (optional)

```
Every 24–72 hours, check if re-centering would materially change positions.
```

### Compound trigger (recommended)

```
Rebalance when ANY of:
  1. Price moved ±20% from last rebalance
  2. Limit > 25% of portfolio
  3. 72 hours elapsed AND limit > 10% of portfolio
```

---

## 9. Rebalance Gas Cost Analysis

Each rebalance involves 3 operations (on Arbitrum):

| Operation | Approx Gas (Arbitrum) |
|-----------|----------------------|
| Remove liquidity from all 3 positions | ~300K gas × 3 |
| Collect fees | ~100K gas × 3 |
| Mint new positions | ~400K gas × 3 |
| **Total** | ~2.4M gas ≈ $0.10–$0.50 |

On Arbitrum at ~0.1 gwei L2 gas + L1 calldata costs, each rebalance costs roughly **$0.10–$0.50**. This is negligible compared to daily fee income on any meaningful position size.

---

## 10. Tracking Rebalance Health

After each rebalance, record these metrics:

```
Rebalance #N at block B, price P:
  - Total Value:        TV
  - Active Deployed:    Active (% of TV)
  - Limit Size:         Limit (% of TV)
  - Limit Side:         USDC (below) or WETH (above)
  - WETH Held:          W
  - USDC Held:          U
  - Implied Drift:      |W_val - U| / TV × 100%
```

### Health indicators

| Metric | Healthy | Warning | Action Needed |
|--------|---------|---------|---------------|
| Limit % of TV | < 15% | 15–25% | > 25% |
| Active % of TV | > 85% | 75–85% | < 75% |
| Implied Drift | < 10% | 10–20% | > 20% |

---

## 11. Comparison: Passive Rebalance vs Active Swap Rebalance

| Aspect | Passive (This Strategy) | Active Swap |
|--------|------------------------|-------------|
| Swap cost | $0 | 0.05–0.30% per swap |
| Slippage | $0 | Variable, worse in thin pools |
| MEV risk | None | Sandwich attack possible |
| Capital efficiency | Lower (limit order is less productive) | Higher (all capital balanced) |
| Complexity | Low | Medium (swap router needed) |
| Rebalance frequency | Can be frequent | Must minimize due to costs |
| Best for | Choppy / mean-reverting markets | Strong trending markets |

---

## 12. Edge Cases

### Both tokens exactly equal

```
Limit = $0. All capital goes to Wide + Base. Ideal state.
```

### One token is zero

Example: 0 WETH, $5,000 USDC after a massive price pump.

```
W_val = $0
Binding = min($0, $5,000) = $0
Active = $0
Limit = $5,000 USDC (100% of portfolio)
```

The entire portfolio becomes a single-sided USDC limit order. No Wide or Base can be deployed. This is the **maximum drift** scenario — the strategy is fully waiting for a price reversal. At this point, manual intervention (swap) may be warranted.

### Price exits the Wide range

If price moves beyond ±70%, even the Wide position goes single-sided. All three positions become single-token. The strategy degrades to a pure limit-order-only state. This should trigger a forced rebalance re-centering.

---

## 13. Implementation Pseudocode

```python
def passive_rebalance(P, W, U):
    """
    P: current price (USDC per WETH)
    W: WETH quantity
    U: USDC quantity
    Returns: (wide, base, limit) position specs
    """
    W_val = W * P
    TV = W_val + U

    binding = min(W_val, U)
    active = 2 * binding

    wide_val = 0.10 * active
    base_val = 0.90 * active
    excess = abs(W_val - U)

    # Determine limit side
    if W_val < U:
        limit_side = "USDC"  # below price, buy-the-dip
        limit_lower = P * 0.90
        limit_upper = P
    elif W_val > U:
        limit_side = "WETH"  # above price, sell-the-rip
        limit_lower = P
        limit_upper = P * 1.10
    else:
        limit_side = None
        limit_lower = limit_upper = 0

    return {
        "wide": {
            "value": wide_val,
            "weth_qty": wide_val / (2 * P),
            "usdc_qty": wide_val / 2,
            "lower": P * 0.30,
            "upper": P * 1.70,
        },
        "base": {
            "value": base_val,
            "weth_qty": base_val / (2 * P),
            "usdc_qty": base_val / 2,
            "lower": P * 0.65,
            "upper": P * 1.35,
        },
        "limit": {
            "value": excess,
            "side": limit_side,
            "lower": limit_lower,
            "upper": limit_upper,
        },
        "stats": {
            "total_value": TV,
            "active_pct": active / TV * 100 if TV > 0 else 0,
            "limit_pct": excess / TV * 100 if TV > 0 else 0,
            "drift_pct": abs(W_val - U) / TV * 100 if TV > 0 else 0,
        }
    }
```

---

## 14. Quick Reference Table

For a portfolio of **$10,000** at various price drift levels:

| WETH:USDC Split | Wide | Base | Limit | Limit % | Health |
|-----------------|------|------|-------|---------|--------|
| 50:50 ($5K:$5K) | $1,000 | $9,000 | $0 | 0% | Perfect |
| 45:55 ($4.5K:$5.5K) | $900 | $8,100 | $1,000 | 10% | Healthy |
| 40:60 ($4K:$6K) | $800 | $7,200 | $2,000 | 20% | Warning |
| 35:65 ($3.5K:$6.5K) | $700 | $6,300 | $3,000 | 30% | Rebalance |
| 25:75 ($2.5K:$7.5K) | $500 | $4,500 | $5,000 | 50% | Critical |
| 0:100 ($0:$10K) | $0 | $0 | $10,000 | 100% | Manual swap |

---

## Summary

1. **Deploy** Wide (10%) and Base (90%) with equal WETH:USDC value in each
2. **Route excess** token into a single-sided Limit order (±10% from price)
3. **Let the AMM rebalance you** — limit orders fill naturally on reversals
4. **Re-center** when price drifts ±20% or limit exceeds 25% of portfolio
5. **Never swap** — the only gas cost is position management, not DEX fees

The strategy sacrifices some capital efficiency (the limit order earns less than balanced positions) in exchange for zero swap costs, zero slippage, and zero MEV exposure. In choppy markets with mean-reverting price action, the limit orders self-heal and the strategy outperforms active rebalancing approaches.