use crate::core::types::*;

/// Compares NWS forecast high vs Open-Meteo forecast high.
pub fn forecast_agreement(weather: &WeatherSnapshot) -> String {
    match weather.nws_forecast_high {
        Some(nws_high) => {
            let diff = (nws_high - weather.open_meteo_forecast_high).abs();
            if diff <= 1.0 {
                format!(
                    "Strong agreement: NWS {:.0}°F vs Open-Meteo {:.0}°F (within 1°F)",
                    nws_high, weather.open_meteo_forecast_high
                )
            } else if diff <= 3.0 {
                format!(
                    "Moderate agreement: NWS {:.0}°F vs Open-Meteo {:.0}°F ({:.0}°F apart)",
                    nws_high, weather.open_meteo_forecast_high, diff
                )
            } else {
                format!(
                    "Disagreement: NWS {:.0}°F vs Open-Meteo {:.0}°F ({:.0}°F apart)",
                    nws_high, weather.open_meteo_forecast_high, diff
                )
            }
        }
        None => format!(
            "NWS unavailable. Open-Meteo forecast high: {:.0}°F",
            weather.open_meteo_forecast_high
        ),
    }
}

/// Formats ensemble statistics as a readable summary.
pub fn ensemble_summary(ensemble: &EnsembleForecast) -> String {
    format!(
        "{} members | Mean: {:.1}°F | Range: {:.0}–{:.0}°F | Std dev: {:.1}°F | P10/P25/P75/P90: {:.0}/{:.0}/{:.0}/{:.0}°F",
        ensemble.model_count,
        ensemble.mean_high,
        ensemble.min_high,
        ensemble.max_high,
        ensemble.std_dev,
        ensemble.p10,
        ensemble.p25,
        ensemble.p75,
        ensemble.p90,
    )
}
