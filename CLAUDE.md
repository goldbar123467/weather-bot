# CLAUDE.md — Kalshi Weather Trading Bot (Rust)

## What This Is

A Rust cron job that fetches weather forecasts from NWS + Open-Meteo ensemble models, compares ensemble probabilities against Kalshi market prices for daily high temperature contracts, and trades when it finds an edge. Pure deterministic strategy — no LLM calls, $0/cycle.

## Architecture

```
                    ┌─────────────────────────────┐
                    │         CORE DOMAIN          │
                    │  (pure Rust, no IO, no deps) │
                    │                              │
                    │  • engine.rs  (10-step cycle) │
                    │  • rules_brain.rs (strategy)  │
                    │  • indicators.rs (helpers)    │
                    │  • risk.rs    (limit checks)  │
                    │  • stats.rs   (ledger math)   │
                    │  • types.rs   (domain types)  │
                    └──────────┬──────────────────┘
                               │ uses traits (ports)
            ┌──────────────────┼──────────────────┐
            │                  │                   │
    ┌───────▼──────┐   ┌──────▼───────┐   ┌──────▼────────┐
    │  Port:       │   │  Port:       │   │  Port:        │
    │  Exchange    │   │  Brain       │   │  WeatherFeed  │
    │              │   │              │   │               │
    │  • market()  │   │  • decide()  │   │  • forecast() │
    │  • orderbook │   │              │   │               │
    │  • order()   │   └──────┬───────┘   └──────┬────────┘
    │  • positions │          │                   │
    │  • cancel()  │   ┌──────▼───────┐   ┌──────▼────────┐
    │  • settle()  │   │  Adapter:    │   │  Adapter:     │
    │  • balance() │   │  RulesBrain  │   │  WeatherClient│
    └───────┬──────┘   │  (pure core) │   │  (NWS+OM)    │
            │          └──────────────┘   └───────────────┘
    ┌───────▼──────┐
    │  Adapter:    │
    │  KalshiApi   │   Storage: plain filesystem — read/append markdown files.
    └──────────────┘
```

### Why Hexagonal

- **Testing**: Mock every adapter. Core domain is pure functions — unit test with zero network.
- **Swappable**: New exchange → implement `Exchange`. Want LLM back → swap `RulesBrain` for `OpenRouterClient`. New weather API → implement `WeatherFeed`.
- **Clarity**: Network → adapter. Pure logic → core. No ambiguity.

## Tech Stack

- **Language**: Rust 2021
- **Async**: Tokio
- **HTTP**: reqwest
- **Serialization**: serde / serde_json
- **Crypto**: rsa (RSA-PSS SHA-256), sha2, base64
- **Time**: chrono
- **Config**: dotenv
- **Error handling**: anyhow

## Project Structure

```
weather-bot/
├── Cargo.toml
├── .env                             # Kalshi creds + weather config
├── CLAUDE.md
├── brain/
│   ├── prompt.md                    # Strategy reference (for LLM adapter if re-enabled)
│   ├── ledger.md                    # Append-only trade log (Rust writes)
│   └── stats.md                     # Computed stats (Rust writes)
├── src/
│   ├── main.rs                      # Entry point — wires adapters, lockfile
│   ├── safety.rs                    # Lockfile, startup validation, live-mode gate
│   ├── storage.rs                   # Read/write brain/*.md files
│   ├── core/
│   │   ├── engine.rs                # Orchestration: the 10-step cycle
│   │   ├── rules_brain.rs           # Deterministic: ensemble prob vs market implied
│   │   ├── indicators.rs            # forecast_agreement(), ensemble_summary()
│   │   ├── risk.rs                  # Pure risk checks — no IO
│   │   ├── stats.rs                 # Compute stats from ledger — no IO
│   │   └── types.rs                 # All domain types, enums, structs
│   ├── ports/
│   │   ├── exchange.rs              # Exchange trait
│   │   ├── brain.rs                 # Brain trait
│   │   └── weather_feed.rs          # WeatherFeed trait
│   └── adapters/
│       ├── kalshi/
│       │   ├── auth.rs              # RSA-PSS signing
│       │   ├── client.rs            # Implements Exchange trait
│       │   └── types.rs             # Kalshi API response structs
│       ├── weather.rs               # NWS + Open-Meteo (implements WeatherFeed)
│       └── openrouter.rs            # LLM adapter (preserved, not wired)
└── logs/
```

## Ports (Traits)

### ports/exchange.rs

```rust
#[async_trait]
pub trait Exchange: Send + Sync {
    async fn active_market(&self) -> Result<Option<MarketState>>;
    async fn orderbook(&self, ticker: &str) -> Result<Orderbook>;
    async fn resting_orders(&self) -> Result<Vec<RestingOrder>>;
    async fn cancel_order(&self, order_id: &str) -> Result<()>;
    async fn place_order(&self, order: &OrderRequest) -> Result<OrderResult>;
    async fn positions(&self) -> Result<Vec<Position>>;
    async fn settlements(&self, ticker: &str) -> Result<Vec<Settlement>>;
    async fn balance(&self) -> Result<u64>;
}
```

### ports/brain.rs

```rust
#[async_trait]
pub trait Brain: Send + Sync {
    async fn decide(&self, context: &DecisionContext) -> Result<TradeDecision>;
}
```

### ports/weather_feed.rs

```rust
#[async_trait]
pub trait WeatherFeed: Send + Sync {
    async fn forecast(&self) -> Result<Option<WeatherSnapshot>>;
}
```

## Core Engine — The 10-Step Cycle

1. **CANCEL** stale resting orders from previous cycles
2. **SETTLE** — check if previous trade settled, update ledger + stats
3. **RISK** — deterministic checks (balance, daily loss, streak)
4. **MARKET** — fetch active market by series ticker (e.g. `KXHIGHNY`)
5. **ORDERBOOK** — fetch orderbook depth
6. **WEATHER** — fetch NWS + Open-Meteo deterministic + ensemble (concurrent)
7. **BRAIN** — deterministic rules: ensemble probability vs market implied price
8. **VALIDATE** — clamp shares/price, handle PASS
9. **FINAL POSITION CHECK** — abort if position appeared during weather fetch
10. **EXECUTE** — order first, ledger second (never phantom trades)

## Strategy — RulesBrain

The deterministic brain in `src/core/rules_brain.rs`:

1. **Parse market type** from Kalshi ticker: `KXHIGHNY-26FEB12-T39` → `Above(39.0)` threshold
2. **Compute ensemble YES probability**: sum ensemble member highs above threshold
3. **Compare to market implied**: `yes_ask / 100`
4. **Apply confidence weighting**: High (std dev <2°F) → 1.0x, Medium (2-4°F) → 0.8x, Low (>4°F) → 0.5x
5. **Pick best side**: whichever of YES/NO has larger adjusted edge
6. **Trade or PASS**: adjusted edge ≥ 5pp and price ≤ 50¢ → BUY, else PASS
7. **Size**: 5-9pp → 1 share, 10-15pp → 2 shares (max_shares=2)
8. **Price**: spread-aware — narrow ≤4¢ → pay ask, wide → midpoint

Fallback: if no ensemble data, uses sigmoid of (forecast_high - threshold) as probability estimate.

## Weather Data Sources

| Source | Endpoint | Data | Required? |
|--------|----------|------|-----------|
| Open-Meteo deterministic | `api.open-meteo.com/v1/forecast` | Current temp, hourly trajectory, daily high | Yes |
| Open-Meteo ensemble | `ensemble-api.open-meteo.com/v1/ensemble` | ICON + GFS + ECMWF members → bucket probabilities | Best-effort |
| NWS | `api.weather.gov/points/{lat},{lon}` | Official forecast high/low, conditions | Best-effort |

All 3 run concurrently via `tokio::join!`. Ensemble failure → sigmoid fallback. NWS failure → continue without.

## Risk Limits (hardcoded defaults)

- max_shares: 2
- max_daily_loss_cents: 1000 ($10)
- max_consecutive_losses: 7
- min_balance_cents: 500 ($5)
- min_minutes_to_expiry: 2.0
- max price per share: 50¢ (enforced in rules_brain)

## Safety

- **Lockfile**: `/tmp/kalshi-bot.lock` — PID-based, prevents double execution
- **Live mode gate**: PAPER_TRADE=true by default; must set CONFIRM_LIVE=true to go live
- **Startup validation**: Checks all config before any network calls
- **Ledger backup**: `brain/ledger.md.bak` before every write
- **Atomic stats**: Write to `.tmp` then rename
- **Order-first**: Order placed before ledger write; if order fails, ledger stays clean
- **50¢ cap**: Never pays more than 50¢ — guarantees ≥1:1 R/R

## Kalshi Auth

RSA-PSS with SHA-256, MGF1(SHA-256), salt length = digest length (32 bytes).
Message format: `{timestamp_ms}{METHOD}{path}`
Handles both PKCS#1 and PKCS#8 PEM formats.

### Endpoints

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/trade-api/v2/markets` | GET | Market discovery |
| `/trade-api/v2/markets/{ticker}/orderbook` | GET | Orderbook |
| `/trade-api/v2/portfolio/orders` | GET | Resting orders |
| `/trade-api/v2/portfolio/orders` | POST | Place order |
| `/trade-api/v2/portfolio/orders/{id}` | DELETE | Cancel order |
| `/trade-api/v2/portfolio/positions` | GET | Open positions |
| `/trade-api/v2/portfolio/settlements` | GET | Settled trades |
| `/trade-api/v2/portfolio/balance` | GET | Balance in cents |

### Base URLs

- **Production**: `https://api.elections.kalshi.com`
- **Demo**: `https://demo-api.kalshi.co`

## Config (.env)

```bash
KALSHI_API_KEY_ID=your-uuid
KALSHI_PRIVATE_KEY_PATH=./kalshi_private_key.pem
KALSHI_BASE_URL=https://api.elections.kalshi.com
KALSHI_SERIES_TICKER=KXHIGHNY

WEATHER_CITY="New York"
WEATHER_LAT=40.7128
WEATHER_LON=-74.0060
WEATHER_TIMEZONE=America/New_York

PAPER_TRADE=false
CONFIRM_LIVE=true
```

## Cron

```bash
0 8,10,12,14,16 * * * cd /path/to/weather-bot && RUST_LOG=info ./target/release/kalshi-bot >> logs/cron.log 2>&1
```

## Cost

$0 per cycle. No LLM. Free weather APIs. Only cost is Kalshi trading fees.
