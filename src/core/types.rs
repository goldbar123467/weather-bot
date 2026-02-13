use serde::Deserialize;

// ── AI Decision ──

#[derive(Debug, Deserialize)]
pub struct TradeDecision {
    pub action: Action,
    pub side: Option<Side>,
    pub shares: Option<u32>,
    pub max_price_cents: Option<u32>,
    pub reasoning: String,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "UPPERCASE")]
pub enum Action {
    Buy,
    Pass,
}

#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Yes,
    No,
}

// ── Market Data ──

#[derive(Debug, Clone)]
pub struct MarketState {
    pub ticker: String,
    pub event_ticker: String,
    pub title: String,
    pub yes_bid: Option<u32>,
    pub yes_ask: Option<u32>,
    pub no_bid: Option<u32>,
    pub no_ask: Option<u32>,
    pub last_price: Option<u32>,
    pub volume: u64,
    pub volume_24h: u64,
    pub open_interest: u64,
    pub expiration_time: String,
    pub minutes_to_expiry: f64,
}

#[derive(Debug)]
pub struct Orderbook {
    pub yes: Vec<(u32, u32)>,
    pub no: Vec<(u32, u32)>,
}

// ── Weather Data ──

#[derive(Debug, Clone)]
pub struct HourlyForecast {
    pub time: String,
    pub temperature_f: f64,
}

#[derive(Debug, Clone)]
pub struct EnsembleForecast {
    pub model_count: usize,
    pub mean_high: f64,
    pub min_high: f64,
    pub max_high: f64,
    pub std_dev: f64,
    pub p10: f64,
    pub p25: f64,
    pub p75: f64,
    pub p90: f64,
}

#[derive(Debug, Clone)]
pub struct TempBucketProbability {
    pub label: String,
    pub lower: f64,
    pub upper: f64,
    pub probability: f64,
}

#[derive(Debug, Clone)]
pub enum ForecastConfidence {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone)]
pub struct WeatherSnapshot {
    pub city: String,
    pub current_temp_f: f64,
    pub nws_forecast_high: Option<f64>,
    pub nws_forecast_low: Option<f64>,
    pub nws_short_forecast: Option<String>,
    pub open_meteo_forecast_high: f64,
    pub hourly_forecasts: Vec<HourlyForecast>,
    pub ensemble: Option<EnsembleForecast>,
    pub bucket_probabilities: Vec<TempBucketProbability>,
    pub confidence: ForecastConfidence,
}

// ── Orders & Positions ──

#[derive(Debug)]
pub struct OrderResult {
    pub order_id: String,
    pub status: String,
}

#[derive(Debug)]
pub struct OrderRequest {
    pub ticker: String,
    pub side: Side,
    pub shares: u32,
    pub price_cents: u32,
}

#[derive(Debug)]
pub struct RestingOrder {
    pub order_id: String,
    pub ticker: String,
}

#[derive(Debug)]
pub struct Position {
    pub ticker: String,
    pub side: Side,
    pub count: u32,
}

#[derive(Debug)]
pub struct Settlement {
    pub ticker: String,
    pub side: Side,
    pub count: u32,
    pub price_cents: u32,
    pub result: String,
    pub pnl_cents: i64,
    pub settled_time: String,
    pub market_result: String,
}

// ── Stats ──

#[derive(Debug)]
pub struct Stats {
    pub total_trades: u32,
    pub wins: u32,
    pub losses: u32,
    pub win_rate: f64,
    pub total_pnl_cents: i64,
    pub today_pnl_cents: i64,
    pub current_streak: i32,
    pub max_drawdown_cents: i64,
    pub avg_win_cents: f64,
    pub avg_loss_cents: f64,
}

// ── Prompt Context ──

#[derive(Debug)]
pub struct DecisionContext {
    pub prompt_md: String,
    pub stats: Stats,
    pub last_n_trades: Vec<LedgerRow>,
    pub market: MarketState,
    pub orderbook: Orderbook,
    pub weather: Option<WeatherSnapshot>,
}

#[derive(Debug, Clone)]
pub struct LedgerRow {
    pub timestamp: String,
    pub ticker: String,
    pub side: String,
    pub shares: u32,
    pub price: u32,
    pub result: String,
    pub pnl_cents: i64,
    pub cumulative_cents: i64,
    pub order_id: String,
}

// ── Config ──

pub struct Config {
    pub max_shares: u32,
    pub max_daily_loss_cents: i64,
    pub max_consecutive_losses: u32,
    pub min_balance_cents: u64,
    pub min_minutes_to_expiry: f64,
    pub paper_trade: bool,
    pub confirm_live: bool,
    pub series_ticker: String,
    pub kalshi_base_url: String,
    pub openrouter_api_key: String,
    pub kalshi_key_id: String,
    pub kalshi_private_key_pem: String,
    pub lockfile_path: String,
    pub weather_city: String,
    pub weather_lat: f64,
    pub weather_lon: f64,
    pub weather_timezone: String,
}

impl Config {
    pub fn from_env() -> anyhow::Result<Self> {
        let pem_path = std::env::var("KALSHI_PRIVATE_KEY_PATH")
            .unwrap_or_else(|_| "./kalshi_private_key.pem".into());
        let pem = std::fs::read_to_string(&pem_path).unwrap_or_default();

        Ok(Self {
            max_shares: 5,
            max_daily_loss_cents: 1000,
            max_consecutive_losses: 7,
            min_balance_cents: 500,
            min_minutes_to_expiry: 2.0,
            paper_trade: std::env::var("PAPER_TRADE")
                .map(|v| v != "false")
                .unwrap_or(true),
            confirm_live: std::env::var("CONFIRM_LIVE")
                .map(|v| v == "true")
                .unwrap_or(false),
            series_ticker: std::env::var("KALSHI_SERIES_TICKER").unwrap_or_default(),
            kalshi_base_url: std::env::var("KALSHI_BASE_URL")
                .unwrap_or_else(|_| "https://api.elections.kalshi.com".into()),
            openrouter_api_key: std::env::var("OPENROUTER_API_KEY").unwrap_or_default(),
            kalshi_key_id: std::env::var("KALSHI_API_KEY_ID").unwrap_or_default(),
            kalshi_private_key_pem: pem,
            lockfile_path: "/tmp/kalshi-bot.lock".into(),
            weather_city: std::env::var("WEATHER_CITY")
                .unwrap_or_else(|_| "New York".into()),
            weather_lat: std::env::var("WEATHER_LAT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(40.7128),
            weather_lon: std::env::var("WEATHER_LON")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(-74.0060),
            weather_timezone: std::env::var("WEATHER_TIMEZONE")
                .unwrap_or_else(|_| "America/New_York".into()),
        })
    }
}
