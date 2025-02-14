use std::collections::{HashMap, BTreeMap};
use std::fs::{self, OpenOptions};
use std::io::{Write};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH, Instant};
use std::env;

use csv::Writer;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde::de;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use num_format::{Locale, ToFormattedString};

use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use crossterm::{
    ExecutableCommand,
    terminal::{Clear, ClearType},
};
use std::io::stdout;

use chrono::Utc;

// --- Governor (rate limiting) dependencies ---
use governor::{Quota, RateLimiter, clock::DefaultClock, state::{NotKeyed, InMemoryState}};
use std::num::NonZeroU32;

/// Global flag to control API request logging.
static API_LOG_ENABLED: AtomicBool = AtomicBool::new(true);

/// Returns a string formatted as milliseconds for the given `Duration`.
fn format_duration_millis(duration: Duration) -> String {
    format!("{}ms", duration.as_millis())
}

/// Returns a human-readable string (seconds, minutes, or hours) for the given `Duration`.
fn format_duration_seconds(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        let minutes = secs / 60;
        let seconds = secs % 60;
        format!("{}m {}s", minutes, seconds)
    } else {
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        let seconds = secs % 60;
        format!("{}h {}m {}s", hours, minutes, seconds)
    }
}

/// Logs an API request message with a timestamp to "api_requests.log".
async fn log_api_request(message: &str) {
    if !API_LOG_ENABLED.load(Ordering::Relaxed) {
        return;
    }
    use tokio::io::AsyncWriteExt;
    let timestamp = Utc::now().to_rfc3339();
    let log_line = format!("{} - {}\n", timestamp, message);
    if let Ok(mut file) = tokio::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open("api_requests.log")
        .await
    {
        let _ = file.write_all(log_line.as_bytes()).await;
    }
}

// -----------------------------------------------------------------------------
// Constant definitions for limits, delays, and other configuration.
// -----------------------------------------------------------------------------

const MAX_STREAMS_PER_CONNECTION: usize = 100;
const MAX_RECONNECT_ATTEMPTS: u32 = 5;
const BASE_RECONNECT_DELAY: u64 = 1;
const MAX_BUFFERED_RECORDS: usize = 1_000_000;
const MAX_TRADES_PER_REQUEST: u64 = 1000;
const RATE_LIMIT_REQUESTS_PER_SECOND_FUTURES: usize = 2;
const RATE_LIMIT_REQUESTS_PER_SECOND_SPOT: usize = 25;
const FINALIZATION_BUFFER_SECONDS: u64 = 3;

// -----------------------------------------------------------------------------
// Data structures for trade data and aggregation.
// -----------------------------------------------------------------------------

/// Represents an aggregated trade record.
#[derive(Debug, Deserialize)]
struct AggTrade {
    s: String, // Symbol (present in WS messages; injected for REST responses)
    p: String, // Price as a string
    q: String, // Quantity as a string
    m: bool,   // Market maker flag.
    #[serde(rename = "a")]
    agg_trade_id: u64,
    #[serde(rename = "T")]
    trade_timestamp: u64,
    #[serde(flatten)]
    _extra: serde_json::Value,
}

/// Wrapper for aggregated trade data received from the WebSocket.
#[derive(Debug, Deserialize)]
struct WsAggTradeWrapper {
    data: AggTrade,
}

/// Represents an aggregated trade record from REST responses.
#[derive(Debug, Deserialize)]
struct RestAggTrade {
    #[serde(rename = "a")]
    agg_trade_id: u64,
    #[serde(rename = "p")]
    p: String,
    #[serde(rename = "q")]
    q: String,
    #[serde(rename = "T")]
    trade_timestamp: u64,
    #[serde(rename = "m")]
    m: bool,
}

/// Represents the two market types.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum MarketType {
    Futures,
    Spot,
}

/// A simple CSV writer wrapper for each symbol.
struct SymbolWriter {
    writer: Writer<std::fs::File>,
}

/// Holds aggregated trade data for a specific minute.
#[derive(Debug, Clone)]
struct AggTradeAggregate {
    net_flow: f64,
    start_atid: Option<u64>,
    end_atid: Option<u64>,
    count: usize,
}

/// Tracks the last seen trade ID per symbol/market pair.
struct AggTradeTracker {
    last_trade_ids: HashMap<(String, MarketType), u64>,
}

impl AggTradeTracker {
    fn new() -> Self {
        Self {
            last_trade_ids: HashMap::new(),
        }
    }

    /// Checks for missing trade IDs. If a gap is found, returns (start, end) of missing IDs.
    fn check_and_update(&mut self, symbol: &str, market: MarketType, agg_trade_id: u64) -> Option<(u64, u64)> {
        let key = (symbol.to_string(), market);
        let last_trade = self.last_trade_ids.entry(key).or_insert(agg_trade_id);
        if agg_trade_id > *last_trade + 1 {
            let start = *last_trade + 1;
            let end = agg_trade_id - 1;
            *last_trade = agg_trade_id;
            Some((start, end))
        } else {
            *last_trade = agg_trade_id.max(*last_trade);
            None
        }
    }
}

/// Runtime metrics.
struct Metrics {
    current_batch_records: AtomicUsize,
    last_batch_records: AtomicUsize,
    last_batch_processing_time: Mutex<Duration>,
    start_time: Instant,
    missing_gaps: AtomicUsize,
    gaps_in_queue: AtomicUsize,
}

/// A helper struct for CSV checkpoint entries.
#[derive(Serialize, Deserialize, Debug)]
struct Checkpoint {
    symbol: String,
    market: String,
    end_atid_refpt: u64,
}

/// Custom deserializer for net_flow that strips commas.
fn deserialize_net_flow<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: de::Deserializer<'de>,
{
    let s: String = String::deserialize(deserializer)?;
    let s_clean = s.replace(",", "");
    s_clean.parse::<f64>().map_err(de::Error::custom)
}

/// A helper struct representing a CSV record.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CsvRecord {
    timestamp: u64,
    start_atid: u64,
    end_atid: u64,
    #[serde(deserialize_with = "deserialize_net_flow")]
    net_flow: f64,
}

// -----------------------------------------------------------------------------
// Functions for fetching trade data via REST and handling WebSocket connections.
// -----------------------------------------------------------------------------

async fn fetch_usdt_symbols(client: &reqwest::Client, is_futures: bool) -> Result<Vec<String>, reqwest::Error> {
    let url = if is_futures {
        "https://fapi.binance.com/fapi/v1/exchangeInfo"
    } else {
        "https://api.binance.com/api/v3/exchangeInfo"
    };

    let exchange_info: serde_json::Value = client.get(url)
        .timeout(Duration::from_secs(10))
        .send()
        .await?
        .json()
        .await?;

    Ok(exchange_info["symbols"].as_array().unwrap().iter()
        .filter(|s| {
            s["quoteAsset"].as_str() == Some("USDT") &&
            s["status"].as_str() == Some("TRADING") &&
            !s["symbol"].as_str().unwrap().contains("_")
        })
        .map(|s| s["symbol"].as_str().unwrap().to_lowercase())
        .collect())
}

async fn fetch_missing_agg_trades(
    client: &reqwest::Client,
    symbol: &str,
    market: MarketType,
    start_id: u64,
    end_id: u64,
    rate_limiter: Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
) -> Result<Vec<AggTrade>, Box<dyn std::error::Error + Send + Sync>> {
    let mut records = Vec::new();
    let mut current_start = start_id;
    while current_start <= end_id {
        rate_limiter.until_ready().await;
        let url = match market {
            MarketType::Futures => format!(
                "https://fapi.binance.com/fapi/v1/aggTrades?symbol={}&fromId={}&limit={}",
                symbol, current_start, MAX_TRADES_PER_REQUEST
            ),
            MarketType::Spot => format!(
                "https://api.binance.com/api/v3/aggTrades?symbol={}&fromId={}&limit={}",
                symbol, current_start, MAX_TRADES_PER_REQUEST
            ),
        };
        log_api_request(&format!("Sending request: {}", url)).await;
        let response = client.get(&url).send().await?;
        let status = response.status();
        let text = response.text().await?;
        log_api_request(&format!("Received response for {}: status {} Body: {}", url, status, text)).await;
        if !status.is_success() {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("HTTP error {}: {}", status, text)
            )));
        }
        let json_value: serde_json::Value = serde_json::from_str(&text)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("Failed to parse JSON: {}", e)))?;
        if !json_value.is_array() {
            return Err(Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("Unexpected JSON format: {}", json_value)
            )));
        }
        let rest_records: Vec<RestAggTrade> = serde_json::from_value(json_value)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("Failed to deserialize agg trades: {}", e)))?;
        if rest_records.is_empty() {
            break;
        }
        if let Some(last_trade) = rest_records.last() {
            if last_trade.agg_trade_id > end_id {
                let filtered: Vec<RestAggTrade> = rest_records.into_iter()
                    .filter(|r| r.agg_trade_id <= end_id)
                    .collect();
                for r in filtered {
                    records.push(AggTrade {
                        s: symbol.to_string(),
                        p: r.p,
                        q: r.q,
                        m: r.m,
                        agg_trade_id: r.agg_trade_id,
                        trade_timestamp: r.trade_timestamp,
                        _extra: serde_json::Value::Null,
                    });
                }
                break;
            }
            current_start = last_trade.agg_trade_id + 1;
        } else {
            break;
        }
        for r in rest_records {
            records.push(AggTrade {
                s: symbol.to_string(),
                p: r.p,
                q: r.q,
                m: r.m,
                agg_trade_id: r.agg_trade_id,
                trade_timestamp: r.trade_timestamp,
                _extra: serde_json::Value::Null,
            });
        }
    }
    Ok(records)
}

fn market_str(market: MarketType) -> &'static str {
    match market {
        MarketType::Futures => "futures",
        MarketType::Spot => "spot",
    }
}

async fn run_ws_connection(
    market: MarketType,
    symbol_list: Vec<String>,
    trade_sender: tokio::sync::mpsc::Sender<(MarketType, AggTrade)>,
    connection_semaphore: Arc<tokio::sync::Semaphore>,
    symbol_version_rx: tokio::sync::watch::Receiver<usize>,
    current_symbol_version: usize,
) {
    let mut reconnect_attempts = 0;
    let stream_paths = symbol_list.iter()
        .map(|symbol| format!("{}@aggTrade", symbol))
        .collect::<Vec<_>>()
        .join("/");
    let url = match market {
        MarketType::Futures => format!("wss://fstream.binance.com/stream?streams={}", stream_paths),
        MarketType::Spot => format!("wss://stream.binance.com:9443/stream?streams={}", stream_paths),
    };

    loop {
        if *symbol_version_rx.borrow() != current_symbol_version {
            break;
        }
        let permit = match connection_semaphore.acquire().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        match connect_async(&url).await {
            Ok((stream, _)) => {
                reconnect_attempts = 0;
                let (_, mut incoming_messages) = stream.split();
                while let Some(message) = incoming_messages.next().await {
                    if *symbol_version_rx.borrow() != current_symbol_version {
                        break;
                    }
                    match message {
                        Ok(Message::Text(text)) => {
                            match serde_json::from_str::<WsAggTradeWrapper>(&text) {
                                Ok(wrapper) => {
                                    if let Err(e) = trade_sender.send((market, wrapper.data)).await {
                                        eprintln!("\nTrade channel send error: {}", e);
                                    }
                                },
                                Err(e) => eprintln!("\nWebSocket parse error: {}", e),
                            }
                        },
                        Err(e) => {
                            eprintln!("\nWebSocket error: {}", e);
                            break;
                        },
                        _ => {}
                    }
                }
            },
            Err(e) => {
                eprintln!("\nWebSocket connection failed: {} (attempt {})", e, reconnect_attempts);
                let delay = BASE_RECONNECT_DELAY * 2u64.pow(reconnect_attempts);
                tokio::time::sleep(Duration::from_secs(delay)).await;
                reconnect_attempts = (reconnect_attempts + 1).min(MAX_RECONNECT_ATTEMPTS);
            }
        }
        drop(permit);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn spawn_ws_connections(
    market: MarketType,
    symbols: Vec<String>,
    ws_trade_sender: tokio::sync::mpsc::Sender<(MarketType, AggTrade)>,
    websocket_semaphore: Arc<tokio::sync::Semaphore>,
    ws_version_tx: &tokio::sync::watch::Sender<usize>,
) {
    let version_rx = ws_version_tx.subscribe();
    let current_version = *version_rx.borrow();
    for symbols_chunk in symbols.chunks(MAX_STREAMS_PER_CONNECTION * 5) {
        let symbols_chunk = symbols_chunk.to_vec();
        let trade_sender_inner = ws_trade_sender.clone();
        let ws_semaphore_inner = websocket_semaphore.clone();
        let version_rx_clone = ws_version_tx.subscribe();

        tokio::spawn(run_ws_connection(
            market,
            symbols_chunk,
            trade_sender_inner,
            ws_semaphore_inner,
            version_rx_clone,
            current_version,
        ));
    }
}

async fn refresh_symbols(
    market: MarketType,
    http_client: &reqwest::Client,
    current_symbols: Arc<Mutex<Vec<String>>>,
    ws_version_tx: &tokio::sync::watch::Sender<usize>,
    trade_sender: tokio::sync::mpsc::Sender<(MarketType, AggTrade)>,
    websocket_semaphore: Arc<tokio::sync::Semaphore>,
) {
    let updated_symbols = match market {
        MarketType::Futures => fetch_usdt_symbols(http_client, true).await,
        MarketType::Spot => fetch_usdt_symbols(http_client, false).await,
    };

    if let Ok(updated_symbols) = updated_symbols {
        let mut symbols_lock = current_symbols.lock().await;
        if *symbols_lock != updated_symbols {
            let old_symbols = symbols_lock.clone();
            *symbols_lock = updated_symbols.clone();

            let new_symbols: Vec<String> = updated_symbols
                .iter()
                .filter(|symbol| !old_symbols.contains(symbol))
                .cloned()
                .collect();
            let removed_symbols: Vec<String> = old_symbols
                .iter()
                .filter(|symbol| !updated_symbols.contains(symbol))
                .cloned()
                .collect();

            let new_version = ws_version_tx.borrow().wrapping_add(1);
            ws_version_tx.send(new_version).ok();

            spawn_ws_connections(market, updated_symbols, trade_sender.clone(), websocket_semaphore.clone(), ws_version_tx).await;

            if !new_symbols.is_empty() {
                eprintln!("\nMarket {:?} - Found new symbols: {:?}", market, new_symbols);
            }
            if !removed_symbols.is_empty() {
                eprintln!("\nMarket {:?} - Removed symbols: {:?}", market, removed_symbols);
            }
        }
    }
}

// -----------------------------------------------------------------------------
// Failsafe Backfill Mechanism: CSV scanning, gap detection, REST backfill, and checkpointing.
// -----------------------------------------------------------------------------

fn read_csv_records(file_path: &str) -> Result<Vec<CsvRecord>, Box<dyn std::error::Error>> {
    let mut reader = csv::Reader::from_path(file_path)?;
    let mut records = Vec::new();
    for result in reader.deserialize() {
        let record: CsvRecord = result?;
        records.push(record);
    }
    Ok(records)
}

/// Scans a CSV file for gaps between consecutive aggregated records.
/// (This scans based on trade IDs.)
fn scan_csv_for_gaps(file_path: &str) -> Result<(u64, Vec<(u64, u64)>), Box<dyn std::error::Error>> {
    let records = read_csv_records(file_path)?;
    let mut gaps = Vec::new();
    let mut prev_end: Option<u64> = None;
    let mut checkpoint = 0;
    for record in records.iter() {
        if let Some(prev) = prev_end {
            if record.start_atid > prev + 1 {
                gaps.push((prev + 1, record.start_atid - 1));
            }
        }
        prev_end = Some(record.end_atid);
        checkpoint = record.end_atid;
    }
    Ok((checkpoint, gaps))
}

/// Aggregates a list of missing AggTrade records by minute.
fn aggregate_missing_trades(trades: &[AggTrade]) -> BTreeMap<u64, AggTradeAggregate> {
    let mut aggregates = BTreeMap::new();
    for trade in trades {
        let trade_time_sec = trade.trade_timestamp / 1000;
        let minute = (trade_time_sec / 60) * 60;
        let price: f64 = trade.p.parse().unwrap_or(0.0);
        let quantity: f64 = trade.q.parse().unwrap_or(0.0);
        let net_flow = if trade.m { -price * quantity } else { price * quantity };

        let entry = aggregates.entry(minute).or_insert(AggTradeAggregate {
            net_flow: 0.0,
            start_atid: None,
            end_atid: None,
            count: 0,
        });
        entry.net_flow += net_flow;
        entry.count += 1;
        if entry.start_atid.is_none() || trade.agg_trade_id < entry.start_atid.unwrap() {
            entry.start_atid = Some(trade.agg_trade_id);
        }
        if entry.end_atid.is_none() || trade.agg_trade_id > entry.end_atid.unwrap() {
            entry.end_atid = Some(trade.agg_trade_id);
        }
    }
    aggregates
}

/// Inserts missing aggregate rows into the CSV file without merging live data.
/// For each new minute from gap-filling that does not already exist in the CSV, we add a new row.
fn merge_and_update_csv(file_path: &str, new_aggregates: &BTreeMap<u64, AggTradeAggregate>) -> Result<(), Box<dyn std::error::Error>> {
    let existing = read_csv_records(file_path)?;
    // Build a map: minute -> CsvRecord from existing data.
    let mut existing_map: HashMap<u64, CsvRecord> = HashMap::new();
    for rec in existing {
        existing_map.insert(rec.timestamp, rec);
    }
    // Insert new aggregates only if that minute is not already present.
    for (&minute, agg) in new_aggregates {
        if !existing_map.contains_key(&minute) {
            let new_record = CsvRecord {
                timestamp: minute,
                start_atid: agg.start_atid.unwrap_or(0),
                end_atid: agg.end_atid.unwrap_or(0),
                net_flow: agg.net_flow,
            };
            existing_map.insert(minute, new_record);
        }
    }
    // Convert to sorted vector.
    let mut merged: Vec<CsvRecord> = existing_map.into_iter().map(|(_, rec)| rec).collect();
    merged.sort_by_key(|r| r.timestamp);
    let file = OpenOptions::new().write(true).truncate(true).open(file_path)?;
    let mut writer = csv::Writer::from_writer(file);
    writer.write_record(&["timestamp", "start_atid", "end_atid", "net_flow"])?;
    for rec in merged {
        let net_flow_str = format!("{:.2}", rec.net_flow);
        let parts: Vec<&str> = net_flow_str.split('.').collect();
        let int_part = parts[0].parse::<i64>().unwrap_or(0);
        let formatted_int = int_part.to_formatted_string(&Locale::en);
        let net_flow_formatted = if parts.len() > 1 {
            format!("{}.{}", formatted_int, parts[1])
        } else {
            formatted_int
        };
        writer.write_record(&[
            rec.timestamp.to_string(),
            rec.start_atid.to_string(),
            rec.end_atid.to_string(),
            net_flow_formatted,
        ])?;
    }
    writer.flush()?;
    Ok(())
}

/// Processes a CSV file for a given symbol and market:
/// It scans for gaps, queries the API for missing trades, aggregates them, and inserts
/// new rows only for minute boundaries that are missing.
async fn process_csv_file(
    file_path: &str,
    symbol: &str,
    market: MarketType,
    http_client: reqwest::Client,
    rate_limiter: Arc<RateLimiter<NotKeyed, InMemoryState, DefaultClock>>,
) -> Result<u64, Box<dyn std::error::Error>> {
    let (checkpoint, gaps) = scan_csv_for_gaps(file_path)?;
    if gaps.is_empty() {
        return Ok(checkpoint);
    }
    let mut combined_missing_trades = Vec::new();
    for (missing_start, missing_end) in gaps {
        eprintln!("Found gap in {} {}: {} to {}", market_str(market), symbol, missing_start, missing_end);
        let mut retry_delay = Duration::from_secs(5);
        loop {
            match fetch_missing_agg_trades(&http_client, symbol, market, missing_start, missing_end, rate_limiter.clone()).await {
                Ok(trades) => {
                    combined_missing_trades.extend(trades);
                    break;
                },
                Err(e) => {
                    eprintln!("Error fetching missing trades for {} {}: {}. Retrying in {} seconds...", market_str(market), symbol, e, retry_delay.as_secs());
                    if e.to_string().contains("429") {
                        retry_delay = std::cmp::min(retry_delay * 2, Duration::from_secs(60));
                    } else {
                        retry_delay = Duration::from_secs(5);
                    }
                    tokio::time::sleep(retry_delay).await;
                }
            }
        }
    }
    let new_aggregates = aggregate_missing_trades(&combined_missing_trades);
    merge_and_update_csv(file_path, &new_aggregates)?;
    let (new_checkpoint, _) = scan_csv_for_gaps(file_path)?;
    Ok(new_checkpoint)
}

/// Writes checkpoint data to a JSON file.
fn update_checkpoint_file(checkpoints: &BTreeMap<String, u64>, checkpoint_file: &str) -> Result<(), Box<dyn std::error::Error>> {
    let checkpoint_vec: Vec<Checkpoint> = checkpoints.iter().map(|(key, &end_atid)| {
        let mut parts = key.split(':');
        let symbol = parts.next().unwrap_or_default().to_string();
        let market = parts.next().unwrap_or("unknown").to_string();
        Checkpoint { symbol, market, end_atid_refpt: end_atid }
    }).collect();

    let json = serde_json::to_string_pretty(&checkpoint_vec)?;
    fs::write(checkpoint_file, json)?;
    Ok(())
}

// -----------------------------------------------------------------------------
// Main application entry point.
// -----------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Check for the "--disable-api-log" flag.
    let args: Vec<String> = env::args().collect();
    if args.contains(&"--disable-api-log".to_string()) {
        API_LOG_ENABLED.store(false, Ordering::Relaxed);
    }

    println!("Initializing Binance USDT aggregated trade data stream...");

    let process_start_unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let next_minute_boundary = ((process_start_unix / 60) + 1) * 60;
    println!("Process start UNIX time: {}. Next full minute boundary: {}", process_start_unix, next_minute_boundary);

    let (trade_data_sender, mut trade_data_receiver) = mpsc::channel::<(MarketType, AggTrade)>(MAX_BUFFERED_RECORDS);

    let trade_aggregates = Arc::new(Mutex::new(BTreeMap::<(String, MarketType, u64), AggTradeAggregate>::new()));
    let csv_writers = Arc::new(Mutex::new(HashMap::new()));
    let agg_trade_tracker = Arc::new(Mutex::new(AggTradeTracker::new()));
    let http_client = reqwest::Client::new();

    let spot_rate_limiter = Arc::new(RateLimiter::direct(
        Quota::per_second(NonZeroU32::new(RATE_LIMIT_REQUESTS_PER_SECOND_SPOT as u32).unwrap())
    ));
    let futures_rate_limiter = Arc::new(RateLimiter::direct(
        Quota::per_second(NonZeroU32::new(RATE_LIMIT_REQUESTS_PER_SECOND_FUTURES as u32).unwrap())
    ));

    let missing_trade_sender = trade_data_sender.clone();

    let metrics = Arc::new(Metrics {
        current_batch_records: AtomicUsize::new(0),
        last_batch_records: AtomicUsize::new(0),
        last_batch_processing_time: Mutex::new(Duration::from_secs(0)),
        start_time: Instant::now(),
        missing_gaps: AtomicUsize::new(0),
        gaps_in_queue: AtomicUsize::new(0),
    });

    let futures_symbols_list = Arc::new(Mutex::new(Vec::<String>::new()));
    let spot_symbols_list = Arc::new(Mutex::new(Vec::<String>::new()));
    let (futures_ws_version_tx, _) = tokio::sync::watch::channel(0);
    let (spot_ws_version_tx, _) = tokio::sync::watch::channel(0);
    let next_symbol_refresh_time = Arc::new(Mutex::new(SystemTime::now() + Duration::from_secs(3600)));

    {
        let metrics = metrics.clone();
        let futures_symbols_list = futures_symbols_list.clone();
        let spot_symbols_list = spot_symbols_list.clone();
        let next_symbol_refresh_time = next_symbol_refresh_time.clone();
        tokio::spawn(async move {
            let mut stdout = stdout();
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let elapsed = Instant::now().duration_since(metrics.start_time);
                let last_batch = metrics.last_batch_records.load(Ordering::Relaxed);
                let proc_time = *metrics.last_batch_processing_time.lock().await;
                let missing = metrics.missing_gaps.load(Ordering::Relaxed);
                let gaps = metrics.gaps_in_queue.load(Ordering::Relaxed);
                let formatted_elapsed = format_duration_seconds(elapsed);
                let formatted_proc_time = format_duration_millis(proc_time);
                let next_refresh_time = {
                    let lock = next_symbol_refresh_time.lock().await;
                    *lock
                };
                let now_sys = SystemTime::now();
                let countdown = if next_refresh_time > now_sys {
                    next_refresh_time.duration_since(now_sys).unwrap()
                } else {
                    Duration::from_secs(0)
                };
                let mins = countdown.as_secs() / 60;
                let secs = countdown.as_secs() % 60;
                let futures_count = futures_symbols_list.lock().await.len();
                let spot_count = spot_symbols_list.lock().await.len();
                stdout.execute(Clear(ClearType::CurrentLine)).ok();
                print!("\rElapsed: {} | Last Batch: {} records | Proc Time: {} | Missing Gaps: {} | Gaps in Queue: {} | Refresh in: {:02}m:{:02}s | Futures: {} | Spot: {}",
                    formatted_elapsed, last_batch, formatted_proc_time, missing, gaps, mins, secs, futures_count, spot_count);
                stdout.flush().ok();
            }
        });
    }

    {
        let start_minute_boundary = next_minute_boundary;
        let trade_aggregates = trade_aggregates.clone();
        let csv_writers = csv_writers.clone();
        let metrics_clone = metrics.clone();
        let agg_trade_tracker = agg_trade_tracker.clone();
        let http_client = http_client.clone();
        let futures_rate_limiter = futures_rate_limiter.clone();
        let spot_rate_limiter = spot_rate_limiter.clone();
        let missing_trade_sender = missing_trade_sender.clone();
        tokio::spawn(async move {
            let mut flush_interval = tokio::time::interval(Duration::from_secs(6));
            loop {
                tokio::select! {
                    Some((market, trade_data)) = trade_data_receiver.recv() => {
                        let trade_time_sec = trade_data.trade_timestamp / 1000;
                        let trade_minute = (trade_time_sec / 60) * 60;
                        if trade_minute < start_minute_boundary {
                            continue;
                        }
                        let price: f64 = trade_data.p.parse().unwrap_or(0.0);
                        let quantity: f64 = trade_data.q.parse().unwrap_or(0.0);
                        let net_flow = if trade_data.m { -price * quantity } else { price * quantity };

                        {
                            let mut aggregates_lock = trade_aggregates.lock().await;
                            let aggregate_entry = aggregates_lock.entry((trade_data.s.clone(), market, trade_minute))
                                .or_insert(AggTradeAggregate {
                                    net_flow: 0.0,
                                    start_atid: None,
                                    end_atid: None,
                                    count: 0,
                                });
                            aggregate_entry.net_flow += net_flow;
                            aggregate_entry.count += 1;
                            if aggregate_entry.start_atid.is_none() || trade_data.agg_trade_id < aggregate_entry.start_atid.unwrap() {
                                aggregate_entry.start_atid = Some(trade_data.agg_trade_id);
                            }
                            if aggregate_entry.end_atid.is_none() || trade_data.agg_trade_id > aggregate_entry.end_atid.unwrap() {
                                aggregate_entry.end_atid = Some(trade_data.agg_trade_id);
                            }
                        }
                        metrics_clone.current_batch_records.fetch_add(1, Ordering::Relaxed);

                        let mut tracker_lock = agg_trade_tracker.lock().await;
                        if let Some((missing_start, missing_end)) = tracker_lock.check_and_update(&trade_data.s, market, trade_data.agg_trade_id) {
                            eprintln!("Detected gap in {} {}: {} to {}", market_str(market), trade_data.s, missing_start, missing_end);
                            metrics_clone.missing_gaps.fetch_add(1, Ordering::Relaxed);
                            metrics_clone.gaps_in_queue.fetch_add(1, Ordering::Relaxed);
                            let http_client_inner = http_client.clone();
                            let symbol_clone = trade_data.s.clone();
                            let rate_limiter = match market {
                                MarketType::Futures => futures_rate_limiter.clone(),
                                MarketType::Spot => spot_rate_limiter.clone(),
                            };
                            let missing_trade_sender_clone = missing_trade_sender.clone();
                            let metrics_inner = metrics_clone.clone();
                            tokio::spawn(async move {
                                let mut retry_delay = Duration::from_secs(5);
                                loop {
                                    match fetch_missing_agg_trades(&http_client_inner, &symbol_clone, market, missing_start, missing_end, rate_limiter.clone()).await {
                                        Ok(trades) => {
                                            for trade in trades {
                                                if let Err(e) = missing_trade_sender_clone.send((market, trade)).await {
                                                    eprintln!("\nFailed to send missing trade: {}", e);
                                                }
                                            }
                                            metrics_inner.gaps_in_queue.fetch_sub(1, Ordering::Relaxed);
                                            metrics_inner.missing_gaps.fetch_sub(1, Ordering::Relaxed);
                                            break;
                                        },
                                        Err(e) => {
                                            let error_str = e.to_string();
                                            eprintln!("\nFailed to fetch missing trades: {}. Retrying in {} seconds...", error_str, retry_delay.as_secs());
                                            if error_str.contains("429") {
                                                retry_delay = std::cmp::min(retry_delay * 2, Duration::from_secs(60));
                                            } else {
                                                retry_delay = Duration::from_secs(5);
                                            }
                                            tokio::time::sleep(retry_delay).await;
                                        }
                                    }
                                }
                            });
                        }
                    },
                    _ = flush_interval.tick() => {
                        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                        let mut aggregates_to_flush = Vec::new();
                        {
                            let aggregates_lock = trade_aggregates.lock().await;
                            for (key, _) in aggregates_lock.iter() {
                                let &(_, _, minute_ts) = key;
                                if minute_ts + 60 + FINALIZATION_BUFFER_SECONDS <= now_unix {
                                    aggregates_to_flush.push(key.clone());
                                }
                            }
                        }
                        if !aggregates_to_flush.is_empty() {
                            let flush_start = Instant::now();
                            let mut batch_record_count = 0;
                            for key in aggregates_to_flush {
                                if let Some(aggregate) = trade_aggregates.lock().await.remove(&key) {
                                    batch_record_count += aggregate.count;
                                    let (symbol, market, minute_ts) = key;
                                    let market_str_val = market_str(market);
                                    let csv_folder = format!("./{}", market_str_val);
                                    fs::create_dir_all(&csv_folder).expect("Failed to create directory");
                                    let filename = format!("{}/{}_{}.csv", csv_folder, symbol.to_uppercase(), market_str_val);
                                    let mut csv_writers_lock = csv_writers.lock().await;
                                    let writer_entry = csv_writers_lock.entry((symbol.clone(), market))
                                        .or_insert_with(|| {
                                            let file = OpenOptions::new()
                                                .create(true)
                                                .append(true)
                                                .open(&filename)
                                                .expect("Failed to open CSV file");
                                            let mut writer = Writer::from_writer(file);
                                            if let Ok(metadata) = fs::metadata(&filename) {
                                                if metadata.len() == 0 {
                                                    writer.write_record(&["timestamp", "start_atid", "end_atid", "net_flow"])
                                                        .expect("Failed to write CSV header");
                                                }
                                            }
                                            SymbolWriter { writer }
                                        });
                                    writer_entry.writer.write_record(&[
                                        minute_ts.to_string(),
                                        aggregate.start_atid.unwrap_or(0).to_string(),
                                        aggregate.end_atid.unwrap_or(0).to_string(),
                                        format!("{:.2}", aggregate.net_flow),
                                    ]).expect("Failed to write CSV record");
                                    writer_entry.writer.flush().expect("Failed to flush CSV writer");
                                }
                            }
                            let flush_duration = Instant::now().duration_since(flush_start);
                            *metrics_clone.last_batch_processing_time.lock().await = flush_duration;
                            metrics_clone.last_batch_records.store(batch_record_count, Ordering::Relaxed);
                        }
                    }
                }
            }
        });
    }

    let (ws_trade_sender, mut ws_trade_receiver) = mpsc::channel(MAX_BUFFERED_RECORDS);
    let websocket_semaphore = Arc::new(tokio::sync::Semaphore::new(10));

    {
        let http_client = http_client.clone();
        let futures_symbols_list = futures_symbols_list.clone();
        let spot_symbols_list = spot_symbols_list.clone();
        let futures_ws_version_tx = futures_ws_version_tx.clone();
        let spot_ws_version_tx = spot_ws_version_tx.clone();

        println!("Fetching futures symbols...");
        let futures_symbols_fetched = fetch_usdt_symbols(&http_client, true).await.unwrap_or_else(|e| {
            eprintln!("Error fetching futures symbols: {}", e);
            vec![]
        });
        {
            let mut futures_list_lock = futures_symbols_list.lock().await;
            *futures_list_lock = futures_symbols_fetched.clone();
        }
        println!("Fetching spot symbols...");
        let spot_symbols_fetched = fetch_usdt_symbols(&http_client, false).await.unwrap_or_else(|e| {
            eprintln!("Error fetching spot symbols: {}", e);
            vec![]
        });
        {
            let mut spot_list_lock = spot_symbols_list.lock().await;
            *spot_list_lock = spot_symbols_fetched.clone();
        }
        {
            spawn_ws_connections(MarketType::Futures, futures_symbols_fetched, ws_trade_sender.clone(), websocket_semaphore.clone(), &futures_ws_version_tx).await;
        }
        {
            spawn_ws_connections(MarketType::Spot, spot_symbols_fetched, ws_trade_sender.clone(), websocket_semaphore.clone(), &spot_ws_version_tx).await;
        }
    }

    {
        let http_client = http_client.clone();
        let futures_symbols_list = futures_symbols_list.clone();
        let spot_symbols_list = spot_symbols_list.clone();
        let futures_ws_version_tx = futures_ws_version_tx.clone();
        let spot_ws_version_tx = spot_ws_version_tx.clone();
        let next_symbol_refresh_time = next_symbol_refresh_time.clone();
        let trade_sender_clone = ws_trade_sender.clone();
        let websocket_semaphore = websocket_semaphore.clone();
        tokio::spawn(async move {
            loop {
                let now = SystemTime::now();
                let now_secs = now.duration_since(UNIX_EPOCH).unwrap().as_secs();
                let next_hour = ((now_secs / 3600) + 1) * 3600;
                let refresh_time = SystemTime::UNIX_EPOCH + Duration::from_secs(next_hour);
                {
                    let mut refresh_lock = next_symbol_refresh_time.lock().await;
                    *refresh_lock = refresh_time;
                }
                let wait_duration = refresh_time.duration_since(SystemTime::now()).unwrap_or(Duration::from_secs(0));
                tokio::time::sleep(wait_duration).await;
                refresh_symbols(MarketType::Futures, &http_client, futures_symbols_list.clone(), &futures_ws_version_tx, trade_sender_clone.clone(), websocket_semaphore.clone()).await;
                refresh_symbols(MarketType::Spot, &http_client, spot_symbols_list.clone(), &spot_ws_version_tx, trade_sender_clone.clone(), websocket_semaphore.clone()).await;
            }
        });
    }

    {
        let http_client = http_client.clone();
        let futures_rate_limiter_clone = futures_rate_limiter.clone();
        let spot_rate_limiter_clone = spot_rate_limiter.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(120)).await;
            let mut checkpoints: BTreeMap<String, u64> = BTreeMap::new();
            for market in &[MarketType::Futures, MarketType::Spot] {
                let folder = format!("./{}", market_str(*market));
                if let Ok(entries) = fs::read_dir(&folder) {
                    for entry in entries {
                        if let Ok(entry) = entry {
                            let path = entry.path();
                            if path.extension().and_then(|s| s.to_str()) == Some("csv") {
                                let file_path = path.to_str().unwrap().to_string();
                                let file_name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                                let parts: Vec<&str> = file_name.split('_').collect();
                                if parts.len() >= 2 {
                                    let symbol = parts[0].to_string();
                                    let rate_limiter = match market {
                                        MarketType::Futures => futures_rate_limiter_clone.clone(),
                                        MarketType::Spot => spot_rate_limiter_clone.clone(),
                                    };
                                    match process_csv_file(&file_path, &symbol, *market, http_client.clone(), rate_limiter).await {
                                        Ok(new_checkpoint) => {
                                            checkpoints.insert(format!("{}:{}", symbol, market_str(*market)), new_checkpoint);
                                            eprintln!("Updated checkpoint for {} {}: {}", market_str(*market), symbol, new_checkpoint);
                                        },
                                        Err(e) => {
                                            eprintln!("Error processing file {}: {}", file_path, e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Err(e) = update_checkpoint_file(&checkpoints, "checkpoint.json") {
                eprintln!("Error updating checkpoint.json: {}", e);
            } else {
                eprintln!("Checkpoint file updated.");
            }
        });
    }

    while let Some((market, trade_data)) = ws_trade_receiver.recv().await {
        if let Err(e) = trade_data_sender.send((market, trade_data)).await {
            eprintln!("\nTrade data channel error: {}", e);
        }
    }

    Ok(())
}
