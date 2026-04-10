use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Clone)]
pub struct Config {
    pub pool_address: String,
    pub from_block: u64,
    pub to_block: u64,
    pub batch_size: u64,
    pub mongo_uri: String,
    pub db_name: String,
}

impl Config {
    pub fn load() -> Self {
        Self::load_from("config.toml")
    }

    pub fn load_from(path: impl AsRef<Path>) -> Self {
        let raw = std::fs::read_to_string(path.as_ref())
            .unwrap_or_else(|_| panic!("{} not found", path.as_ref().display()));
        toml::from_str(&raw).expect("invalid config.toml")
    }
}
