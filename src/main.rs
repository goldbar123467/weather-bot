mod adapters;
mod core;
mod ports;
mod safety;
mod storage;

use adapters::weather::WeatherClient;
use adapters::kalshi::client::KalshiClient;
use core::rules_brain::RulesBrain;
use core::types::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = dotenv::dotenv() {
        eprintln!("WARNING: .env load failed: {}", e);
    }
    tracing_subscriber::fmt::init();

    let config = Config::from_env()?;
    let city_names: Vec<&str> = config.cities.iter().map(|c| c.name.as_str()).collect();
    tracing::info!(
        "paper_trade={} confirm_live={} cities=[{}]",
        config.paper_trade, config.confirm_live, city_names.join(", ")
    );

    safety::validate_startup(&config)?;

    let _lock = safety::Lockfile::acquire(&config.lockfile_path)?;

    let exchange = KalshiClient::new(&config)?;
    let brain = RulesBrain::new();
    let weather_feed = WeatherClient::new()?;

    core::engine::run_cycle(&exchange, &brain, &weather_feed, &config).await
}
