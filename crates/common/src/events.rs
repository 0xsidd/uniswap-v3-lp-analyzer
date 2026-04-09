use alloy::sol;
use alloy::sol_types::SolEvent;
use mongodb::bson::{doc, Document};
use std::collections::HashMap;

// ── Uniswap V3 Pool Events ─────────────────────────────────────────────────

sol! {
    event Swap(
        address indexed sender,
        address indexed recipient,
        int256 amount0,
        int256 amount1,
        uint160 sqrtPriceX96,
        uint128 liquidity,
        int24 tick
    );

    event Mint(
        address sender,
        address indexed owner,
        int24 indexed tickLower,
        int24 indexed tickUpper,
        uint128 amount,
        uint256 amount0,
        uint256 amount1
    );

    event Burn(
        address indexed owner,
        int24 indexed tickLower,
        int24 indexed tickUpper,
        uint128 amount,
        uint256 amount0,
        uint256 amount1
    );

    event Collect(
        address indexed owner,
        address recipient,
        int24 indexed tickLower,
        int24 indexed tickUpper,
        uint128 amount0,
        uint128 amount1
    );

    event CollectProtocol(
        address indexed sender,
        address indexed recipient,
        uint128 amount0,
        uint128 amount1
    );

    event Flash(
        address indexed sender,
        address indexed recipient,
        uint256 amount0,
        uint256 amount1,
        uint256 paid0,
        uint256 paid1
    );

    event Initialize(uint160 sqrtPriceX96, int24 tick);

    event IncreaseObservationCardinalityNext(
        uint16 observationCardinalityNextOld,
        uint16 observationCardinalityNextNew
    );

    event SetFeeProtocol(
        uint8 feeProtocol0Old,
        uint8 feeProtocol1Old,
        uint8 feeProtocol0New,
        uint8 feeProtocol1New
    );
}

// ── ERC20 + Pool contract calls ─────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    contract ERC20 {
        function symbol() external view returns (string);
        function decimals() external view returns (uint8);
    }

    #[sol(rpc)]
    contract UniswapV3Pool {
        function token0() external view returns (address);
        function token1() external view returns (address);
    }
}

// ── Log → BSON Document parser ──────────────────────────────────────────────

pub fn parse_log(log: &alloy::rpc::types::Log) -> Option<Document> {
    let block = log.block_number?;
    let tx_hash = format!("{:#x}", log.transaction_hash?);
    let tx_idx = log.transaction_index? as i64;
    let log_idx = log.log_index? as i64;
    let topics = log.topics();
    let data = log.data().data.as_ref();

    if topics.is_empty() {
        return None;
    }
    let sig = topics[0];

    let (event_name, args) = if sig == Swap::SIGNATURE_HASH {
        let d = Swap::decode_raw_log(topics.iter().copied(), data).ok()?;
        let mut m = HashMap::new();
        m.insert("sender", format!("{:#x}", d.sender));
        m.insert("recipient", format!("{:#x}", d.recipient));
        m.insert("amount0", d.amount0.to_string());
        m.insert("amount1", d.amount1.to_string());
        m.insert("sqrtPriceX96", d.sqrtPriceX96.to_string());
        m.insert("liquidity", d.liquidity.to_string());
        m.insert("tick", d.tick.to_string());
        ("Swap", m)
    } else if sig == Mint::SIGNATURE_HASH {
        let d = Mint::decode_raw_log(topics.iter().copied(), data).ok()?;
        let mut m = HashMap::new();
        m.insert("sender", format!("{:#x}", d.sender));
        m.insert("owner", format!("{:#x}", d.owner));
        m.insert("tickLower", d.tickLower.to_string());
        m.insert("tickUpper", d.tickUpper.to_string());
        m.insert("amount", d.amount.to_string());
        m.insert("amount0", d.amount0.to_string());
        m.insert("amount1", d.amount1.to_string());
        ("Mint", m)
    } else if sig == Burn::SIGNATURE_HASH {
        let d = Burn::decode_raw_log(topics.iter().copied(), data).ok()?;
        let mut m = HashMap::new();
        m.insert("owner", format!("{:#x}", d.owner));
        m.insert("tickLower", d.tickLower.to_string());
        m.insert("tickUpper", d.tickUpper.to_string());
        m.insert("amount", d.amount.to_string());
        m.insert("amount0", d.amount0.to_string());
        m.insert("amount1", d.amount1.to_string());
        ("Burn", m)
    } else if sig == Collect::SIGNATURE_HASH {
        let d = Collect::decode_raw_log(topics.iter().copied(), data).ok()?;
        let mut m = HashMap::new();
        m.insert("owner", format!("{:#x}", d.owner));
        m.insert("recipient", format!("{:#x}", d.recipient));
        m.insert("tickLower", d.tickLower.to_string());
        m.insert("tickUpper", d.tickUpper.to_string());
        m.insert("amount0", d.amount0.to_string());
        m.insert("amount1", d.amount1.to_string());
        ("Collect", m)
    } else if sig == CollectProtocol::SIGNATURE_HASH {
        let d = CollectProtocol::decode_raw_log(topics.iter().copied(), data).ok()?;
        let mut m = HashMap::new();
        m.insert("sender", format!("{:#x}", d.sender));
        m.insert("recipient", format!("{:#x}", d.recipient));
        m.insert("amount0", d.amount0.to_string());
        m.insert("amount1", d.amount1.to_string());
        ("CollectProtocol", m)
    } else if sig == Flash::SIGNATURE_HASH {
        let d = Flash::decode_raw_log(topics.iter().copied(), data).ok()?;
        let mut m = HashMap::new();
        m.insert("sender", format!("{:#x}", d.sender));
        m.insert("recipient", format!("{:#x}", d.recipient));
        m.insert("amount0", d.amount0.to_string());
        m.insert("amount1", d.amount1.to_string());
        m.insert("paid0", d.paid0.to_string());
        m.insert("paid1", d.paid1.to_string());
        ("Flash", m)
    } else if sig == Initialize::SIGNATURE_HASH {
        let d = Initialize::decode_raw_log(topics.iter().copied(), data).ok()?;
        let mut m = HashMap::new();
        m.insert("sqrtPriceX96", d.sqrtPriceX96.to_string());
        m.insert("tick", d.tick.to_string());
        ("Initialize", m)
    } else if sig == IncreaseObservationCardinalityNext::SIGNATURE_HASH {
        let d = IncreaseObservationCardinalityNext::decode_raw_log(
            topics.iter().copied(),
            data,
        )
        .ok()?;
        let mut m = HashMap::new();
        m.insert(
            "observationCardinalityNextOld",
            d.observationCardinalityNextOld.to_string(),
        );
        m.insert(
            "observationCardinalityNextNew",
            d.observationCardinalityNextNew.to_string(),
        );
        ("IncreaseObservationCardinalityNext", m)
    } else if sig == SetFeeProtocol::SIGNATURE_HASH {
        let d = SetFeeProtocol::decode_raw_log(topics.iter().copied(), data).ok()?;
        let mut m = HashMap::new();
        m.insert("feeProtocol0Old", d.feeProtocol0Old.to_string());
        m.insert("feeProtocol1Old", d.feeProtocol1Old.to_string());
        m.insert("feeProtocol0New", d.feeProtocol0New.to_string());
        m.insert("feeProtocol1New", d.feeProtocol1New.to_string());
        ("SetFeeProtocol", m)
    } else {
        return None;
    };

    let args_doc: Document = args
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.into()))
        .collect();

    Some(doc! {
        "blockNumber": block as i64,
        "transactionHash": &tx_hash,
        "transactionIndex": tx_idx,
        "logIndex": log_idx,
        "eventName": event_name,
        "args": args_doc,
    })
}
