use std::process::Stdio;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::process::Command;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};

struct JobState {
    running: bool,
    started_at: Option<Instant>,
    last_completed: Option<String>,
    last_success: Option<bool>,
}

struct AppState {
    api_key: String,
    script_path: String,
    jams_dir: String,
    job: Mutex<JobState>,
}

#[derive(Serialize)]
struct JobResult {
    success: bool,
    output: String,
}

#[derive(Serialize)]
struct StatusResult {
    running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    running_for_secs: Option<u64>,
    jam_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_completed: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_success: Option<bool>,
}

fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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

    let mut job = state.job.lock().await;
    if job.running {
        eprintln!("[make-jam] rejected: job already running");
        return (
            StatusCode::CONFLICT,
            Json(JobResult {
                success: false,
                output: "a job is already running".into(),
            }),
        );
    }
    job.running = true;
    job.started_at = Some(Instant::now());
    drop(job);

    let log_file = std::env::temp_dir().join("nockchain-jammer-last.log");
    let exit_file = std::env::temp_dir().join("nockchain-jammer-last.exit");

    let _ = std::fs::remove_file(&exit_file);
    let _ = std::fs::write(&log_file, "");

    eprintln!("[make-jam] starting: bash {} jam", &state.script_path);
    let start = Instant::now();

    let wrapper = format!(
        "bash {script} jam 2>&1 | tee {log} >&2; echo ${{PIPESTATUS[0]}} > {exit}",
        script = shell_escape(&state.script_path),
        log = shell_escape(&log_file.to_string_lossy()),
        exit = shell_escape(&exit_file.to_string_lossy()),
    );

    let spawn_result = Command::new("setsid")
        .arg("bash")
        .arg("-c")
        .arg(&wrapper)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .kill_on_drop(false)
        .spawn();

    if let Err(e) = spawn_result {
        eprintln!("[make-jam] failed to spawn: {e}");
        let mut job = state.job.lock().await;
        job.running = false;
        job.started_at = None;
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(JobResult {
                success: false,
                output: format!("failed to spawn script: {e}"),
            }),
        );
    }

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        if exit_file.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            break;
        }
    }

    let elapsed = start.elapsed();
    let output_text = std::fs::read_to_string(&log_file).unwrap_or_default();
    let exit_code_str = std::fs::read_to_string(&exit_file).unwrap_or_default();
    let exit_code: i32 = exit_code_str.trim().parse().unwrap_or(-1);
    let success = exit_code == 0;

    if success {
        eprintln!("[make-jam] completed successfully in {:.1}s", elapsed.as_secs_f64());
    } else {
        eprintln!("[make-jam] failed with exit code {exit_code} in {:.1}s", elapsed.as_secs_f64());
    }

    let finished_at = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let code = if success { StatusCode::OK } else { StatusCode::INTERNAL_SERVER_ERROR };

    let mut job = state.job.lock().await;
    job.running = false;
    job.started_at = None;
    job.last_completed = Some(finished_at);
    job.last_success = Some(success);

    (code, Json(JobResult { success, output: output_text }))
}

fn count_jams(dir: &str) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .extension()
                        .map(|ext| ext == "jam")
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0)
}

async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let job = state.job.lock().await;
    let running_for_secs = job
        .started_at
        .map(|t| t.elapsed().as_secs());
    Json(StatusResult {
        running: job.running,
        running_for_secs,
        jam_count: count_jams(&state.jams_dir),
        last_completed: job.last_completed.clone(),
        last_success: job.last_success,
    })
}

#[tokio::main]
async fn main() {
    let api_key = std::env::var("API_KEY").unwrap_or_else(|_| {
        eprintln!("WARNING: API_KEY not set, using empty string");
        String::new()
    });

    let script_path = std::env::var("SCRIPT_PATH")
        .unwrap_or_else(|_| "/usr/local/bin/make-jam.sh".into());

    let jams_dir = std::env::var("JAMS_DIR")
        .unwrap_or_else(|_| "/usr/share/nginx/html/jams".into());

    eprintln!("config: SCRIPT_PATH={script_path}");
    eprintln!("config: JAMS_DIR={jams_dir}");
    for var in ["NOCKCHAIN_BIN", "NOCKCHAIN_DIR", "NOCKCHAIN_RPC", "HTML_ROOT"] {
        eprintln!("config: {}={}", var, std::env::var(var).unwrap_or_else(|_| "(unset)".into()));
    }

    let state = Arc::new(AppState {
        api_key,
        script_path,
        jams_dir,
        job: Mutex::new(JobState {
            running: false,
            started_at: None,
            last_completed: None,
            last_success: None,
        }),
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
