use alloy::primitives::Address;
use alloy::providers::Provider;
use alloy::rpc::types::{Filter, Log};

const MAX_RETRIES: u32 = 10;
const BASE_DELAY_MS: u64 = 2000;

pub async fn fetch_logs(
    provider: &impl Provider,
    pool_addr: Address,
    from: u64,
    to: u64,
) -> Result<Vec<Log>, String> {
    for attempt in 1..=MAX_RETRIES {
        let filter = Filter::new()
            .address(pool_addr)
            .from_block(from)
            .to_block(to);
        match provider.get_logs(&filter).await {
            Ok(logs) => return Ok(logs),
            Err(e) => {
                if attempt == MAX_RETRIES {
                    return Err(format!("{}", e));
                }
                let delay = BASE_DELAY_MS * 2u64.pow(attempt - 1);
                let msg = format!("{}", e);
                eprintln!(
                    "\n  [retry {}/{}] {}... waiting {}s",
                    attempt,
                    MAX_RETRIES,
                    &msg[..msg.len().min(120)],
                    delay / 1000
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
        }
    }
    Err("unreachable".into())
}
