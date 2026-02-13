use crate::core::types::WeatherSnapshot;
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait WeatherFeed: Send + Sync {
    async fn forecast(&self) -> Result<Option<WeatherSnapshot>>;
}
