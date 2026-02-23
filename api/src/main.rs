use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::Mutex;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;

mod jammer;

mod proto {
    tonic::include_proto!("nockchain.public.v2");
}

struct JobState {
    running: bool,
    started_at: Option<Instant>,
    last_completed: Option<String>,
    last_success: Option<bool>,
    last_output: Option<String>,
    live_log: Option<JobLog>,
}

/// Thread-safe log buffer that jammer writes to during a job.
#[derive(Clone)]
pub struct JobLog(Arc<std::sync::Mutex<String>>);

impl JobLog {
    fn new() -> Self {
        Self(Arc::new(std::sync::Mutex::new(String::new())))
    }

    pub fn append(&self, msg: &str) {
        eprintln!("{}", msg);
        if let Ok(mut buf) = self.0.lock() {
            buf.push_str(msg);
            buf.push('\n');
        }
    }

    fn take(&self) -> String {
        self.0.lock().map(|mut s| std::mem::take(&mut *s)).unwrap_or_default()
    }
}

struct AppState {
    api_key: String,
    config: jammer::JammerConfig,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    last_output: Option<String>,
}

fn verify_api_key(headers: &HeaderMap, expected: &str) -> Result<(), StatusCode> {
    let key = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if key != expected {
        eprintln!("Unauthorized API key attempt");
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
    let log = JobLog::new();
    job.running = true;
    job.started_at = Some(Instant::now());
    job.live_log = Some(log.clone());
    drop(job);

    log.append("[make-jam] starting jam creation");

    let bg_state = Arc::clone(&state);
    let bg_log = log.clone();
    tokio::spawn(async move {
        let start = Instant::now();
        let result = jammer::run_jam(&bg_state.config, &bg_log).await;
        let elapsed = start.elapsed();

        match &result {
            Ok(msg) => bg_log.append(&format!("[make-jam] completed in {:.1}s: {}", elapsed.as_secs_f64(), msg)),
            Err(e) => bg_log.append(&format!("[make-jam] failed in {:.1}s: {:#}", elapsed.as_secs_f64(), e)),
        };

        let finished_at = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();

        let mut job = bg_state.job.lock().await;
        job.running = false;
        job.started_at = None;
        job.last_completed = Some(finished_at);
        job.last_success = Some(result.is_ok());
        job.last_output = Some(bg_log.take());
        job.live_log = None;
    });

    (
        StatusCode::ACCEPTED,
        Json(JobResult {
            success: true,
            output: "job started".into(),
        }),
    )
}

fn count_jams(dir: PathBuf) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.path()
                        .extension()
                        .is_some_and(|ext| ext == "jam")
                })
                .count()
        })
        .unwrap_or(0)
}

async fn status(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let job = state.job.lock().await;
    let running_for_secs = job.started_at.map(|t| t.elapsed().as_secs());
    let last_completed = job.last_completed.clone();
    let last_success = job.last_success;
    let last_output = if let Some(ref live) = job.live_log {
        let buf = live.0.lock().unwrap_or_else(|e| e.into_inner());
        Some(buf.clone())
    } else {
        job.last_output.clone()
    };
    let running = job.running;
    drop(job);

    let jams_dir = state.config.jams_dir.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || { let _ = tx.send(count_jams(jams_dir)); });
    let jam_count = rx.await.unwrap_or(0);

    Json(StatusResult {
        running,
        running_for_secs,
        jam_count,
        last_completed,
        last_success,
        last_output,
    })
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.into())
}

#[tokio::main]
async fn main() {
    let api_key = std::env::var("API_KEY").unwrap_or_else(|_| {
        eprintln!("WARNING: API_KEY not set, using empty string");
        String::new()
    });

    let jams_dir = env_or("JAMS_DIR", "/usr/share/nginx/html/jams");
    let html_root = env_or("HTML_ROOT", "/usr/share/nginx/html");
    let nockchain_dir = PathBuf::from(env_or("NOCKCHAIN_DIR", "/root/nockchain"));

    let config = jammer::JammerConfig {
        manifest_path: PathBuf::from(env_or(
            "MANIFEST",
            &format!("{}/SHA256SUMS", jams_dir),
        )),
        jams_dir: PathBuf::from(&jams_dir),
        html_root: PathBuf::from(&html_root),
        nockchain_rpc: env_or("NOCKCHAIN_RPC", "localhost:5556"),
        nockchain_bin: PathBuf::from(env_or(
            "NOCKCHAIN_BIN",
            "/root/.cargo/bin/nockchain",
        )),
        nockchain_dir: nockchain_dir.clone(),
        checkpoints_dir: nockchain_dir.join(".data.nockchain").join("checkpoints"),
        nockchain_user: std::env::var("NOCKCHAIN_USER").ok().filter(|s| !s.is_empty()),
        nockchain_service: env_or("NOCKCHAIN_SERVICE", "nockchain"),
    };

    eprintln!("config: JAMS_DIR={}", config.jams_dir.display());
    eprintln!("config: HTML_ROOT={}", config.html_root.display());
    eprintln!("config: NOCKCHAIN_RPC={}", config.nockchain_rpc);
    eprintln!("config: NOCKCHAIN_BIN={}", config.nockchain_bin.display());
    eprintln!("config: NOCKCHAIN_DIR={}", config.nockchain_dir.display());
    eprintln!(
        "config: NOCKCHAIN_USER={}",
        config.nockchain_user.as_deref().unwrap_or("(none)")
    );
    eprintln!("config: NOCKCHAIN_SERVICE={}", config.nockchain_service);
    eprintln!("config: CHECKPOINTS_DIR={}", config.checkpoints_dir.display());

    let state = Arc::new(AppState {
        api_key,
        config,
        job: Mutex::new(JobState {
            running: false,
            started_at: None,
            last_completed: None,
            last_success: None,
            last_output: None,
            live_log: None,
        }),
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_headers(Any)
        .allow_methods(Any);

    let jams_service = ServeDir::new(&state.config.jams_dir)
        .append_index_html_on_directories(true);

    let app = Router::new()
        .route("/api/make-jam", post(make_jam))
        .route("/api/status", get(status))
        .route("/", get(|| async { Redirect::permanent("/jams/") }))
        .nest_service("/jams", jams_service)
        .layer(cors)
        .with_state(state);

    let port = env_or("API_PORT", "80");
    let addr = format!("0.0.0.0:{}", port);
    eprintln!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
