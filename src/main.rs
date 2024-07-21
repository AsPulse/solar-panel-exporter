use std::net::Ipv4Addr;

use axum::extract::{MatchedPath, State};
use axum::http::{Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use clap::Parser;
use tower_http::trace::TraceLayer;
use tracing::{error, info, info_span, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

#[derive(Parser, Debug)]
struct Args {
    #[arg(long)]
    endpoint: String,
    #[arg(long, env)]
    port: u16,
}

#[derive(Clone)]
struct AppState {
    endpoint: String,
}

const GENERATE_START_MARKER: &str = "<!-- ここから発電量表示 -->";
const GENERATE_END_MARKER: &str = "<!-- ここまで発電量表示 -->";

const CONSUMPTION_START_MARKER: &str = "<!-- ここから消費量表示 -->";
const CONSUMPTION_END_MARKER: &str = "<!-- ここまで消費量表示 -->";

#[tokio::main]
async fn main() {
    tracing_subscriber::Registry::default()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_filter(tracing_subscriber::filter::LevelFilter::DEBUG),
        )
        .try_init()
        .expect("failed to initialize tracing subscriber");

    let config = Args::parse();

    let app = Router::new()
        .route("/metrics", get(metrics))
        .fallback(handler_404)
        .with_state(AppState {
            endpoint: config.endpoint,
        })
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &Request<_>| {
                // Log the matched route's path (with placeholders not filled in).
                // Use request.uri() or OriginalUri if you want the real path.
                let matched_path = request
                    .extensions()
                    .get::<MatchedPath>()
                    .map(MatchedPath::as_str);

                info_span!(
                    "http_request",
                    method = ?request.method(),
                    matched_path,
                    some_other_field = tracing::field::Empty,
                )
            }),
        );

    let listener = tokio::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, config.port))
        .await
        .unwrap();

    axum::serve(listener, app).await.unwrap();
}

async fn metrics(state: State<AppState>) -> (StatusCode, String) {
    info!("collecting metrics...");

    let mut retry_count = 0u32;

    let value = loop {
        let body = match get_body(&state.endpoint).await {
            Ok(body) => body,
            Err(e) => {
                error!("failed to fetch metrics: {}", e);
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "failed to fetch metrics".to_string(),
                );
            }
        };

        if let Some(value) = parse(body) {
            break Some(value);
        }

        retry_count += 1;

        if retry_count > 3 {
            break None;
        }

        warn!(
            "retrying to fetch metrics... (retry_count: {})",
            retry_count
        );
        tokio::time::sleep(std::time::Duration::from_secs(2u64.pow(retry_count))).await;
    };

    let Some((generate, consumption)) = value else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed to parse metrics".to_string(),
        );
    };

    let response = [
        "# HELP power_solar_generation_watts An amount of solar power generation in watts"
            .to_string(),
        "# TYPE power_solar_generation_watts gauge".to_string(),
        format!("power_solar_generation_watts {}", generate),
        "# HELP power_consumption_watts An amount of power consumption in watts".to_string(),
        "# TYPE power_consumption_watts gauge".to_string(),
        format!("power_consumption_watts {}", consumption),
    ];
    (StatusCode::OK, response.join("\n"))
}

async fn get_body(ep: &str) -> Result<String, reqwest::Error> {
    reqwest::get(ep).await?.text_with_charset("shift_jis").await
}

fn parse(body: String) -> Option<(i64, i64)> {
    let generate = body
        .lines()
        .filter_map(|line| {
            if !line.contains(GENERATE_START_MARKER) {
                return None;
            }

            let end = line.find(GENERATE_END_MARKER)?;

            Some(line[GENERATE_START_MARKER.len()..end].to_string())
        })
        .next();

    let consumption = body
        .lines()
        .filter_map(|line| {
            if !line.contains(CONSUMPTION_START_MARKER) {
                return None;
            }

            let end = line.find(CONSUMPTION_END_MARKER)?;

            Some(line[CONSUMPTION_START_MARKER.len()..end].to_string())
        })
        .next();

    let (Some(generate), Some(consumption)) = (generate, consumption) else {
        error!("failed to parse metrics. generate or consumption is missing.",);
        error!("body: {:?}", body);
        return None;
    };

    let Ok(generate) = generate.parse::<f64>().map(|v| (v * 1000.0).round() as i64) else {
        error!("failed to parse generate as a float: {}", generate);
        return None;
    };

    let Ok(consumption) = consumption
        .parse::<f64>()
        .map(|v| (v * 1000.0).round() as i64)
    else {
        error!("failed to parse consumption as a float: {}", consumption);
        return None;
    };

    Some((generate, consumption))
}

async fn handler_404() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "404 Not Found. Try /metrics")
}
