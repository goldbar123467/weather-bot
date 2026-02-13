use crate::core::types::*;
use crate::ports::brain::Brain;
use anyhow::Result;
use async_trait::async_trait;

/// Pure deterministic brain — no LLM, no network calls.
/// Compares ensemble probability to market implied probability.
pub struct RulesBrain;

impl RulesBrain {
    pub fn new() -> Self {
        Self
    }
}

/// What kind of temperature contract this is.
#[derive(Debug)]
enum MarketType {
    /// "Will high be >X°?" — YES if above threshold
    Above(f64),
    /// "Will high be between X° and Y°?" — YES if in range
    Between(f64, f64),
}

#[async_trait]
impl Brain for RulesBrain {
    async fn decide(&self, ctx: &DecisionContext) -> Result<TradeDecision> {
        let weather = match &ctx.weather {
            Some(w) => w,
            None => return Ok(pass("No weather data available")),
        };

        let market_type = parse_market_type(&ctx.market.title, &ctx.market.ticker);
        tracing::info!(
            "Market: '{}' | ticker: {} | Parsed: {:?} | Ensemble buckets: {} | Confidence: {:?}",
            ctx.market.title, ctx.market.ticker, market_type,
            weather.bucket_probabilities.len(), weather.confidence,
        );

        if !weather.bucket_probabilities.is_empty() {
            let summary: Vec<String> = weather.bucket_probabilities.iter()
                .map(|b| format!("{}: {:.0}%", b.label, b.probability * 100.0))
                .collect();
            tracing::info!("Ensemble buckets: {}", summary.join(", "));
        }

        let market_implied = match ctx.market.yes_ask {
            Some(ask) => ask as f64 / 100.0,
            None => return Ok(pass("No yes_ask price available")),
        };

        let no_ask = ctx.market.no_ask.unwrap_or(100);
        let yes_ask = ctx.market.yes_ask.unwrap_or(100);

        tracing::info!(
            "Prices: yes_ask={}¢ no_ask={}¢ | Market implied YES={:.0}%",
            yes_ask, no_ask, market_implied * 100.0
        );

        // Compute ensemble YES probability based on market type
        let ensemble_yes = match &market_type {
            Some(mt) if weather.ensemble.is_some() && !weather.bucket_probabilities.is_empty() => {
                let prob = compute_ensemble_yes(&weather.bucket_probabilities, mt);
                tracing::info!("Ensemble YES probability: {:.1}%", prob * 100.0);
                Some(prob)
            }
            _ => {
                // Fallback: use forecast point estimate vs threshold
                match &market_type {
                    Some(MarketType::Above(threshold)) => {
                        let forecast_high = weather.open_meteo_forecast_high;
                        let diff = forecast_high - threshold;
                        // Rough sigmoid: if forecast is well above threshold, high prob
                        let prob = 1.0 / (1.0 + (-diff / 2.0_f64).exp());
                        tracing::info!(
                            "No ensemble — using point estimate: forecast_high={:.1}°F vs threshold={:.0}°F → {:.0}% YES",
                            forecast_high, threshold, prob * 100.0
                        );
                        Some(prob)
                    }
                    _ => {
                        tracing::info!("Cannot compute ensemble probability — no market type or no data");
                        None
                    }
                }
            }
        };

        if let Some(ens_yes) = ensemble_yes {
            let edge_yes = ens_yes - market_implied;
            let edge_no = (1.0 - ens_yes) - (no_ask as f64 / 100.0);

            let confidence_multiplier = match weather.confidence {
                ForecastConfidence::High => 1.0,
                ForecastConfidence::Medium => 0.8,
                ForecastConfidence::Low => 0.5,
            };

            let adj_edge_yes = edge_yes * confidence_multiplier;
            let adj_edge_no = edge_no * confidence_multiplier;

            let (side, raw_edge, adj_edge, price) = if adj_edge_yes >= adj_edge_no {
                (Side::Yes, edge_yes, adj_edge_yes, yes_ask)
            } else {
                (Side::No, edge_no, adj_edge_no, no_ask)
            };

            tracing::info!(
                "Edge: YES={:+.1}pp NO={:+.1}pp (adj YES={:+.1}pp NO={:+.1}pp) → best={:?}",
                edge_yes * 100.0, edge_no * 100.0,
                adj_edge_yes * 100.0, adj_edge_no * 100.0, side
            );

            if adj_edge < 0.05 {
                return Ok(pass(&format!(
                    "Edge too small: {:.1}pp adj on {:?}. Ensemble YES={:.0}% vs market={:.0}%. {:?} confidence.",
                    adj_edge * 100.0, side, ens_yes * 100.0, market_implied * 100.0, weather.confidence
                )));
            }

            if price > 50 {
                return Ok(pass(&format!(
                    "Edge {:.1}pp on {:?} but price {}¢ > 50¢ cap",
                    adj_edge * 100.0, side, price
                )));
            }

            let shares = size_from_edge(adj_edge);
            let max_price = spread_aware_price(&ctx.market, &ctx.orderbook, &side);

            if max_price > 50 {
                return Ok(pass(&format!(
                    "Edge {:.1}pp on {:?} but spread-aware price {}¢ > 50¢",
                    adj_edge * 100.0, side, max_price
                )));
            }

            let reasoning = format!(
                "Ensemble YES={:.0}% vs market implied={:.0}% → {:.1}pp edge on {:?} (adj {:.1}pp, {:?} confidence). {}x @ {}¢.",
                ens_yes * 100.0, market_implied * 100.0,
                raw_edge * 100.0, side, adj_edge * 100.0, weather.confidence,
                shares, max_price,
            );

            return Ok(TradeDecision {
                action: Action::Buy,
                side: Some(side),
                shares: Some(shares),
                max_price_cents: Some(max_price),
                reasoning,
            });
        }

        Ok(pass(&format!(
            "Cannot determine ensemble probability for '{}'",
            ctx.market.title
        )))
    }
}

fn pass(reason: &str) -> TradeDecision {
    TradeDecision {
        action: Action::Pass,
        side: None,
        shares: None,
        max_price_cents: None,
        reasoning: reason.to_string(),
    }
}

/// Parse market type from title and/or ticker.
///
/// Titles:  "Will the **high temp in NYC** be >39° on Feb 12, 2026?"
/// Tickers: "KXHIGHNY-26FEB12-T39" — T39 means threshold 39°F
fn parse_market_type(title: &str, ticker: &str) -> Option<MarketType> {
    // Try ticker first: look for -T{num} pattern (most reliable)
    if let Some(t_pos) = ticker.rfind("-T") {
        let after_t = &ticker[t_pos + 2..];
        if let Ok(threshold) = after_t.parse::<f64>() {
            return Some(MarketType::Above(threshold));
        }
    }

    // Try title: look for ">X°" or "above X°" or "at least X°"
    let clean = title.replace("°F", "°").replace("**", "");
    if let Some(pos) = clean.find('>') {
        let after = &clean[pos + 1..];
        let num_str: String = after.chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        if let Ok(threshold) = num_str.parse::<f64>() {
            return Some(MarketType::Above(threshold));
        }
    }

    // Try title: look for "between X and Y" or "X to Y"
    let lower = clean.replace("°", "");
    if lower.contains("between") {
        let nums: Vec<f64> = lower
            .split(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
            .filter_map(|s| s.parse().ok())
            .filter(|&n| n > -50.0 && n < 150.0)
            .collect();
        if nums.len() >= 2 {
            let a = nums[nums.len() - 2];
            let b = nums[nums.len() - 1];
            if (b - a).abs() <= 20.0 {
                return Some(MarketType::Between(a.min(b), a.max(b)));
            }
        }
    }

    None
}

/// Compute ensemble YES probability given the market type.
fn compute_ensemble_yes(buckets: &[TempBucketProbability], market_type: &MarketType) -> f64 {
    match market_type {
        MarketType::Above(threshold) => {
            // Sum probability of all buckets whose lower bound >= threshold
            // Plus partial credit for the bucket that straddles the threshold
            let mut prob = 0.0;
            for b in buckets {
                if b.lower >= *threshold {
                    // Entire bucket is above threshold
                    prob += b.probability;
                } else if b.upper > *threshold {
                    // Bucket straddles threshold — interpolate
                    let fraction = (b.upper - threshold) / (b.upper - b.lower);
                    prob += b.probability * fraction;
                }
            }
            prob
        }
        MarketType::Between(low, high) => {
            // Sum probability of buckets that overlap [low, high]
            let mut prob = 0.0;
            for b in buckets {
                if b.lower >= *high || b.upper <= *low {
                    continue; // no overlap
                }
                let overlap_low = b.lower.max(*low);
                let overlap_high = b.upper.min(*high);
                let fraction = (overlap_high - overlap_low) / (b.upper - b.lower);
                prob += b.probability * fraction;
            }
            prob
        }
    }
}

fn size_from_edge(edge: f64) -> u32 {
    if edge >= 0.15 {
        4
    } else if edge >= 0.10 {
        3
    } else if edge >= 0.05 {
        1
    } else {
        1
    }
}

fn spread_aware_price(market: &MarketState, orderbook: &Orderbook, side: &Side) -> u32 {
    let (bid, ask, ob_levels) = match side {
        Side::Yes => (
            market.yes_bid.unwrap_or(1),
            market.yes_ask.unwrap_or(99),
            &orderbook.yes,
        ),
        Side::No => (
            market.no_bid.unwrap_or(1),
            market.no_ask.unwrap_or(99),
            &orderbook.no,
        ),
    };

    let spread = ask.saturating_sub(bid);

    if spread <= 4 {
        return ask;
    }

    let ask_depth: u32 = ob_levels
        .iter()
        .filter(|(p, _)| *p == ask)
        .map(|(_, q)| *q)
        .sum();

    if ask_depth >= 10 {
        ask
    } else {
        (bid + ask) / 2
    }
}
