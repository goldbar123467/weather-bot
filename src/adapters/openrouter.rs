use crate::core::indicators;
use crate::core::types::*;
use crate::ports::brain::Brain;
use anyhow::Result;
use async_trait::async_trait;

pub struct OpenRouterClient {
    client: reqwest::Client,
    api_key: String,
}

impl OpenRouterClient {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            api_key: config.openrouter_api_key.clone(),
        })
    }
}

#[async_trait]
impl Brain for OpenRouterClient {
    async fn decide(&self, ctx: &DecisionContext) -> Result<TradeDecision> {
        let weather_section = match &ctx.weather {
            Some(w) => format!(
                "\n\n---\n## WEATHER FORECAST ({})\n{}",
                w.city, format_weather(w)
            ),
            None => "\n\n---\n## WEATHER FORECAST\nUnavailable this cycle.".into(),
        };

        let prompt = format!(
            "{prompt}\n\n---\n## STATS\n{stats}\n\n---\n## LAST {n} TRADES\n{ledger}\n\n---\n## MARKET\n{market}\n\n---\n## ORDERBOOK\nYes bids: {yes_ob}\nNo bids: {no_ob}{weather}",
            prompt = ctx.prompt_md,
            stats = format_stats(&ctx.stats),
            n = ctx.last_n_trades.len(),
            ledger = format_ledger(&ctx.last_n_trades),
            market = format_market(&ctx.market),
            yes_ob = format_ob_side(&ctx.orderbook.yes),
            no_ob = format_ob_side(&ctx.orderbook.no),
            weather = weather_section,
        );

        let body = serde_json::json!({
            "model": "moonshotai/kimi-k2.5",
            "max_tokens": 1200,
            "temperature": 0.2,
            "messages": [{"role": "user", "content": prompt}]
        });

        let resp = self
            .client
            .post("https://openrouter.ai/api/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("HTTP-Referer", "https://kyzlolabs.com")
            .header("X-Title", "Kalshi Weather Bot")
            .json(&body)
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let content = resp["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("No content in OpenRouter response"))?;

        parse_decision(content)
    }
}

fn format_stats(s: &Stats) -> String {
    format!(
        "Trades: {} | W/L: {}/{} | Win rate: {:.1}% | P&L: {}¢ | Today: {}¢ | Streak: {} | Drawdown: {}¢",
        s.total_trades, s.wins, s.losses, s.win_rate * 100.0,
        s.total_pnl_cents, s.today_pnl_cents, s.current_streak, s.max_drawdown_cents
    )
}

fn format_ledger(trades: &[LedgerRow]) -> String {
    if trades.is_empty() {
        return "No trades yet.".into();
    }
    trades
        .iter()
        .map(|t| {
            format!(
                "{} | {} | {} | {}x @ {}¢ | {} | {}¢",
                t.timestamp, t.ticker, t.side, t.shares, t.price, t.result, t.pnl_cents
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_market(m: &MarketState) -> String {
    format!(
        "Ticker: {} | Title: {} | Yes bid/ask: {:?}/{:?} | No bid/ask: {:?}/{:?} | Last: {:?} | Vol: {} | 24h Vol: {} | OI: {} | Expiry: {} ({:.1}min)",
        m.ticker, m.title, m.yes_bid, m.yes_ask, m.no_bid, m.no_ask,
        m.last_price, m.volume, m.volume_24h, m.open_interest,
        m.expiration_time, m.minutes_to_expiry
    )
}

fn format_ob_side(levels: &[(u32, u32)]) -> String {
    if levels.is_empty() {
        return "empty".into();
    }
    levels
        .iter()
        .take(5)
        .map(|(p, q)| format!("{}¢ x{}", p, q))
        .collect::<Vec<_>>()
        .join(", ")
}

fn format_weather(w: &WeatherSnapshot) -> String {
    let confidence_str = match w.confidence {
        ForecastConfidence::High => "HIGH (<2°F std dev)",
        ForecastConfidence::Medium => "MEDIUM (2-4°F std dev)",
        ForecastConfidence::Low => "LOW (>4°F std dev)",
    };

    let mut s = format!("Current temp: {:.1}°F\n", w.current_temp_f);
    s.push_str(&format!("Forecast confidence: {}\n", confidence_str));
    s.push_str(&format!("Source agreement: {}\n", indicators::forecast_agreement(w)));

    if let Some(nws_high) = w.nws_forecast_high {
        s.push_str(&format!("NWS forecast high: {:.0}°F", nws_high));
        if let Some(ref short) = w.nws_short_forecast {
            s.push_str(&format!(" ({})", short));
        }
        s.push('\n');
    }
    if let Some(nws_low) = w.nws_forecast_low {
        s.push_str(&format!("NWS forecast low: {:.0}°F\n", nws_low));
    }

    s.push_str(&format!("Open-Meteo forecast high: {:.1}°F\n", w.open_meteo_forecast_high));

    if let Some(ref ens) = w.ensemble {
        s.push_str(&format!("Ensemble: {}\n", indicators::ensemble_summary(ens)));
    }

    if !w.bucket_probabilities.is_empty() {
        s.push_str("\nTemperature bucket probabilities (ensemble-derived):\n");
        for b in &w.bucket_probabilities {
            s.push_str(&format!("  {} → {:.0}%\n", b.label, b.probability * 100.0));
        }
    }

    if !w.hourly_forecasts.is_empty() {
        s.push_str("\nHourly trajectory (today):\n");
        for h in w.hourly_forecasts.iter().step_by(3) {
            let time_short = h.time.split('T').nth(1).unwrap_or(&h.time);
            s.push_str(&format!("  {} → {:.1}°F\n", time_short, h.temperature_f));
        }
    }

    s
}

fn parse_decision(raw: &str) -> Result<TradeDecision> {
    let json_str = if let Some(s) = raw.find("```json") {
        let start = s + 7;
        let end = raw[start..]
            .find("```")
            .map(|i| start + i)
            .unwrap_or(raw.len());
        &raw[start..end]
    } else if raw.trim().starts_with('{') {
        raw.trim()
    } else if let (Some(s), Some(e)) = (raw.find('{'), raw.rfind('}')) {
        &raw[s..=e]
    } else {
        return Ok(TradeDecision {
            action: Action::Pass,
            side: None,
            shares: None,
            max_price_cents: None,
            reasoning: "Failed to parse AI response".into(),
        });
    };

    serde_json::from_str(json_str.trim()).map_err(Into::into)
}
