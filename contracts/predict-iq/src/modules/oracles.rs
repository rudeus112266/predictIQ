use crate::errors::ErrorCode;
use crate::types::OracleConfig;
use soroban_sdk::{contracttype, symbol_short, Bytes, Env, Map};

pub const MAX_STALENESS: u64 = 60;
pub const MAX_STALENESS_SECONDS: u64 = MAX_STALENESS;

#[contracttype]
pub enum OracleData {
    Result(u64, u32),     // market_id -> outcome
    LastUpdate(u64, u64), // market_id -> timestamp
    OracleResponses(u64), // market_id -> Map<oracle_index, outcome>
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PythPrice {
    pub price: i64,
    pub conf: u64,
    pub expo: i32,
    pub publish_time: i64,
}

/// Decode a 64-char hex feed_id string into a 32-byte BytesN<32>.
fn decode_feed_id(
    e: &Env,
    feed_id: &soroban_sdk::String,
) -> Result<soroban_sdk::BytesN<32>, ErrorCode> {
    if feed_id.len() != 64 {
        return Err(ErrorCode::OracleFailure);
    }
    let bytes = Bytes::from(feed_id.clone());
    let mut buf = [0u8; 32];
    for i in 0..32 {
        let hi = hex_char(bytes.get(i * 2).ok_or(ErrorCode::OracleFailure)?)?;
        let lo = hex_char(bytes.get(i * 2 + 1).ok_or(ErrorCode::OracleFailure)?)?;
        buf[i as usize] = (hi << 4) | lo;
    }
    Ok(soroban_sdk::BytesN::from_array(e, &buf))
}

fn hex_char(c: u8) -> Result<u8, ErrorCode> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(ErrorCode::OracleFailure),
    }
}

/// Call the on-chain Pyth price-feed contract using config.oracle_address and config.feed_id.
pub fn fetch_pyth_price(e: &Env, config: &OracleConfig) -> Result<PythPrice, ErrorCode> {
    let feed_id = decode_feed_id(e, &config.feed_id)?;
    let client = crate::pyth_client::PythOracleClient::new(e, &config.oracle_address);
    let (price, conf, expo, publish_time) = client.get_price(&feed_id);
    Ok(PythPrice {
        price,
        conf,
        expo,
        publish_time,
    })
}

pub fn validate_price(e: &Env, price: &PythPrice, config: &OracleConfig) -> Result<(), ErrorCode> {
    let publish_time = cast_external_timestamp(price.publish_time)?;
    let current_time = e.ledger().timestamp();

    if is_stale(current_time, publish_time, effective_max_staleness(config)) {
        return Err(ErrorCode::StalePrice);
    }

    let price_abs = abs_price_to_u64(price.price);
    let max_conf = (price_abs.saturating_mul(config.max_confidence_bps)) / 10000;

    if price.conf > max_conf {
        return Err(ErrorCode::ConfidenceTooLow);
    }

    Ok(())
}

/// Issue #508: Validate oracle staleness before resolution — checks all configured oracle indices.
pub fn validate_oracle_staleness(
    e: &Env,
    market_id: u64,
    config: &OracleConfig,
) -> Result<(), ErrorCode> {
    let num_oracles = config.min_responses.unwrap_or(1);
    let current_time = e.ledger().timestamp();
    let max_staleness = effective_max_staleness(config);
    let mut any_found = false;

    for idx in 0..num_oracles {
        let last_update = e
            .storage()
            .persistent()
            .get::<_, u64>(&OracleData::LastUpdate(market_id, idx as u64));

        if let Some(update_time) = last_update {
            any_found = true;
            let age = current_time.saturating_sub(update_time);
            if age > max_staleness {
                crate::modules::monitoring::track_error(e);
                return Err(ErrorCode::StalePrice);
            }
        }
    }

    if any_found {
        Ok(())
    } else {
        crate::modules::monitoring::track_error(e);
        Err(ErrorCode::OracleFailure)
    }
}

pub fn resolve_with_pyth(
    e: &Env,
    market_id: u64,
    oracle_id: u32,
    config: &OracleConfig,
) -> Result<u32, ErrorCode> {
    let price = fetch_pyth_price(e, config)?;
    validate_price(e, &price, config)?;

    let outcome = determine_outcome(&price, config);

    let publish_time = cast_external_timestamp(price.publish_time)?;

    e.storage()
        .persistent()
        .set(&OracleData::Result(market_id, oracle_id), &outcome);
    e.storage().persistent().set(
        &OracleData::LastUpdate(market_id, oracle_id as u64),
        &publish_time,
    );

    e.events().publish(
        (
            symbol_short!("oracle_ok"),
            market_id,
            config.oracle_address.clone(),
        ),
        (outcome, price.price, price.conf),
    );

    Ok(outcome)
}

/// Issue #1545: Determine the winning outcome index from a Pyth price.
///
/// Binary markets (2 options) keep the original above/below-strike semantics:
/// outcome 0 when `price >= strike_price`, outcome 1 otherwise.
///
/// Multi-outcome markets (3+ options) are resolved against ascending price
/// brackets supplied via `config.strike_thresholds`. The price is compared
/// against each threshold in order; the first threshold the price is below
/// selects that bracket's outcome index. A price at or above every threshold
/// selects the final (highest) outcome index. This lets auto-resolution pick
/// outcome indices >= 2 instead of silently collapsing to 0/1.
fn determine_outcome(price: &PythPrice, config: &OracleConfig) -> u32 {
    let thresholds = config.strike_thresholds.as_ref();

    // Multi-outcome bracket resolution when explicit thresholds are configured.
    if let Some(thresholds) = thresholds {
        if thresholds.len() >= 2 {
            for (i, threshold) in thresholds.iter().enumerate() {
                if price.price < threshold {
                    return i as u32;
                }
            }
            // Price at or above every threshold -> highest bracket.
            return thresholds.len() as u32;
        }
    }

    // Binary fallback (unchanged legacy behavior).
    let threshold = config.strike_price.unwrap_or(0);
    if price.price >= threshold {
        0
    } else {
        1
    }
}

pub fn get_oracle_result(e: &Env, market_id: u64, oracle_id: u32) -> Option<u32> {
    e.storage()
        .persistent()
        .get(&OracleData::Result(market_id, oracle_id))
}

pub fn get_last_update(e: &Env, market_id: u64, oracle_id: u32) -> Option<u64> {
    e.storage()
        .persistent()
        .get(&OracleData::LastUpdate(market_id, oracle_id as u64))
}

pub fn set_oracle_result(
    e: &Env,
    market_id: u64,
    oracle_id: u32,
    outcome: u32,
) -> Result<(), ErrorCode> {
    e.storage()
        .persistent()
        .set(&OracleData::Result(market_id, oracle_id), &outcome);
    e.storage().persistent().set(
        &OracleData::LastUpdate(market_id, oracle_id as u64),
        &e.ledger().timestamp(),
    );

    let oracle_addr = crate::modules::markets::get_market(e, market_id)
        .map(|m| m.oracle_config.oracle_address)
        .unwrap_or_else(|| e.current_contract_address());
    crate::modules::events::emit_oracle_result_set(e, market_id, oracle_id, oracle_addr, outcome);

    Ok(())
}

/// Convert i64 timestamp to u64, rejecting negative values.
pub fn cast_external_timestamp(ts: i64) -> Result<u64, ErrorCode> {
    if ts < 0 {
        Err(ErrorCode::InvalidTimestamp)
    } else {
        Ok(ts as u64)
    }
}

/// Check if oracle data is stale (age > max_staleness_seconds).
pub fn is_stale(current_time: u64, result_time: u64, max_staleness_seconds: u64) -> bool {
    let age = current_time.saturating_sub(result_time);
    age > max_staleness_seconds
}

fn effective_max_staleness(config: &OracleConfig) -> u64 {
    if config.max_staleness_seconds < MAX_STALENESS_SECONDS {
        config.max_staleness_seconds
    } else {
        MAX_STALENESS_SECONDS
    }
}

/// Convert i64 price to u64 absolute value, saturating i64::MIN to i64::MAX as u64.
pub fn abs_price_to_u64(price: i64) -> u64 {
    if price == i64::MIN {
        i64::MAX as u64
    } else if price < 0 {
        (-price) as u64
    } else {
        price as u64
    }
}

/// Issue #1554: Validate that an oracle config is usable before a market accepts bets.
///
/// Unlike a mere non-emptiness check, this verifies the `feed_id` actually decodes
/// into a valid 32-byte feed identifier (exactly 64 hex characters). This catches
/// malformed feed_ids at market-creation time instead of only failing later at
/// resolution via `decode_feed_id`.
pub fn verify_oracle_health(e: &Env, config: &OracleConfig) -> bool {
    decode_feed_id(e, &config.feed_id).is_ok()
}

/// Issue #1554: Reject market creation when the oracle config is malformed.
pub fn validate_oracle_config(e: &Env, config: &OracleConfig) -> Result<(), ErrorCode> {
    if verify_oracle_health(e, config) {
        Ok(())
    } else {
        Err(ErrorCode::OracleFailure)
    }
}

/// Issue #509: Record an oracle response for consensus validation
pub fn record_oracle_response(
    e: &Env,
    market_id: u64,
    oracle_index: u32,
    outcome: u32,
) -> Result<(), ErrorCode> {
    let key = OracleData::OracleResponses(market_id);
    let mut responses: Map<u32, u32> = e
        .storage()
        .persistent()
        .get(&key)
        .unwrap_or_else(|| Map::new(e));

    responses.set(oracle_index, outcome);
    e.storage().persistent().set(&key, &responses);

    Ok(())
}

/// Issue #509: Validate oracle consensus - requires min_responses confirmations
pub fn validate_consensus(
    e: &Env,
    

/* … truncated 3836 chars — edit only what you need near the top … */
