//! Handler for weather plugin — open-meteo via network's http_request.

use vynkor_sdk::VynkorClient;

use crate::request::{parse_forecast, parse_now, Defaults};

#[derive(serde::Deserialize)]
struct NetworkHttpResponse {
    status: u16,
    body: String,
    body_encoding: String,
}

fn open_meteo_now_url(lat: f64, lon: f64, timezone: &str) -> String {
    // current weather + hourly context, no API key needed
    let tz = urlencoding_simple(timezone);
    format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&current=temperature_2m,relative_humidity_2m,apparent_temperature,precipitation,wind_speed_10m,wind_direction_10m,weather_code&timezone={tz}"
    )
}

fn open_meteo_forecast_url(lat: f64, lon: f64, days: u32, timezone: &str) -> String {
    let tz = urlencoding_simple(timezone);
    format!(
        "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}&daily=temperature_2m_max,temperature_2m_min,precipitation_sum,wind_speed_10m_max,weather_code,sunrise,sunset&timezone={tz}&forecast_days={days}"
    )
}

fn urlencoding_simple(s: &str) -> String {
    // minimal encoding for timezone slashes — open-meteo accepts raw slash, but be safe
    s.replace('/', "%2F")
}

async fn fetch_via_network(
    client: &mut VynkorClient,
    url: &str,
    timeout_ms: u64,
) -> Result<serde_json::Value, String> {
    let http_req = serde_json::json!({
        "url": url,
        "method": "GET",
        "timeout_ms": timeout_ms,
    });
    let http_req_json =
        serde_json::to_vec(&http_req).map_err(|e| format!("failed to encode http_request: {e}"))?;

    let resp = client
        .send_action("http_request", &http_req_json, timeout_ms as u32)
        .await
        .map_err(|e| format!("network plugin call failed: {e}"))?;

    if resp.status != vynkor_sdk::proto::ActionStatus::ActionOk as i32 {
        return Err(format!("network plugin error: {}", resp.error));
    }
    let net: NetworkHttpResponse = serde_json::from_slice(&resp.data_json)
        .map_err(|e| format!("malformed network response: {e}"))?;

    if !(200..300).contains(&net.status) {
        return Err(format!("open-meteo returned HTTP {}: {}", net.status, net.body));
    }

    let body_bytes: Vec<u8> = match net.body_encoding.as_str() {
        "base64" => {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD
                .decode(&net.body)
                .map_err(|e| format!("malformed base64 body: {e}"))?
        }
        _ => net.body.into_bytes(),
    };

    let v: serde_json::Value =
        serde_json::from_slice(&body_bytes).map_err(|e| format!("malformed open-meteo JSON: {e}"))?;
    Ok(v)
}

pub async fn handle_weather_now(
    client: &mut VynkorClient,
    params_json: &[u8],
    defaults: &Defaults,
) -> Result<Vec<u8>, String> {
    let p = parse_now(params_json, defaults)?;
    let url = open_meteo_now_url(p.lat, p.lon, &p.timezone);
    let raw = fetch_via_network(client, &url, p.timeout_ms).await?;

    // Normalize: keep raw but also surface key blocks for agent convenience
    let out = serde_json::json!({
        "latitude": raw.get("latitude").cloned().unwrap_or(serde_json::json!(p.lat)),
        "longitude": raw.get("longitude").cloned().unwrap_or(serde_json::json!(p.lon)),
        "timezone": raw.get("timezone").cloned().unwrap_or(serde_json::json!(p.timezone)),
        "current": raw.get("current").cloned().unwrap_or(serde_json::Value::Null),
        "hourly_units": raw.get("current_units").cloned().unwrap_or(serde_json::Value::Null),
        "raw": raw,
    });
    serde_json::to_vec(&out).map_err(|e| format!("failed to encode response: {e}"))
}

pub async fn handle_weather_forecast(
    client: &mut VynkorClient,
    params_json: &[u8],
    defaults: &Defaults,
) -> Result<Vec<u8>, String> {
    let p = parse_forecast(params_json, defaults)?;
    let url = open_meteo_forecast_url(p.lat, p.lon, p.days, &p.timezone);
    let raw = fetch_via_network(client, &url, p.timeout_ms).await?;

    let out = serde_json::json!({
        "latitude": raw.get("latitude").cloned().unwrap_or(serde_json::json!(p.lat)),
        "longitude": raw.get("longitude").cloned().unwrap_or(serde_json::json!(p.lon)),
        "timezone": raw.get("timezone").cloned().unwrap_or(serde_json::json!(p.timezone)),
        "daily": raw.get("daily").cloned().unwrap_or(serde_json::Value::Null),
        "daily_units": raw.get("daily_units").cloned().unwrap_or(serde_json::Value::Null),
        "raw": raw,
    });
    serde_json::to_vec(&out).map_err(|e| format!("failed to encode response: {e}"))
}
