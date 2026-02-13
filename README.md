# weather-bot

Autonomous weather trading bot for Kalshi's daily high temperature contracts. Rust cron job that fetches weather forecasts from NWS + Open-Meteo, compares ensemble model probabilities against market prices, and trades when it finds an edge. Zero LLM cost — pure deterministic strategy.

## How It Works

Each cycle, the bot:

1. Cancels any stale resting orders from the previous cycle
2. Checks if the last trade settled (win/loss) and updates the ledger
3. Runs deterministic risk checks (balance floor, daily loss cap, streak limit)
4. Fetches the active temperature contract from Kalshi (e.g. `KXHIGHNY`)
5. Fetches the orderbook
6. Fetches weather data concurrently from 3 sources:
   - **NWS** — official forecast high/low + short forecast
   - **Open-Meteo deterministic** — current temp, hourly trajectory, forecast high
   - **Open-Meteo ensemble** — 40+ model members across ICON, GFS, ECMWF → bucket probabilities
7. Deterministic rules brain compares ensemble probability vs market implied probability
8. BUY if adjusted edge > 5pp and price ≤ 50¢, PASS otherwise
9. Places the order on Kalshi (or logs it in paper mode)
10. Exits

No AI calls. No API keys beyond Kalshi. All stats computed deterministically in Rust from an append-only markdown ledger.

## Strategy

The edge comes from weather ensemble models disagreeing with the market:

```
Ensemble probability (from 40+ model members)
    vs
Market implied probability (yes_ask / 100)
    =
Edge (in percentage points)
```

- Parse threshold from Kalshi ticker (`KXHIGHNY-26FEB12-T39` → "will high be >39°F?")
- Sum ensemble member probabilities above/below that threshold
- Apply confidence weighting (High/Medium/Low based on ensemble std dev)
- Size position by edge magnitude: 5-9pp → 1 share, 10-15pp → 3, 15+ → 4
- Spread-aware pricing: narrow spread → pay the ask, wide → place at midpoint

## Architecture

Hexagonal architecture — every external boundary is a swappable trait.

```
                    ┌─────────────────────────────┐
                    │         CORE DOMAIN          │
                    │  (pure Rust, no IO, no deps) │
                    │                              │
                    │  • engine.rs  (10-step cycle) │
                    │  • rules_brain.rs (strategy)  │
                    │  • risk.rs    (limit checks)  │
                    │  • stats.rs   (ledger math)   │
                    │  • types.rs   (domain types)  │
                    └──────────┬──────────────────┘
                               │ uses traits (ports)
            ┌──────────────────┼──────────────────┐
            │                  │                   │
    ┌───────▼──────┐   ┌──────▼───────┐   ┌──────▼───────┐
    │  Exchange    │   │  Brain       │   │  WeatherFeed │
    │  (Kalshi)    │   │  (Rules)     │   │  (NWS+OM)    │
    └──────────────┘   └──────────────┘   └──────────────┘
```

## Project Structure

```
weather-bot/
├── src/
│   ├── main.rs                   # Entry point, config, lockfile
│   ├── safety.rs                 # Lockfile, startup validation, live-mode gate
│   ├── storage.rs                # Read/write brain/*.md files
│   ├── core/
│   │   ├── engine.rs             # The 10-step trading cycle
│   │   ├── rules_brain.rs        # Deterministic ensemble vs market strategy
│   │   ├── indicators.rs         # Forecast agreement + ensemble summary
│   │   ├── risk.rs               # Pure risk checks
│   │   ├── stats.rs              # Compute stats from ledger
│   │   └── types.rs              # All domain types
│   ├── ports/
│   │   ├── exchange.rs           # Exchange trait
│   │   ├── brain.rs              # Brain trait
│   │   └── weather_feed.rs       # WeatherFeed trait
│   └── adapters/
│       ├── kalshi/               # Kalshi API + RSA-PSS auth
│       ├── weather.rs            # NWS + Open-Meteo adapter
│       └── openrouter.rs         # LLM adapter (preserved, not wired)
├── brain/
│   ├── prompt.md                 # Strategy reference (used by LLM adapter)
│   ├── ledger.md                 # Append-only trade log
│   └── stats.md                  # Computed performance stats
└── logs/
```

## Setup

### Prerequisites

- Rust toolchain (stable)
- Kalshi account with API access + RSA key pair

### Environment Variables

Create a `.env` file:

```bash
# Kalshi
KALSHI_API_KEY_ID=your-api-key-uuid
KALSHI_PRIVATE_KEY_PATH=./kalshi_private_key.pem
KALSHI_BASE_URL=https://api.elections.kalshi.com
KALSHI_SERIES_TICKER=KXHIGHNY

# Weather (defaults to NYC)
WEATHER_CITY="New York"
WEATHER_LAT=40.7128
WEATHER_LON=-74.0060
WEATHER_TIMEZONE=America/New_York

# Safety
PAPER_TRADE=true
CONFIRM_LIVE=false
```

### Build & Run

```bash
cargo build --release

# Paper trading (default — no real orders)
RUST_LOG=info ./target/release/kalshi-bot

# Live trading (real money)
PAPER_TRADE=false CONFIRM_LIVE=true ./target/release/kalshi-bot
```

### Cron Setup

Run every 2 hours during weather market hours:

```bash
0 8,10,12,14,16 * * * cd /path/to/weather-bot && RUST_LOG=info ./target/release/kalshi-bot >> logs/cron.log 2>&1
```

## Risk Limits

All hardcoded — no config knobs to accidentally blow up:

| Limit | Default | What It Does |
|-------|---------|--------------|
| Max shares per trade | 5 | Position size cap |
| Max daily loss | $10 | Stop trading for the day |
| Max consecutive losses | 7 | Stop trading until a win |
| Min balance | $5 | Don't trade below this floor |
| Min time to expiry | 2 min | Don't enter dying markets |
| Max price per share | 50¢ | Guarantees at least 1:1 R/R |

## Weather Data Sources

| Source | Endpoint | Data | Required? |
|--------|----------|------|-----------|
| Open-Meteo deterministic | `api.open-meteo.com/v1/forecast` | Current temp, hourly trajectory, daily high | Yes |
| Open-Meteo ensemble | `ensemble-api.open-meteo.com/v1/ensemble` | 40+ model members → bucket probabilities | Best-effort |
| NWS | `api.weather.gov/points/{lat},{lon}` | Official forecast high/low, conditions | Best-effort |

All 3 API calls run concurrently via `tokio::join!`. If ensemble fails, falls back to sigmoid estimate from point forecast. If NWS fails, continues without it.

## Safety

- **Lockfile** (`/tmp/kalshi-bot.lock`): PID-based, prevents double execution
- **Live mode gate**: `PAPER_TRADE=true` by default. Must set both `PAPER_TRADE=false` and `CONFIRM_LIVE=true`
- **Order-first writes**: Order placed before ledger write — no phantom trades
- **Ledger backup**: `brain/ledger.md.bak` before every write
- **50¢ cap**: Never pays more than 50¢ per share on any trade

## Cost

$0 per cycle. No LLM. Free weather APIs. Only cost is Kalshi trading fees.

## License

MIT
