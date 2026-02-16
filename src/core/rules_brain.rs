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

#[async_trait]
impl Brain for RulesBrain {
    async fn decide(&self, ctx: &DecisionContext) -> Result<TradeDecision> {
        let weather = match &ctx.weather {
            Some(w) => w,
            None => return Ok(pass("No weather data available")),
        };

        // Use API strike fields via MarketType::from_market()
        let market_type = MarketType::from_market(&ctx.market);
        tracing::info!(
            "Market: '{}' | ticker: {} | MarketType: {:?} | floor={:?} cap={:?} strike_type='{}' | Confidence: {:?}",
            ctx.market.title, ctx.market.ticker, market_type,
            ctx.market.floor_strike, ctx.market.cap_strike, ctx.market.strike_type,
            weather.confidence,
        );

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

        // Skip extreme prices — likely settled or stale
        if market_implied > 0.90 || market_implied < 0.10 {
            return Ok(pass(&format!(
                "Extreme price: yes_ask={}¢ (implied {:.0}%) — likely settled or stale",
                yes_ask, market_implied * 100.0
            )));
        }

        // Compute ensemble YES probability — prefer raw member highs, fall back to buckets
        let ensemble_yes = match &market_type {
            Some(mt) => {
                if !weather.ensemble_member_highs.is_empty() {
                    // Exact computation from raw ensemble members
                    let prob = compute_ensemble_yes_from_members(&weather.ensemble_member_highs, mt);
                    let total = weather.ensemble_member_highs.len();
                    let matching = (prob * total as f64).round() as usize;
                    tracing::info!(
                        "Ensemble YES (raw members): {}/{} members = {:.1}% | {:?}",
                        matching, total, prob * 100.0, mt
                    );
                    Some(prob)
                } else if weather.ensemble.is_some() && !weather.bucket_probabilities.is_empty() {
                    // Fallback: bucket interpolation
                    let prob = compute_ensemble_yes_from_buckets(&weather.bucket_probabilities, mt);
                    tracing::info!("Ensemble YES (bucket fallback): {:.1}%", prob * 100.0);
                    Some(prob)
                } else {
                    // Last resort: sigmoid from point estimate
                    match mt {
                        MarketType::Above(threshold) => {
                            let diff = weather.open_meteo_forecast_high - threshold;
                            let prob = 1.0 / (1.0 + (-diff / 2.0_f64).exp());
                            tracing::info!(
                                "No ensemble — sigmoid: forecast_high={:.1}°F vs threshold={:.0}°F → {:.0}% YES",
                                weather.open_meteo_forecast_high, threshold, prob * 100.0
                            );
                            Some(prob)
                        }
                        _ => {
                            tracing::info!("No ensemble data and non-Above market type — cannot estimate");
                            None
                        }
                    }
                }
            }
            None => {
                tracing::info!("Cannot determine market type from strike fields or ticker");
                None
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

            let (side, _raw_edge, adj_edge, price) = if adj_edge_yes >= adj_edge_no {
                (Side::Yes, edge_yes, adj_edge_yes, yes_ask)
            } else {
                (Side::No, edge_no, adj_edge_no, no_ask)
            };

            // Fee-aware: subtract estimated taker fee from edge
            let fee_pp = estimate_fee_pp(1, price);
            let net_edge = adj_edge - fee_pp;

            tracing::info!(
                "Edge: YES={:+.1}pp NO={:+.1}pp (adj YES={:+.1}pp NO={:+.1}pp) → best={:?} | Gross edge: {:.1}pp, fee: ~{:.1}pp, net edge: {:.1}pp",
                edge_yes * 100.0, edge_no * 100.0,
                adj_edge_yes * 100.0, adj_edge_no * 100.0, side,
                adj_edge * 100.0, fee_pp * 100.0, net_edge * 100.0
            );

            if net_edge < 0.05 {
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

            let shares = size_from_edge(net_edge);
            let max_price = spread_aware_price(&ctx.market, &ctx.orderbook, &side);

            if max_price > 50 {
                return Ok(pass(&format!(
                    "Edge {:.1}pp on {:?} but spread-aware price {}¢ > 50¢",
                    adj_edge * 100.0, side, max_price
                )));
            }

            // Liquidity filter — skip brackets with no real market activity
            if ctx.market.volume_24h < 10 && ctx.market.open_interest < 10 {
                return Ok(pass(&format!(
                    "Net edge {:.1}pp on {:?} but illiquid: vol_24h={}, OI={}",
                    net_edge * 100.0, side, ctx.market.volume_24h, ctx.market.open_interest
                )));
            }

            let reasoning = format!(
                "Ensemble YES={:.0}% vs market={:.0}% → {:.1}pp net edge on {:?} (gross {:.1}pp - fee ~{:.1}pp, {:?} confidence). {}x @ {}¢. vol_24h={} OI={}",
                ens_yes * 100.0, market_implied * 100.0,
                net_edge * 100.0, side, adj_edge * 100.0, fee_pp * 100.0, weather.confidence,
                shares, max_price, ctx.market.volume_24h, ctx.market.open_interest,
            );

            return Ok(TradeDecision {
                action: Action::Buy,
                side: Some(side),
                shares: Some(shares),
                max_price_cents: Some(max_price),
                reasoning,
                edge_magnitude: net_edge.abs(),
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
        edge_magnitude: 0.0,
    }
}

/// Compute ensemble YES probability from raw member highs — exact, no interpolation.
fn compute_ensemble_yes_from_members(member_highs: &[f64], market_type: &MarketType) -> f64 {
    let total = member_highs.len() as f64;
    if total == 0.0 {
        return 0.0;
    }
    let count = match market_type {
        MarketType::Above(t) => member_highs.iter().filter(|h| **h > *t).count(),
        MarketType::Below(t) => member_highs.iter().filter(|h| **h < *t).count(),
        MarketType::Between(lo, hi) => member_highs.iter().filter(|h| **h >= *lo && **h < *hi).count(),
    };
    count as f64 / total
}

/// Fallback: compute ensemble YES probability from 2°F temperature buckets.
fn compute_ensemble_yes_from_buckets(buckets: &[TempBucketProbability], market_type: &MarketType) -> f64 {
    match market_type {
        MarketType::Above(threshold) => {
            let mut prob = 0.0;
            for b in buckets {
                if b.lower >= *threshold {
                    prob += b.probability;
                } else if b.upper > *threshold {
                    let fraction = (b.upper - threshold) / (b.upper - b.lower);
                    prob += b.probability * fraction;
                }
            }
            prob
        }
        MarketType::Below(threshold) => {
            let mut prob = 0.0;
            for b in buckets {
                if b.upper <= *threshold {
                    prob += b.probability;
                } else if b.lower < *threshold {
                    let fraction = (threshold - b.lower) / (b.upper - b.lower);
                    prob += b.probability * fraction;
                }
            }
            prob
        }
        MarketType::Between(low, high) => {
            let mut prob = 0.0;
            for b in buckets {
                if b.lower >= *high || b.upper <= *low {
                    continue;
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

/// Estimate Kalshi taker fee as a fraction (percentage points).
/// Kalshi charges: fee_per_contract = min(price, 100-price) * fee_rate
/// where fee_rate is ~7% for taker (crossing the spread).
/// Returns fee as a fraction of notional (i.e., divide by 100 to get per-cent).
fn estimate_fee_pp(shares: u32, price_cents: u32) -> f64 {
    let fee_rate = 0.07; // 7% taker fee
    let capped_price = price_cents.min(100 - price_cents) as f64;
    let fee_per_contract = capped_price * fee_rate;
    // Convert to percentage points: fee in cents / 100 cents per dollar
    fee_per_contract * shares as f64 / (shares as f64 * 100.0)
}

fn size_from_edge(_edge: f64) -> u32 {
    50
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
