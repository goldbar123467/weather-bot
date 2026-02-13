use crate::core::{risk, stats, types::*};
use crate::ports::brain::Brain;
use crate::ports::exchange::Exchange;
use crate::ports::weather_feed::WeatherFeed;
use crate::storage;
use anyhow::Result;

pub async fn run_cycle(
    exchange: &dyn Exchange,
    brain: &dyn Brain,
    weather_feed: &dyn WeatherFeed,
    config: &Config,
) -> Result<()> {
    // 1. CANCEL stale resting orders from previous cycles
    let resting = exchange.resting_orders().await?;
    for order in &resting {
        exchange.cancel_order(&order.order_id).await?;
        storage::cancel_trade(&order.order_id)?;
        tracing::info!("Canceled stale order: {} (ledger marked cancelled)", order.order_id);
    }

    // 2. SETTLE — check if previous trade settled, update ledger + stats
    let mut ledger = storage::read_ledger()?;
    if let Some(pending) = ledger.iter().rev().find(|r| r.result == "pending") {
        let pending_ticker = pending.ticker.clone();
        let pending_timestamp = pending.timestamp.clone();
        let settlements = exchange.settlements(&pending_ticker).await?;
        if let Some(s) = settlements.first() {
            storage::settle_last_trade(s)?;
            ledger = storage::read_ledger()?;
            let settled_stats = stats::compute(&ledger);
            storage::write_stats(&settled_stats)?;
            tracing::info!(
                "Settled: {} (market_result={}) | {} {}¢",
                s.result.to_uppercase(), s.market_result, s.ticker, s.pnl_cents
            );
        } else {
            // No settlement found — check if pending entry is stale (>30 min old)
            if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&pending_timestamp) {
                let age_min = (chrono::Utc::now() - ts.with_timezone(&chrono::Utc)).num_minutes();
                if age_min > 30 {
                    let zombie = Settlement {
                        ticker: pending_ticker.clone(),
                        side: Side::Yes,
                        count: 0,
                        price_cents: 0,
                        result: "unknown".into(),
                        pnl_cents: 0,
                        settled_time: chrono::Utc::now().to_rfc3339(),
                        market_result: "unknown".into(),
                    };
                    storage::settle_last_trade(&zombie)?;
                    ledger = storage::read_ledger()?;
                    tracing::warn!(
                        "Zombie cleanup: pending entry for {} was {}min old, marked unknown",
                        pending_ticker, age_min
                    );
                }
            }
        }
    }

    // 3. RISK — deterministic checks in Rust
    let computed_stats = stats::compute(&ledger);
    let balance = exchange.balance().await?;

    if let Some(veto) = risk::check(&computed_stats, balance, config) {
        tracing::info!("Risk veto: {}", veto);
        return Ok(());
    }

    // 4. MARKETS — fetch all brackets for nearest event
    let brackets = exchange.active_markets().await?;
    if brackets.is_empty() {
        tracing::info!("No active markets");
        return Ok(());
    }

    // Filter by min_minutes_to_expiry
    let brackets: Vec<MarketState> = brackets
        .into_iter()
        .filter(|m| m.minutes_to_expiry >= config.min_minutes_to_expiry)
        .collect();

    if brackets.is_empty() {
        tracing::info!("All brackets too close to expiry");
        return Ok(());
    }

    let event_ticker = brackets[0].event_ticker.clone();
    tracing::info!(
        "Found {} brackets for event {} (expiry in {:.1}min)",
        brackets.len(), event_ticker, brackets[0].minutes_to_expiry
    );

    // 5. EVENT-LEVEL POSITION CHECK — skip if ANY position on this event
    let positions = exchange.positions().await?;
    if positions.iter().any(|p| {
        brackets.iter().any(|b| b.ticker == p.ticker)
    }) {
        tracing::warn!("Existing position on event {} — skipping entire event", event_ticker);
        return Ok(());
    }

    // 6. WEATHER — fetch once, shared across all brackets
    let weather = match weather_feed.forecast().await {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("Weather forecast failed: {}", e);
            None
        }
    };

    // 7. EVALUATE all brackets
    let prompt_md = storage::read_prompt()?;
    let mut buy_candidates: Vec<(MarketState, Orderbook, TradeDecision)> = Vec::new();
    let mut scan_lines: Vec<String> = Vec::new();

    for market in &brackets {
        let orderbook = exchange.orderbook(&market.ticker).await?;

        let context = DecisionContext {
            prompt_md: prompt_md.clone(),
            stats: stats::compute(&ledger),
            last_n_trades: ledger.iter().rev().take(20).cloned().collect(),
            market: market.clone(),
            orderbook,
            weather: weather.clone(),
        };

        let decision = brain.decide(&context).await?;

        // Build scan line for logging
        let market_type = MarketType::from_market(market);
        let mt_label = match &market_type {
            Some(MarketType::Above(t)) => format!(">{:.0}°", t),
            Some(MarketType::Below(t)) => format!("<{:.0}°", t),
            Some(MarketType::Between(lo, hi)) => format!("{:.0}-{:.0}°", lo, hi),
            None => "???".into(),
        };

        let ens_pct = if let (Some(w), Some(mt)) = (&weather, &market_type) {
            if !w.ensemble_member_highs.is_empty() {
                let total = w.ensemble_member_highs.len() as f64;
                let count = match mt {
                    MarketType::Above(t) => w.ensemble_member_highs.iter().filter(|h| **h > *t).count(),
                    MarketType::Below(t) => w.ensemble_member_highs.iter().filter(|h| **h < *t).count(),
                    MarketType::Between(lo, hi) => w.ensemble_member_highs.iter().filter(|h| **h >= *lo && **h < *hi).count(),
                };
                format!("{:.0}%", count as f64 / total * 100.0)
            } else {
                "n/a".into()
            }
        } else {
            "n/a".into()
        };

        let mkt_pct = market.yes_ask.map(|a| format!("{:.0}%", a)).unwrap_or_else(|| "n/a".into());
        let edge_pp = decision.edge_magnitude * 100.0;

        let action_label = match decision.action {
            Action::Buy => {
                let side = decision.side.as_ref().map(|s| format!("{:?}", s).to_uppercase()).unwrap_or_default();
                format!("BUY {}", side)
            }
            Action::Pass => "PASS".into(),
        };

        scan_lines.push(format!(
            "  {:<12} ({:<8}): ens={:<5} mkt={:<5} edge={:+.1}pp → {}",
            market.ticker.split('-').last().unwrap_or(&market.ticker),
            mt_label, ens_pct, mkt_pct, edge_pp, action_label
        ));

        if decision.action == Action::Buy {
            // Reclaim the orderbook for the candidate
            let ob = exchange.orderbook(&market.ticker).await?;
            buy_candidates.push((market.clone(), ob, decision));
        }
    }

    // 8. LOG bracket scan table
    tracing::info!("Bracket scan for {}:", event_ticker);
    for line in &scan_lines {
        tracing::info!("{}", line);
    }

    // 9. SELECT best bracket by edge_magnitude
    if buy_candidates.is_empty() {
        tracing::info!("PASS: No bracket has sufficient edge");
        return Ok(());
    }

    buy_candidates.sort_by(|a, b| {
        b.2.edge_magnitude.partial_cmp(&a.2.edge_magnitude).unwrap()
    });

    let (best_market, _best_ob, best_decision) = &buy_candidates[0];
    let side = best_decision.side.clone().unwrap_or(Side::Yes);
    let shares = best_decision.shares.unwrap_or(1).min(config.max_shares);
    let price = best_decision.max_price_cents.unwrap_or(50).clamp(1, 99);

    tracing::info!(
        "Best bracket: {} | edge={:.1}pp | {:?} {}x @ {}¢ | {}",
        best_market.ticker, best_decision.edge_magnitude * 100.0,
        side, shares, price, best_decision.reasoning
    );

    // 10. FINAL POSITION CHECK (race condition guard)
    let fresh_positions = exchange.positions().await?;
    if fresh_positions.iter().any(|p| {
        brackets.iter().any(|b| b.ticker == p.ticker)
    }) {
        tracing::warn!("Position appeared on event {} during evaluation — aborting", event_ticker);
        return Ok(());
    }

    // 11. EXECUTE — order FIRST, ledger SECOND
    let current_stats = stats::compute(&ledger);

    if config.paper_trade {
        let paper_id = format!("paper-{}", chrono::Utc::now().timestamp_millis());
        tracing::info!(
            "PAPER: {:?} {}x @ {}¢ | {} ({})",
            side,
            shares,
            price,
            best_market.ticker,
            paper_id
        );
        storage::append_ledger(&LedgerRow {
            timestamp: chrono::Utc::now().to_rfc3339(),
            ticker: best_market.ticker.clone(),
            side: format!("{:?}", side).to_lowercase(),
            shares,
            price,
            result: "pending".into(),
            pnl_cents: 0,
            cumulative_cents: current_stats.total_pnl_cents,
            order_id: paper_id,
        })?;
    } else {
        let order_result = exchange
            .place_order(&OrderRequest {
                ticker: best_market.ticker.clone(),
                side: side.clone(),
                shares,
                price_cents: price,
            })
            .await;

        match order_result {
            Ok(result) => {
                tracing::info!(
                    "LIVE: {:?} {}x @ {}¢ | {} (order {} status: {})",
                    side, shares, price, best_market.ticker, result.order_id, result.status
                );
                if let Err(e) = storage::append_ledger(&LedgerRow {
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    ticker: best_market.ticker.clone(),
                    side: format!("{:?}", side).to_lowercase(),
                    shares,
                    price,
                    result: "pending".into(),
                    pnl_cents: 0,
                    cumulative_cents: current_stats.total_pnl_cents,
                    order_id: result.order_id.clone(),
                }) {
                    tracing::error!(
                        "CRITICAL: Order {} placed but ledger write failed: {}",
                        result.order_id,
                        e
                    );
                    return Err(e.into());
                }
            }
            Err(e) => {
                tracing::error!("Order placement failed: {}", e);
                return Err(e);
            }
        }
    }

    Ok(())
}
