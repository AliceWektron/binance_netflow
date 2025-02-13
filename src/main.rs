use std::collections::{HashMap, BTreeMap};
use std::fs::{self, OpenOptions};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH, Instant};

use csv::Writer;
use futures::StreamExt;
use serde::Deserialize;
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use num_format::{Locale, ToFormattedString};

use std::sync::atomic::{AtomicUsize, Ordering};
use crossterm::{
    ExecutableCommand,
    terminal::{Clear, ClearType},
};
use std::io::{stdout, Write};

use chrono::Utc;

// --- Governor (rate limiting) dependencies ---
use governor::{Quota, RateLimiter, clock::DefaultClock, state::{NotKeyed, InMemoryState}};
use std::num::NonZeroU32;

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

/// Logs an HTTP request message with a timestamp to "http_requests.log".
async fn log_http_request(message: &str) {
    use tokio::io::AsyncWriteExt;
    let timestamp = Utc::now().to_rfc3339();
    let log_line = format!("{} - {}\n", timestamp, message);
    if let Ok(mut file) = tokio::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open("http_requests.log")
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
    p: String, // Price as a string (to be parsed)
    q: String, // Quantity as a string (to be parsed)
    m: bool,   // Indicates whether the buyer is the market maker.
    #[serde(rename = "a")]
    agg_trade_id: u64,  // Aggregated trade ID
    #[serde(rename = "T")]
    trade_timestamp: u64, // Trade timestamp (in milliseconds)
    #[serde(flatten)]
    _extra: serde_json::Value, // Any additional fields
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

/// Represents the two market types being monitored.
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

/// Tracks the last seen trade ID per symbol/market pair to detect gaps.
struct AggTradeTracker {
    last_trade_ids: HashMap<(String, MarketType), u64>,
}

impl AggTradeTracker {
    fn new() -> Self {
        Self {
            last_trade_ids: HashMap::new(),
        }
    }

    /// Checks for missing trade IDs and updates the tracker.
    /// Returns the range (start, end) of missing trade IDs if a gap is found.
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

/// Runtime metrics for monitoring the application.
struct Metrics {
    current_batch_records: AtomicUsize,
    last_batch_records: AtomicUsize,
    last_batch_processing_time: Mutex<Duration>,
    start_time: Instant,
    missing_gaps: AtomicUsize,
    gaps_in_queue: AtomicUsize,
}

// -----------------------------------------------------------------------------
// Functions for fetching trade data via REST and handling WebSocket connections.
// -----------------------------------------------------------------------------

/// Fetches the list of USDT symbols from Binance for the given market type (futures or spot).
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

/// Fetches missing aggregated trades via REST for a specific symbol and trade ID range.
///
/// The function makes batched REST API calls and uses a rate limiter to stay within API limits.
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
        // Wait for the rate limiter permit.
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
        log_http_request(&format!("Sending request: {}", url)).await;
        let response = client.get(&url).send().await?;
        let status = response.status();
        let text = response.text().await?;
        log_http_request(&format!("Received response for {}: status {} Body: {}", url, status, text)).await;
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
        // Check if the last trade fetched exceeds our end_id.
        if let Some(last_trade) = rest_records.last() {
            if last_trade.agg_trade_id > end_id {
                let filtered: Vec<RestAggTrade> = rest_records.into_iter()
                    .filter(|r| r.agg_trade_id <= end_id)
                    .collect();
                if filtered.is_empty() {
                    break;
                }
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
        // Append all records from this batch.
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

/// Runs a WebSocket connection for a list of symbols in a specified market.
///
/// This function connects to Binance's stream endpoint, listens for aggregated trade data,
/// and sends each parsed trade to the provided channel. It also monitors the symbol version
/// (using a watch channel) so that stale connections exit when symbols change.
async fn run_ws_connection(
    market: MarketType,
    symbol_list: Vec<String>,
    trade_sender: tokio::sync::mpsc::Sender<(MarketType, AggTrade)>,
    connection_semaphore: Arc<tokio::sync::Semaphore>,
    symbol_version_rx: tokio::sync::watch::Receiver<usize>,
    current_symbol_version: usize,
) {
    let mut reconnect_attempts = 0;
    // Build the stream path by joining symbol streams with a "/".
    let stream_paths = symbol_list.iter()
        .map(|symbol| format!("{}@aggTrade", symbol))
        .collect::<Vec<_>>()
        .join("/");
    let url = match market {
        MarketType::Futures => format!("wss://fstream.binance.com/stream?streams={}", stream_paths),
        MarketType::Spot => format!("wss://stream.binance.com:9443/stream?streams={}", stream_paths),
    };

    loop {
        // Check if the symbol version has changed and exit if so.
        if *symbol_version_rx.borrow() != current_symbol_version {
            break;
        }
        // Acquire a connection permit.
        let permit = match connection_semaphore.acquire().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        match connect_async(&url).await {
            Ok((stream, _)) => {
                reconnect_attempts = 0;
                let (_, mut incoming_messages) = stream.split();
                // Process incoming WebSocket messages.
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

/// Establishes WebSocket connections for the specified market.
///
/// The function splits the full symbol list into smaller chunks (to avoid exceeding
/// connection limits) and spawns an asynchronous task for each chunk. Each spawned task
/// creates a WebSocket connection that subscribes to aggregated trade streams for its chunk of symbols.
/// The provided watch channel (`ws_version_tx`) signals when the symbol list is updated,
/// causing outdated connections to exit.
async fn spawn_ws_connections(
    market: MarketType,
    symbols: Vec<String>,
    ws_trade_sender: tokio::sync::mpsc::Sender<(MarketType, AggTrade)>,
    websocket_semaphore: Arc<tokio::sync::Semaphore>,
    ws_version_tx: &tokio::sync::watch::Sender<usize>,
) {
    // Subscribe to the current version so that connection tasks know when to exit.
    let version_rx = ws_version_tx.subscribe();
    let current_version = *version_rx.borrow();

    // Split the symbol list into manageable chunks.
    for symbols_chunk in symbols.chunks(MAX_STREAMS_PER_CONNECTION * 5) {
        let symbols_chunk = symbols_chunk.to_vec();
        let trade_sender_inner = ws_trade_sender.clone();
        let ws_semaphore_inner = websocket_semaphore.clone();
        let version_rx_clone = ws_version_tx.subscribe();

        // Spawn a new task to handle this chunk of symbols.
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

/// Updates the list of tradable symbols for the given market.
///
/// This function fetches the latest symbols from Binance’s API (for futures or spot)
/// and compares the new list with the current one. If the symbol list has changed:
///   - It updates the shared symbol list,
///   - Increments the version in the watch channel (causing old WebSocket connections to stop),
///   - And spawns new WebSocket connections using the updated symbol list.
/// It also prints out which symbols were added or removed.
async fn refresh_symbols(
    market: MarketType,
    http_client: &reqwest::Client,
    current_symbols: Arc<Mutex<Vec<String>>>,
    ws_version_tx: &tokio::sync::watch::Sender<usize>,
    trade_sender: tokio::sync::mpsc::Sender<(MarketType, AggTrade)>,
    websocket_semaphore: Arc<tokio::sync::Semaphore>,
) {
    // Fetch updated symbols for the given market.
    let updated_symbols = match market {
        MarketType::Futures => fetch_usdt_symbols(http_client, true).await,
        MarketType::Spot => fetch_usdt_symbols(http_client, false).await,
    };

    if let Ok(updated_symbols) = updated_symbols {
        let mut symbols_lock = current_symbols.lock().await;
        // Check if the symbol list has changed.
        if *symbols_lock != updated_symbols {
            let old_symbols = symbols_lock.clone();
            *symbols_lock = updated_symbols.clone();

            // Determine which symbols are new and which were removed.
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

            // Increment version so that existing connections become obsolete.
            let new_version = ws_version_tx.borrow().wrapping_add(1);
            ws_version_tx.send(new_version).ok();

            // Spawn WebSocket connections using the new symbol list.
            spawn_ws_connections(market, updated_symbols, trade_sender.clone(), websocket_semaphore.clone(), ws_version_tx).await;

            // Log which symbols were added or removed using eprintln.
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
// Main application entry point.
// -----------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("Initializing Binance USDT aggregated trade data stream...");

    // Determine process start time and the next full minute boundary (used for aggregation).
    let process_start_unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let next_minute_boundary = ((process_start_unix / 60) + 1) * 60;
    println!(
        "Process start UNIX time: {}. Next full minute boundary: {}",
        process_start_unix, next_minute_boundary
    );

    // Create a channel for trade data coming from both WebSocket and REST (for missing trades).
    let (trade_data_sender, mut trade_data_receiver) = mpsc::channel::<(MarketType, AggTrade)>(MAX_BUFFERED_RECORDS);

    // Shared in-memory storage for aggregated trades, keyed by (symbol, market, minute timestamp).
    let trade_aggregates = Arc::new(Mutex::new(BTreeMap::<(String, MarketType, u64), AggTradeAggregate>::new()));
    // Shared CSV writers for persisting trade aggregates per symbol/market.
    let csv_writers = Arc::new(Mutex::new(HashMap::new()));
    // Tracker for detecting missing trade IDs (gaps).
    let agg_trade_tracker = Arc::new(Mutex::new(AggTradeTracker::new()));
    let http_client = reqwest::Client::new();

    // Create rate limiters (using governor) for REST API requests for spot and futures.
    let spot_rate_limiter = Arc::new(RateLimiter::direct(
        Quota::per_second(NonZeroU32::new(RATE_LIMIT_REQUESTS_PER_SECOND_SPOT as u32).unwrap())
    ));
    let futures_rate_limiter = Arc::new(RateLimiter::direct(
        Quota::per_second(NonZeroU32::new(RATE_LIMIT_REQUESTS_PER_SECOND_FUTURES as u32).unwrap())
    ));

    // Clone the trade data sender to be used for missing trades fetched via REST.
    let missing_trade_sender = trade_data_sender.clone();

    // Metrics for runtime monitoring.
    let metrics = Arc::new(Metrics {
        current_batch_records: AtomicUsize::new(0),
        last_batch_records: AtomicUsize::new(0),
        last_batch_processing_time: Mutex::new(Duration::from_secs(0)),
        start_time: Instant::now(),
        missing_gaps: AtomicUsize::new(0),
        gaps_in_queue: AtomicUsize::new(0),
    });

    // Shared symbol lists for futures and spot markets.
    let futures_symbols_list = Arc::new(Mutex::new(Vec::<String>::new()));
    let spot_symbols_list = Arc::new(Mutex::new(Vec::<String>::new()));
    // Watch channels to signal when the symbol list is updated.
    let (futures_ws_version_tx, _) = tokio::sync::watch::channel(0);
    let (spot_ws_version_tx, _) = tokio::sync::watch::channel(0);
    // Next scheduled symbol refresh time.
    let next_symbol_refresh_time = Arc::new(Mutex::new(SystemTime::now() + Duration::from_secs(3600)));

    // Spawn a task that prints runtime metrics (e.g., elapsed time, batch sizes, refresh countdown) every second.
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

    // Process incoming trade records: update aggregates, detect gaps, and flush aggregates to CSV.
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
            // Set an interval for flushing aggregates to disk.
            let mut flush_interval = tokio::time::interval(Duration::from_secs(6));
            loop {
                tokio::select! {
                    // Process each incoming trade record.
                    Some((market, trade_data)) = trade_data_receiver.recv() => {
                        let trade_time_sec = trade_data.trade_timestamp / 1000;
                        let trade_minute = (trade_time_sec / 60) * 60;
                        // Skip trades that occurred before the aggregation boundary.
                        if trade_minute < start_minute_boundary {
                            continue;
                        }
                        let price: f64 = trade_data.p.parse().unwrap_or(0.0);
                        let quantity: f64 = trade_data.q.parse().unwrap_or(0.0);
                        // Compute net flow: subtract if the market maker flag is true.
                        let net_flow = if trade_data.m { -price * quantity } else { price * quantity };

                        {
                            // Update or initialize the aggregate record for the current minute.
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
                            // Record the earliest and latest trade IDs for gap detection.
                            if aggregate_entry.start_atid.is_none() || trade_data.agg_trade_id < aggregate_entry.start_atid.unwrap() {
                                aggregate_entry.start_atid = Some(trade_data.agg_trade_id);
                            }
                            if aggregate_entry.end_atid.is_none() || trade_data.agg_trade_id > aggregate_entry.end_atid.unwrap() {
                                aggregate_entry.end_atid = Some(trade_data.agg_trade_id);
                            }
                        }
                        // Update processing metrics.
                        metrics_clone.current_batch_records.fetch_add(1, Ordering::Relaxed);

                        // Check for gaps in trade IDs to detect missing trades.
                        let mut tracker_lock = agg_trade_tracker.lock().await;
                        if let Some((missing_start, missing_end)) = tracker_lock.check_and_update(&trade_data.s, market, trade_data.agg_trade_id) {
                            metrics_clone.missing_gaps.fetch_add(1, Ordering::Relaxed);
                            metrics_clone.gaps_in_queue.fetch_add(1, Ordering::Relaxed);
                            let http_client_inner = http_client.clone();
                            let symbol_clone = trade_data.s.clone();
                            // Choose the appropriate rate limiter based on the market.
                            let rate_limiter = match market {
                                MarketType::Futures => futures_rate_limiter.clone(),
                                MarketType::Spot => spot_rate_limiter.clone(),
                            };
                            let missing_trade_sender_clone = missing_trade_sender.clone();
                            let metrics_inner = metrics_clone.clone();
                            // Spawn a task to fetch the missing trades via REST.
                            tokio::spawn(async move {
                                let mut retry_delay = Duration::from_secs(5);
                                loop {
                                    match fetch_missing_agg_trades(&http_client_inner, &symbol_clone, market, missing_start, missing_end, rate_limiter.clone()).await {
                                        Ok(trades) => {
                                            // Send each missing trade back into the processing pipeline.
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
                                            // Apply exponential backoff for rate-limit errors.
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
                    // Periodically flush aggregated data to CSV files.
                    _ = flush_interval.tick() => {
                        let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
                        let mut aggregates_to_flush = Vec::new();
                        {
                            // Identify aggregates that are older than a full minute plus a finalization buffer.
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
                            // Remove and write each aggregate to its corresponding CSV file.
                            for key in aggregates_to_flush {
                                if let Some(aggregate) = trade_aggregates.lock().await.remove(&key) {
                                    batch_record_count += aggregate.count;
                                    let (symbol, market, minute_ts) = key;
                                    let market_str = match market {
                                        MarketType::Futures => "futures",
                                        MarketType::Spot => "spot",
                                    };
                                    let csv_folder = format!("./{}", market_str);
                                    fs::create_dir_all(&csv_folder).expect("Failed to create directory");
                                    let filename = format!("{}/{}_{}.csv", csv_folder, symbol.to_uppercase(), market_str);
                                    let mut csv_writers_lock = csv_writers.lock().await;
                                    let writer_entry = csv_writers_lock.entry((symbol.clone(), market))
                                        .or_insert_with(|| {
                                            let file = OpenOptions::new()
                                                .create(true)
                                                .append(true)
                                                .open(&filename)
                                                .expect("Failed to open CSV file");
                                            let mut writer = Writer::from_writer(file);
                                            // Write header if the file is new.
                                            if let Ok(metadata) = fs::metadata(&filename) {
                                                if metadata.len() == 0 {
                                                    writer.write_record(&["timestamp", "start_atid", "end_atid", "net_flow"])
                                                        .expect("Failed to write CSV header");
                                                }
                                            }
                                            SymbolWriter { writer }
                                        });
                                    // Format the net flow value with thousands separators.
                                    let net_flow_str = format!("{:.2}", aggregate.net_flow);
                                    let parts: Vec<&str> = net_flow_str.split('.').collect();
                                    let int_part = parts[0].parse::<i64>().unwrap_or(0);
                                    let formatted_int = int_part.to_formatted_string(&Locale::en);
                                    let net_flow_formatted = if parts.len() > 1 {
                                        format!("{}.{}", formatted_int, parts[1])
                                    } else {
                                        formatted_int
                                    };
                                    writer_entry.writer.write_record(&[
                                        minute_ts.to_string(),
                                        aggregate.start_atid.unwrap_or(0).to_string(),
                                        aggregate.end_atid.unwrap_or(0).to_string(),
                                        net_flow_formatted,
                                    ]).expect("Failed to write CSV record");
                                    writer_entry.writer.flush().expect("Failed to flush CSV writer");
                                }
                            }
                            let flush_duration = Instant::now().duration_since(flush_start);
                            // Update flush metrics.
                            *metrics_clone.last_batch_processing_time.lock().await = flush_duration;
                            metrics_clone.last_batch_records.store(batch_record_count, Ordering::Relaxed);
                        }
                    }
                }
            }
        });
    }

    // Create a dedicated channel for receiving trade data from WebSocket connections.
    let (ws_trade_sender, mut ws_trade_receiver) = mpsc::channel(MAX_BUFFERED_RECORDS);
    let websocket_semaphore = Arc::new(tokio::sync::Semaphore::new(10));

    // Fetch the initial symbol lists and spawn WebSocket connections.
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
        println!("Found {} futures symbols", futures_symbols_fetched.len());
        {
            let mut futures_list_lock = futures_symbols_list.lock().await;
            *futures_list_lock = futures_symbols_fetched.clone();
        }
        println!("Fetching spot symbols...");
        let spot_symbols_fetched = fetch_usdt_symbols(&http_client, false).await.unwrap_or_else(|e| {
            eprintln!("Error fetching spot symbols: {}", e);
            vec![]
        });
        println!("Found {} spot symbols", spot_symbols_fetched.len());
        {
            let mut spot_list_lock = spot_symbols_list.lock().await;
            *spot_list_lock = spot_symbols_fetched.clone();
        }
        {
            // Spawn WebSocket connections for futures symbols.
            spawn_ws_connections(MarketType::Futures, futures_symbols_fetched, ws_trade_sender.clone(), websocket_semaphore.clone(), &futures_ws_version_tx).await;
        }
        {
            // Spawn WebSocket connections for spot symbols.
            spawn_ws_connections(MarketType::Spot, spot_symbols_fetched, ws_trade_sender.clone(), websocket_semaphore.clone(), &spot_ws_version_tx).await;
        }
    }

    // Schedule symbol list refresh every hour to capture any changes.
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
                // Compute the next full hour as the refresh time.
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
                // Refresh the symbols for both futures and spot markets.
                refresh_symbols(MarketType::Futures, &http_client, futures_symbols_list.clone(), &futures_ws_version_tx, trade_sender_clone.clone(), websocket_semaphore.clone()).await;
                refresh_symbols(MarketType::Spot, &http_client, spot_symbols_list.clone(), &spot_ws_version_tx, trade_sender_clone.clone(), websocket_semaphore.clone()).await;
            }
        });
    }

    // Forward messages from the WebSocket channel to the main trade data channel.
    while let Some((market, trade_data)) = ws_trade_receiver.recv().await {
        if let Err(e) = trade_data_sender.send((market, trade_data)).await {
            eprintln!("\nTrade data channel error: {}", e);
        }
    }

    Ok(())
}

