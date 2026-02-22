use std::process::Stdio;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::process::Command;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};

struct AppState {
    api_key: String,
    script_path: String,
    running: Mutex<bool>,
}

#[derive(Serialize)]
struct JobResult {
    success: bool,
    output: String,
}

#[derive(Serialize)]
struct StatusResult {
    running: bool,
}

fn verify_api_key(headers: &HeaderMap, expected: &str) -> Result<(), StatusCode> {
    let key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if key != expected {
        eprintln!("Unauthorized API key: {key}");
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}

async fn make_jam(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(code) = verify_api_key(&headers, &state.api_key) {
        return (
            code,
            Json(JobResult {
                success: false,
                output: "unauthorized".into(),
            }),
        );
    }

    let mut lock = state.running.lock().await;
    if *lock {
        return (
            StatusCode::CONFLICT,
            Json(JobResult {
                success: false,
                output: "a job is already running".into(),
            }),
        );
    }
    *lock = true;
    drop(lock);

    let result = Command::new("bash")
        .arg(&state.script_path)
        .arg("jam")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    let mut lock = state.running.lock().await;
    *lock = false;

    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let combined = format!("{}{}", stdout, stderr);
            let success = output.status.success();
            let code = if success {
                StatusCode::OK
            } else {
                StatusCode::INTERNAL_SERVER_ERROR
            };
            (code, Json(JobResult { success, output: combined }))
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(JobResult {
                success: false,
                output: format!("failed to spawn script: {e}"),
            }),
        ),
    }
}

async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let running = *state.running.lock().await;
    Json(StatusResult { running })
}

#[tokio::main]
async fn main() {
    let api_key = std::env::var("API_KEY").unwrap_or_else(|_| {
        eprintln!("WARNING: API_KEY not set, using empty string");
        String::new()
    });

    let script_path = std::env::var("SCRIPT_PATH")
        .unwrap_or_else(|_| "/usr/local/bin/make-jam.sh".into());

    let state = Arc::new(AppState {
        api_key,
        script_path,
        running: Mutex::new(false),
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_headers(Any)
        .allow_methods(Any);

    let app = Router::new()
        .route("/api/make-jam", post(make_jam))
        .route("/api/status", get(status))
        .layer(cors)
        .with_state(state);

    let addr = "127.0.0.1:3001";
    eprintln!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
