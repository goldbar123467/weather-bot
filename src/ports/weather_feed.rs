use crate::core::types::{CityConfig, WeatherSnapshot};
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait WeatherFeed: Send + Sync {
    async fn forecast(&self, city: &CityConfig) -> Result<Option<WeatherSnapshot>>;
}
