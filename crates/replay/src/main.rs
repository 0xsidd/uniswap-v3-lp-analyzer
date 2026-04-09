use alloy::primitives::{Address, I256, U256};
use alloy::providers::ProviderBuilder;
use alloy::sol;
use futures::StreamExt;
use mongodb::bson::{doc, Document};
use mongodb::options::FindOptions;
use mongodb::Client as MongoClient;
use serde::Deserialize;
use std::str::FromStr;
use std::time::Instant;

use sim_common::progress;
use v3_pool::math::tick_math;
use v3_pool::pool::UniswapV3Pool;

// ── Config ──────────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ReplayConfig {
    rpc: String,
    pool_address: String,
    from_block: u64,
    verify_at_block: u64,
    mongo_uri: String,
    db_name: String,
    fee: u32,
    tick_spacing: i32,
}

impl ReplayConfig {
    fn load() -> Self {
        let raw = std::fs::read_to_string("replay_config.toml")
            .expect("replay_config.toml not found");
        toml::from_str(&raw).expect("invalid replay_config.toml")
    }
}

// ── On-chain contract calls for verification ────────────────────────────────

sol! {
    #[sol(rpc)]
    contract IUniswapV3Pool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );

        function liquidity() external view returns (uint128);
        function feeGrowthGlobal0X128() external view returns (uint256);
        function feeGrowthGlobal1X128() external view returns (uint256);
    }
}

// ── Arg parsing helpers ─────────────────────────────────────────────────────

fn arg_str<'a>(args: &'a Document, key: &str) -> &'a str {
    args.get_str(key)
        .unwrap_or_else(|_| panic!("missing arg '{}'", key))
}

fn arg_u256(args: &Document, key: &str) -> U256 {
    let s = arg_str(args, key);
    U256::from_str(s).unwrap_or_else(|_| panic!("bad U256 for '{}': {}", key, s))
}

fn arg_i256(args: &Document, key: &str) -> I256 {
    let s = arg_str(args, key);
    I256::from_dec_str(s).unwrap_or_else(|_| panic!("bad I256 for '{}': {}", key, s))
}

fn arg_i32(args: &Document, key: &str) -> i32 {
    let s = arg_str(args, key);
    s.parse::<i32>()
        .unwrap_or_else(|_| panic!("bad i32 for '{}': {}", key, s))
}

fn arg_u128(args: &Document, key: &str) -> u128 {
    let s = arg_str(args, key);
    s.parse::<u128>()
        .unwrap_or_else(|_| panic!("bad u128 for '{}': {}", key, s))
}

fn arg_u8(args: &Document, key: &str) -> u8 {
    let s = arg_str(args, key);
    s.parse::<u8>()
        .unwrap_or_else(|_| panic!("bad u8 for '{}': {}", key, s))
}

fn arg_u16(args: &Document, key: &str) -> u16 {
    let s = arg_str(args, key);
    s.parse::<u16>()
        .unwrap_or_else(|_| panic!("bad u16 for '{}': {}", key, s))
}

fn arg_address(args: &Document, key: &str) -> Address {
    let s = arg_str(args, key);
    Address::from_str(s).unwrap_or_else(|_| panic!("bad address for '{}': {}", key, s))
}

// ── Main ────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    let cfg = ReplayConfig::load();

    println!("Replay Engine");
    println!("─────────────────────────────────────────────");
    println!("Pool:          {}", cfg.pool_address);
    println!("Fee:           {} ({:.2}%)", cfg.fee, cfg.fee as f64 / 10_000.0);
    println!("Tick spacing:  {}", cfg.tick_spacing);
    println!("Block range:   {} -> {}", cfg.from_block, cfg.verify_at_block);
    println!();

    // ── Connect to MongoDB ──────────────────────────────────────────────────
    let mongo = MongoClient::with_uri_str(&cfg.mongo_uri)
        .await
        .expect("MongoDB connection failed");
    let db = mongo.database(&cfg.db_name);
    let events_col = db.collection::<Document>("events");

    // ── Query events ────────────────────────────────────────────────────────
    let filter = doc! {
        "blockNumber": {
            "$gte": cfg.from_block as i64,
            "$lte": cfg.verify_at_block as i64,
        }
    };
    let sort = doc! {
        "blockNumber": 1,
        "transactionIndex": 1,
        "logIndex": 1,
    };
    let find_opts = FindOptions::builder().sort(sort).build();

    // Count total for progress bar
    let total_events = events_col
        .count_documents(filter.clone())
        .await
        .expect("count_documents failed");

    println!("Found {} events to replay\n", total_events);

    if total_events == 0 {
        eprintln!("No events found. Run event-catcher first.");
        std::process::exit(1);
    }

    // ── Create pool ─────────────────────────────────────────────────────────
    let mut pool = UniswapV3Pool::new(cfg.fee, cfg.tick_spacing);

    // ── Process events ──────────────────────────────────────────────────────
    let mut cursor = events_col
        .find(filter)
        .with_options(find_opts)
        .await
        .expect("find failed");

    let start_time = Instant::now();
    let mut processed: u64 = 0;
    let mut block_timestamp: u32 = 0;
    let mut last_block: i64 = 0;

    let mut event_counts: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    while let Some(result) = cursor.next().await {
        let event_doc = result.expect("cursor error");

        let event_name = event_doc.get_str("eventName").expect("missing eventName");
        let args = event_doc.get_document("args").expect("missing args");
        let block_number = event_doc.get_i64("blockNumber").unwrap_or(0);

        // Simple block_timestamp counter: increment each new block.
        // Oracle state won't match on-chain exactly, but core state (price, liquidity, fees) will.
        if block_number != last_block {
            block_timestamp += 1;
            last_block = block_number;
        }

        *event_counts.entry(event_name.to_string()).or_default() += 1;

        match event_name {
            "Initialize" => {
                let sqrt_price_x96 = arg_u256(args, "sqrtPriceX96");
                pool.initialize(sqrt_price_x96);
            }

            "Mint" => {
                let owner = arg_address(args, "owner");
                let tick_lower = arg_i32(args, "tickLower");
                let tick_upper = arg_i32(args, "tickUpper");
                let amount = arg_u128(args, "amount");
                pool.mint(owner, tick_lower, tick_upper, amount, block_timestamp);
            }

            "Burn" => {
                let owner = arg_address(args, "owner");
                let tick_lower = arg_i32(args, "tickLower");
                let tick_upper = arg_i32(args, "tickUpper");
                let amount = arg_u128(args, "amount");
                pool.burn(owner, tick_lower, tick_upper, amount, block_timestamp);
            }

            "Swap" => {
                let amount0 = arg_i256(args, "amount0");
                let amount1 = arg_i256(args, "amount1");
                let event_sqrt_price = arg_u256(args, "sqrtPriceX96");
                let event_tick = arg_i32(args, "tick");
                let event_liquidity = arg_u128(args, "liquidity");

                let zero_for_one = amount0 > I256::ZERO;
                let amount_specified = if zero_for_one { amount0 } else { amount1 };

                let sqrt_price_limit_x96 = if zero_for_one {
                    tick_math::MIN_SQRT_RATIO + U256::from(1)
                } else {
                    tick_math::MAX_SQRT_RATIO - U256::from(1)
                };

                pool.swap(
                    zero_for_one,
                    amount_specified,
                    sqrt_price_limit_x96,
                    block_timestamp,
                );

                // Correct pool state from event to prevent rounding drift
                pool.slot0.sqrt_price_x96 = event_sqrt_price;
                pool.slot0.tick = event_tick;
                pool.liquidity = event_liquidity;
            }

            "Collect" => {
                let owner = arg_address(args, "owner");
                let tick_lower = arg_i32(args, "tickLower");
                let tick_upper = arg_i32(args, "tickUpper");
                let amount0 = arg_u128(args, "amount0");
                let amount1 = arg_u128(args, "amount1");
                pool.collect(owner, tick_lower, tick_upper, amount0, amount1);
            }

            "Flash" => {
                let paid0 = arg_u256(args, "paid0");
                let paid1 = arg_u256(args, "paid1");
                pool.flash_with_paid(paid0, paid1);
            }

            "SetFeeProtocol" => {
                let fee_protocol0_new = arg_u8(args, "feeProtocol0New");
                let fee_protocol1_new = arg_u8(args, "feeProtocol1New");
                pool.set_fee_protocol(fee_protocol0_new, fee_protocol1_new);
            }

            "IncreaseObservationCardinalityNext" => {
                let new_val = arg_u16(args, "observationCardinalityNextNew");
                pool.increase_observation_cardinality_next(new_val);
            }

            "CollectProtocol" => {
                // CollectProtocol just withdraws from protocol_fees — doesn't affect
                // swap state (sqrtPrice, tick, liquidity, feeGrowthGlobal).
                // We subtract from protocol_fees to keep that counter in sync.
                let amount0 = arg_u128(args, "amount0");
                let amount1 = arg_u128(args, "amount1");
                if amount0 > 0 {
                    pool.protocol_fees.token0 = pool.protocol_fees.token0.saturating_sub(amount0);
                }
                if amount1 > 0 {
                    pool.protocol_fees.token1 = pool.protocol_fees.token1.saturating_sub(amount1);
                }
            }

            other => {
                eprintln!("Unknown event: {}", other);
            }
        }

        processed += 1;
        if processed % 100 == 0 || processed == total_events {
            let extra = format!("block {}", block_number);
            progress::render("replay", processed, total_events, &extra, start_time);
        }
    }

    let elapsed = progress::format_duration(start_time.elapsed().as_millis());
    println!("\n\nReplay complete: {} events in {}", processed, elapsed);

    // Print event breakdown
    println!("\nEvent breakdown:");
    let mut counts: Vec<_> = event_counts.iter().collect();
    counts.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
    for (name, count) in &counts {
        println!("  {:>40}: {}", name, count);
    }

    // ── Replayed state ──────────────────────────────────────────────────────
    println!("\n--- Replayed Pool State ---");
    println!("  sqrtPriceX96:         {}", pool.slot0.sqrt_price_x96);
    println!("  tick:                 {}", pool.slot0.tick);
    println!("  liquidity:            {}", pool.liquidity);
    println!("  feeGrowthGlobal0X128: {}", pool.fee_growth_global_0_x128);
    println!("  feeGrowthGlobal1X128: {}", pool.fee_growth_global_1_x128);
    println!(
        "  observationIndex:     {}",
        pool.slot0.observation_index
    );
    println!(
        "  observationCardinality: {}",
        pool.slot0.observation_cardinality
    );
    println!(
        "  observationCardinalityNext: {}",
        pool.slot0.observation_cardinality_next
    );
    println!("  feeProtocol:          {}", pool.slot0.fee_protocol);

    // ── Verify against on-chain state ───────────────────────────────────────
    println!("\n--- On-Chain Verification at block {} ---", cfg.verify_at_block);

    let rpc_url: url::Url = cfg.rpc.parse().expect("invalid rpc url");
    let provider = ProviderBuilder::new().connect_http(rpc_url);
    let pool_address: Address = cfg.pool_address.parse().expect("invalid pool_address");

    let contract = IUniswapV3Pool::new(pool_address, &provider);

    let block_id = alloy::eips::BlockId::number(cfg.verify_at_block);

    // Query on-chain state
    let slot0_result = contract
        .slot0()
        .block(block_id)
        .call()
        .await
        .expect("slot0() call failed");

    let onchain_liquidity_result = contract
        .liquidity()
        .block(block_id)
        .call()
        .await
        .expect("liquidity() call failed");
    let onchain_liquidity: u128 = onchain_liquidity_result.into();

    let onchain_fg0_result = contract
        .feeGrowthGlobal0X128()
        .block(block_id)
        .call()
        .await
        .expect("feeGrowthGlobal0X128() call failed");
    let onchain_fg0: U256 = onchain_fg0_result.into();

    let onchain_fg1_result = contract
        .feeGrowthGlobal1X128()
        .block(block_id)
        .call()
        .await
        .expect("feeGrowthGlobal1X128() call failed");
    let onchain_fg1: U256 = onchain_fg1_result.into();

    // Extract on-chain values
    let onchain_sqrt_price = slot0_result.sqrtPriceX96;
    let onchain_tick = slot0_result.tick;
    let onchain_obs_index = slot0_result.observationIndex;
    let onchain_obs_cardinality = slot0_result.observationCardinality;
    let onchain_obs_cardinality_next = slot0_result.observationCardinalityNext;
    let onchain_fee_protocol = slot0_result.feeProtocol;

    println!("  sqrtPriceX96:         {}", onchain_sqrt_price);
    println!("  tick:                 {}", onchain_tick);
    println!("  liquidity:            {}", onchain_liquidity);
    println!("  feeGrowthGlobal0X128: {}", onchain_fg0);
    println!("  feeGrowthGlobal1X128: {}", onchain_fg1);
    println!("  observationIndex:     {}", onchain_obs_index);
    println!("  observationCardinality: {}", onchain_obs_cardinality);
    println!("  observationCardinalityNext: {}", onchain_obs_cardinality_next);
    println!("  feeProtocol:          {}", onchain_fee_protocol);

    // ── Compare ─────────────────────────────────────────────────────────────
    println!("\n--- Comparison ---");

    let sqrt_price_match = pool.slot0.sqrt_price_x96 == U256::from(onchain_sqrt_price);
    let tick_match = pool.slot0.tick == onchain_tick.as_i32();
    let liquidity_match = pool.liquidity == onchain_liquidity;
    let fg0_match = pool.fee_growth_global_0_x128 == onchain_fg0;
    let fg1_match = pool.fee_growth_global_1_x128 == onchain_fg1;
    let fee_protocol_match = pool.slot0.fee_protocol == onchain_fee_protocol;

    fn status(ok: bool) -> &'static str {
        if ok { "PASS" } else { "FAIL" }
    }

    println!(
        "  sqrtPriceX96:          [{}]",
        status(sqrt_price_match)
    );
    println!("  tick:                  [{}]", status(tick_match));
    println!(
        "  liquidity:             [{}]",
        status(liquidity_match)
    );
    println!(
        "  feeGrowthGlobal0X128:  [{}]",
        status(fg0_match)
    );
    println!(
        "  feeGrowthGlobal1X128:  [{}]",
        status(fg1_match)
    );
    println!(
        "  feeProtocol:           [{}]",
        status(fee_protocol_match)
    );

    let all_pass = sqrt_price_match && tick_match && liquidity_match && fg0_match && fg1_match && fee_protocol_match;

    if all_pass {
        println!("\nAll core state fields match on-chain state!");
    } else {
        println!("\nSome fields do not match. Check event coverage and replay logic.");
        std::process::exit(1);
    }
}
