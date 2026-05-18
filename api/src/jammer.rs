use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use nockapp_grpc::services::private_nockapp::client::PrivateNockAppGrpcClient;
use sha2::{Digest, Sha256};
use tonic::transport::Channel;

use crate::proto::{
    get_blocks_response, nockchain_block_service_client::NockchainBlockServiceClient,
    GetBlocksRequest, PageRequest,
};
use crate::JobLog;

pub struct JammerConfig {
    pub html_root: PathBuf,
    pub jams_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub nockchain_rpc: String,
    pub nockchain_private_grpc: String,
    pub nockchain_bin: PathBuf,
    pub nockchain_dir: PathBuf,
    pub nockchain_user: Option<String>,
    pub nockchain_service: String,
}

pub async fn get_tip_block(config: &JammerConfig) -> Result<u64> {
    let endpoint = Channel::from_shared(format!("http://{}", config.nockchain_rpc))?
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30));

    let channel = endpoint
        .connect()
        .await
        .context("Failed to connect to nockchain gRPC")?;

    let mut client = NockchainBlockServiceClient::new(channel);

    let request = GetBlocksRequest {
        page: Some(PageRequest {
            client_page_items_limit: 1,
            page_token: String::new(),
        }),
    };

    let response = client
        .get_blocks(request)
        .await
        .context("GetBlocks RPC failed")?;

    match response.into_inner().result {
        Some(get_blocks_response::Result::Blocks(data)) => Ok(data.current_height),
        Some(get_blocks_response::Result::Error(e)) => {
            bail!("gRPC error (code {}): {}", e.code, e.message)
        }
        None => bail!("Empty gRPC response"),
    }
}

/// Export live kernel state from the running nockchain node via private gRPC
/// (`NockApp::export_state` on the node). Does not stop the node.
pub async fn export_state_to_jam(
    private_grpc: &str,
    out_jam_path: &Path,
    log: &JobLog,
) -> Result<()> {
    if let Some(parent) = out_jam_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
    }

    log.append(&format!(
        "[jammer] Exporting live state via private gRPC ({}) to {}",
        private_grpc,
        out_jam_path.display()
    ));

    let mut client = PrivateNockAppGrpcClient::connect(private_grpc)
        .await
        .map_err(|e| anyhow::anyhow!("Private gRPC connect failed: {e}"))?;

    // Blocks until nockchain finishes writing the file (RPC is synchronous end-to-end).
    client
        .export_state(out_jam_path.to_string_lossy().into_owned())
        .await
        .map_err(|e| anyhow::anyhow!("ExportState RPC failed: {e}"))?;

    if !out_jam_path.exists() {
        bail!(
            "ExportState succeeded but no jam file at {}",
            out_jam_path.display()
        );
    }

    log.append(&format!(
        "[jammer] Exported jam: {}",
        out_jam_path.display()
    ));
    Ok(())
}

/// Runs the entire export → manifest flow.
/// Uses live `NockApp::export_state` on the running node (private gRPC).
/// `set_phase` is called as work progresses (for `/api/status`).
pub async fn run_jam<F, Fut>(
    config: &JammerConfig,
    log: &JobLog,
    mut set_phase: F,
) -> Result<String>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    set_phase("fetching_tip".into()).await;
    let tip = get_tip_block(config)
        .await
        .context("Failed to get tip block")?;
    if tip == 0 {
        bail!("Tip block is 0");
    }

    log.append(&format!("[jammer] Tip block: {}", tip));

    let jam_path = config.jams_dir.join(format!("{}.jam", tip));

    if jam_path.exists() {
        log.append(&format!(
            "[jammer] Jam already exists: {} (skipping)",
            jam_path.display()
        ));
        set_phase("manifest".into()).await;
        write_manifest(config, log).await?;
        return Ok(format!("Jam for block {} already exists", tip));
    }

    std::fs::create_dir_all(&config.jams_dir).context("Failed to create jams directory")?;
    log.append(&format!(
        "[jammer] Exporting live state to: {}",
        jam_path.display()
    ));

    set_phase("exporting".into()).await;
    export_state_to_jam(&config.nockchain_private_grpc, &jam_path, log)
        .await
        .context("Live state export failed")?;

    set_phase("manifest".into()).await;
    write_manifest(config, log).await?;

    Ok(format!("Exported jam for block {}", tip))
}

fn hash_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open(path).with_context(|| format!("Failed to open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    eprintln!("[jammer] Hashed file: {}", path.display());
    Ok(hex::encode(hasher.finalize()))
}

fn collect_hashable_files(html_root: &Path, jams_dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();

    for name in ["index.html", "privacy.html"] {
        let path = html_root.join(name);
        if path.exists() {
            files.push(path);
        }
    }

    if let Ok(entries) = std::fs::read_dir(jams_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jam") {
                files.push(path);
            }
        }
    }

    files.sort();
    files
}

fn write_manifest_sync(
    html_root: &Path,
    jams_dir: &Path,
    manifest_path: &Path,
    log: &JobLog,
) -> Result<()> {
    log.append(&format!(
        "[jammer] Writing manifest: {}",
        manifest_path.display()
    ));
    let files = collect_hashable_files(html_root, jams_dir);

    if files.is_empty() {
        bail!("No files found to hash");
    }

    let results: Vec<Result<(String, String)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = files
            .iter()
            .map(|file| {
                scope.spawn(|| -> Result<(String, String)> {
                    let rel = file
                        .strip_prefix(html_root)
                        .unwrap_or(file)
                        .to_string_lossy()
                        .to_string();
                    log.append(&format!("[jammer] Hashing: {}", rel));
                    let hash = hash_file(file)?;
                    log.append(&format!("[jammer] Hashed: {}", rel));
                    Ok((hash, rel))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let mut content = String::new();
    for result in results {
        let (hash, rel) = result?;
        content.push_str(&format!("{}  {}\n", hash, rel));
    }

    let tmp = manifest_path.with_extension("tmp");
    std::fs::write(&tmp, &content).context("Failed to write temp manifest")?;
    std::fs::rename(&tmp, manifest_path).context("Failed to rename manifest")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(manifest_path, std::fs::Permissions::from_mode(0o644));
    }

    log.append(&format!(
        "[jammer] Manifest written: {} ({} files)",
        manifest_path.display(),
        files.len()
    ));
    Ok(())
}

pub async fn write_manifest(config: &JammerConfig, log: &JobLog) -> Result<()> {
    let html_root = config.html_root.clone();
    let jams_dir = config.jams_dir.clone();
    let manifest_path = config.manifest_path.clone();
    let log = log.clone();

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<()>>();
    std::thread::spawn(move || {
        let result = write_manifest_sync(&html_root, &jams_dir, &manifest_path, &log);
        let _ = tx.send(result);
    });
    rx.await
        .context("manifest thread dropped sender")?
        .context("Manifest task failed")
}
