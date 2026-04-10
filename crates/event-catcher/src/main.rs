use alloy::primitives::Address;
use alloy::providers::ProviderBuilder;
use mongodb::bson::{doc, Document};
use mongodb::options::{IndexOptions, InsertManyOptions};
use mongodb::{Client as MongoClient, Collection, IndexModel};
use std::time::Instant;

use sim_common::{events, progress, retry};
use sim_config::Config;

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();
    let cfg = Config::load();
    let pool_addr: Address = cfg.pool_address.parse().expect("invalid pool_address");

    let rpc_url: url::Url = std::env::var("RPC_URL").expect("RPC_URL not set in .env").parse().expect("invalid RPC_URL");
    let provider = ProviderBuilder::new().connect_http(rpc_url);

    // ── MongoDB setup ───────────────────────────────────────────────────────
    let mongo = MongoClient::with_uri_str(&cfg.mongo_uri)
        .await
        .expect("mongo connect failed");
    let db = mongo.database(&cfg.db_name);
    let events_col: Collection<Document> = db.collection("events");
    let meta_col: Collection<Document> = db.collection("meta");

    // Create unique index
    let idx = IndexModel::builder()
        .keys(doc! { "blockNumber": 1, "transactionIndex": 1, "logIndex": 1 })
        .options(IndexOptions::builder().unique(true).build())
        .build();
    events_col.create_index(idx).await.ok();

    // ── Config change detection ─────────────────────────────────────────────
    let saved = meta_col
        .find_one(doc! { "_id": "catcher_config" })
        .await
        .unwrap();
    let config_changed = match &saved {
        Some(d) => {
            d.get_i64("fromBlock").unwrap_or(-1) != cfg.from_block as i64
                || d.get_i64("toBlock").unwrap_or(-1) != cfg.to_block as i64
                || d.get_str("poolAddress").unwrap_or("") != cfg.pool_address
        }
        None => true,
    };

    if config_changed {
        println!("Config changed — dropping old events...");
        events_col.drop().await.ok();
        let idx = IndexModel::builder()
            .keys(doc! { "blockNumber": 1, "transactionIndex": 1, "logIndex": 1 })
            .options(IndexOptions::builder().unique(true).build())
            .build();
        events_col.create_index(idx).await.ok();
        meta_col
            .replace_one(
                doc! { "_id": "catcher_config" },
                doc! {
                    "_id": "catcher_config",
                    "fromBlock": cfg.from_block as i64,
                    "toBlock": cfg.to_block as i64,
                    "poolAddress": &cfg.pool_address,
                    "lastBlock": (cfg.from_block - 1) as i64,
                },
            )
            .upsert(true)
            .await
            .unwrap();
    }

    // ── Fetch token info ────────────────────────────────────────────────────
    let pool = events::UniswapV3Pool::new(pool_addr, &provider);
    let token0_addr = Address::from(pool.token0().call().await.expect("token0 call failed"));
    let token1_addr = Address::from(pool.token1().call().await.expect("token1 call failed"));

    let t0 = events::ERC20::new(token0_addr, &provider);
    let t1 = events::ERC20::new(token1_addr, &provider);
    let sym0: String = t0.symbol().call().await.expect("symbol0");
    let sym1: String = t1.symbol().call().await.expect("symbol1");
    let dec0: u8 = t0.decimals().call().await.expect("decimals0");
    let dec1: u8 = t1.decimals().call().await.expect("decimals1");

    println!("Pool: {}", cfg.pool_address);
    println!("Token0: {} ({:#x}) decimals={}", sym0, token0_addr, dec0);
    println!("Token1: {} ({:#x}) decimals={}", sym1, token1_addr, dec1);
    println!("---");

    // ── Determine start block ───────────────────────────────────────────────
    let meta = meta_col
        .find_one(doc! { "_id": "catcher_config" })
        .await
        .unwrap();
    let last_block = meta
        .as_ref()
        .and_then(|d| d.get_i64("lastBlock").ok())
        .unwrap_or(cfg.from_block as i64 - 1) as u64;
    let start_block = last_block + 1;

    if start_block > cfg.to_block {
        let count = events_col.count_documents(doc! {}).await.unwrap_or(0);
        println!("All blocks already processed. {} events in DB.", count);
        return;
    }

    let total_blocks = cfg.to_block - start_block + 1;
    let existing_count = events_col.count_documents(doc! {}).await.unwrap_or(0);
    let mut processed_blocks: u64 = 0;
    let mut total_events = existing_count;
    let mut batch_size = cfg.batch_size;
    let start_time = Instant::now();

    if existing_count > 0 {
        println!(
            "Resuming from block {} ({} events already in DB)",
            start_block, existing_count
        );
    } else {
        println!("Starting fresh from block {}", start_block);
    }
    println!(
        "Range: {} -> {} ({} blocks)\n",
        start_block, cfg.to_block, total_blocks
    );

    // ── Fetch in batches ────────────────────────────────────────────────────
    let mut current_from = start_block;
    while current_from <= cfg.to_block {
        let current_to = (current_from + batch_size - 1).min(cfg.to_block);

        match retry::fetch_logs(&provider, pool_addr, current_from, current_to).await {
            Ok(logs) => {
                let docs: Vec<Document> =
                    logs.iter().filter_map(|l| events::parse_log(l)).collect();
                let new_count = docs.len() as u64;

                if !docs.is_empty() {
                    let opts = InsertManyOptions::builder().ordered(false).build();
                    if let Err(e) = events_col.insert_many(&docs).with_options(opts).await {
                        let msg = format!("{}", e);
                        if !msg.contains("E11000") {
                            eprintln!("\n[MONGO ERROR] {}", msg);
                        }
                    }
                    total_events += new_count;
                }

                // Update progress
                meta_col
                    .update_one(
                        doc! { "_id": "catcher_config" },
                        doc! { "$set": { "lastBlock": current_to as i64 } },
                    )
                    .await
                    .ok();

                processed_blocks += current_to - current_from + 1;
                let extra = format!("{} blocks  |  {} events", total_blocks, total_events);
                progress::render("blocks", processed_blocks, total_blocks, &extra, start_time);

                current_from = current_to + 1;

                if batch_size < cfg.batch_size {
                    batch_size = (batch_size * 2).min(cfg.batch_size);
                }
            }
            Err(msg) => {
                if batch_size > 100 {
                    eprintln!(
                        "\n  Batch error ({}-{}), halving batch size to {}...",
                        current_from,
                        current_to,
                        batch_size >> 1
                    );
                    batch_size >>= 1;
                    continue;
                }
                eprintln!("\n\n[ERROR] {}", msg);
                eprintln!(
                    "Stopped at block {}. Run again to resume.\n",
                    current_from - 1
                );
                std::process::exit(1);
            }
        }
    }

    let final_count = events_col.count_documents(doc! {}).await.unwrap_or(0);
    let elapsed = progress::format_duration(start_time.elapsed().as_millis());
    println!("\n\nDone! {} events in MongoDB ({})", final_count, elapsed);
}
