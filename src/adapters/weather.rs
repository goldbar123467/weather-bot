use crate::core::types::*;
use crate::ports::weather_feed::WeatherFeed;
use anyhow::Result;
use async_trait::async_trait;

pub struct WeatherClient {
    client: reqwest::Client,
    lat: f64,
    lon: f64,
    city: String,
    timezone: String,
}

impl WeatherClient {
    pub fn new(config: &Config) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()?,
            lat: config.weather_lat,
            lon: config.weather_lon,
            city: config.weather_city.clone(),
            timezone: config.weather_timezone.clone(),
        })
    }

    async fn fetch_nws(&self) -> Option<(Option<f64>, Option<f64>, Option<String>)> {
        let points_url = format!(
            "https://api.weather.gov/points/{:.4},{:.4}",
            self.lat, self.lon
        );

        let points_resp = self
            .client
            .get(&points_url)
            .header("User-Agent", "(kalshi-weather-bot, contact@kyzlolabs.com)")
            .send()
            .await
            .ok()?;

        if !points_resp.status().is_success() {
            tracing::warn!("NWS points -> {}", points_resp.status());
            return None;
        }

        let points: serde_json::Value = points_resp.json().await.ok()?;
        let forecast_url = points["properties"]["forecast"].as_str()?;

        let forecast_resp = self
            .client
            .get(forecast_url)
            .header("User-Agent", "(kalshi-weather-bot, contact@kyzlolabs.com)")
            .send()
            .await
            .ok()?;

        if !forecast_resp.status().is_success() {
            tracing::warn!("NWS forecast -> {}", forecast_resp.status());
            return None;
        }

        let forecast: serde_json::Value = forecast_resp.json().await.ok()?;
        let periods = forecast["properties"]["periods"].as_array()?;

        let mut high = None;
        let mut low = None;
        let mut short_forecast = None;

        for period in periods.iter().take(4) {
            let is_daytime = period["isDaytime"].as_bool().unwrap_or(false);
            let temp = period["temperature"].as_f64();
            if is_daytime && high.is_none() {
                high = temp;
                short_forecast = period["shortForecast"].as_str().map(String::from);
            } else if !is_daytime && low.is_none() {
                low = temp;
            }
            if high.is_some() && low.is_some() {
                break;
            }
        }

        Some((high, low, short_forecast))
    }

    async fn fetch_open_meteo_deterministic(&self) -> Result<OpenMeteoDeterministic> {
        let url = format!(
            "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&hourly=temperature_2m&current=temperature_2m&temperature_unit=fahrenheit&timezone={}&forecast_days=2",
            self.lat, self.lon, self.timezone
        );

        let resp = self.client.get(&url).send().await?.json::<serde_json::Value>().await?;

        let current_temp = resp["current"]["temperature_2m"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("Missing current temp from Open-Meteo"))?;

        let times = resp["hourly"]["time"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing hourly times"))?;
        let temps = resp["hourly"]["temperature_2m"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing hourly temps"))?;

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let mut hourly = Vec::new();
        let mut daily_high: f64 = f64::NEG_INFINITY;

        for (time_val, temp_val) in times.iter().zip(temps.iter()) {
            let time_str = time_val.as_str().unwrap_or_default();
            if let Some(temp) = temp_val.as_f64() {
                if time_str.starts_with(&today) {
                    if temp > daily_high {
                        daily_high = temp;
                    }
                    hourly.push(HourlyForecast {
                        time: time_str.to_string(),
                        temperature_f: temp,
                    });
                }
            }
        }

        if daily_high == f64::NEG_INFINITY {
            return Err(anyhow::anyhow!("No hourly data for today"));
        }

        Ok(OpenMeteoDeterministic {
            current_temp,
            forecast_high: daily_high,
            hourly,
        })
    }

    async fn fetch_open_meteo_ensemble(&self) -> Option<(EnsembleForecast, Vec<TempBucketProbability>)> {
        let url = format!(
            "https://ensemble-api.open-meteo.com/v1/ensemble?latitude={}&longitude={}&hourly=temperature_2m&models=icon_seamless,gfs_seamless,ecmwf_ifs025&temperature_unit=fahrenheit&timezone={}&forecast_days=2",
            self.lat, self.lon, self.timezone
        );

        let resp = self.client.get(&url).send().await.ok()?;
        if !resp.status().is_success() {
            tracing::warn!("Open-Meteo ensemble -> {}", resp.status());
            return None;
        }

        let data: serde_json::Value = resp.json().await.ok()?;
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        let mut all_highs: Vec<f64> = Vec::new();

        // Each model is a separate key under "hourly"
        let hourly = data["hourly"].as_object()?;
        let times = hourly.get("time")?.as_array()?;

        // Find indices for today
        let today_indices: Vec<usize> = times
            .iter()
            .enumerate()
            .filter(|(_, t)| t.as_str().unwrap_or_default().starts_with(&today))
            .map(|(i, _)| i)
            .collect();

        for (key, values) in hourly {
            if key == "time" {
                continue;
            }
            // Keys like "temperature_2m_member01", etc.
            if !key.starts_with("temperature_2m") {
                continue;
            }
            let arr = values.as_array()?;
            let mut member_high: f64 = f64::NEG_INFINITY;
            for &idx in &today_indices {
                if let Some(temp) = arr.get(idx).and_then(|v| v.as_f64()) {
                    if temp > member_high {
                        member_high = temp;
                    }
                }
            }
            if member_high > f64::NEG_INFINITY {
                all_highs.push(member_high);
            }
        }

        if all_highs.is_empty() {
            return None;
        }

        all_highs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = all_highs.len();
        let mean = all_highs.iter().sum::<f64>() / n as f64;
        let variance = all_highs.iter().map(|h| (h - mean).powi(2)).sum::<f64>() / n as f64;
        let std_dev = variance.sqrt();

        let percentile = |p: f64| -> f64 {
            let idx = (p / 100.0 * (n - 1) as f64).round() as usize;
            all_highs[idx.min(n - 1)]
        };

        let ensemble = EnsembleForecast {
            model_count: n,
            mean_high: mean,
            min_high: all_highs[0],
            max_high: all_highs[n - 1],
            std_dev,
            p10: percentile(10.0),
            p25: percentile(25.0),
            p75: percentile(75.0),
            p90: percentile(90.0),
        };

        // Build 2°F temperature buckets
        let bucket_low = (all_highs[0] / 2.0).floor() as i32 * 2 - 2;
        let bucket_high = (all_highs[n - 1] / 2.0).ceil() as i32 * 2 + 2;

        let mut buckets: Vec<TempBucketProbability> = Vec::new();
        let mut temp = bucket_low;
        while temp < bucket_high {
            let lower = temp as f64;
            let upper = (temp + 2) as f64;
            let count = all_highs.iter().filter(|&&h| h >= lower && h < upper).count();
            let prob = count as f64 / n as f64;
            if prob > 0.0 {
                buckets.push(TempBucketProbability {
                    label: format!("{}-{}°F", temp, temp + 2),
                    lower,
                    upper,
                    probability: prob,
                });
            }
            temp += 2;
        }

        Some((ensemble, buckets))
    }
}

struct OpenMeteoDeterministic {
    current_temp: f64,
    forecast_high: f64,
    hourly: Vec<HourlyForecast>,
}

#[async_trait]
impl WeatherFeed for WeatherClient {
    async fn forecast(&self) -> Result<Option<WeatherSnapshot>> {
        let (nws_result, deterministic_result, ensemble_result) = tokio::join!(
            self.fetch_nws(),
            self.fetch_open_meteo_deterministic(),
            self.fetch_open_meteo_ensemble(),
        );

        let det = match deterministic_result {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("Open-Meteo deterministic failed: {}", e);
                return Ok(None);
            }
        };

        let (nws_high, nws_low, nws_short) = match nws_result {
            Some(n) => (n.0, n.1, n.2),
            None => {
                tracing::warn!("NWS forecast unavailable, continuing without it");
                (None, None, None)
            }
        };

        let (ensemble, bucket_probabilities) = match ensemble_result {
            Some((e, b)) => (Some(e), b),
            None => {
                tracing::warn!("Open-Meteo ensemble unavailable, continuing without it");
                (None, Vec::new())
            }
        };

        let confidence = match &ensemble {
            Some(e) if e.std_dev < 2.0 => ForecastConfidence::High,
            Some(e) if e.std_dev < 4.0 => ForecastConfidence::Medium,
            Some(_) => ForecastConfidence::Low,
            None => ForecastConfidence::Medium,
        };

        Ok(Some(WeatherSnapshot {
            city: self.city.clone(),
            current_temp_f: det.current_temp,
            nws_forecast_high: nws_high,
            nws_forecast_low: nws_low,
            nws_short_forecast: nws_short,
            open_meteo_forecast_high: det.forecast_high,
            hourly_forecasts: det.hourly,
            ensemble,
            bucket_probabilities,
            confidence,
        }))
    }
}
