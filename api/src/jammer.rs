use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use tonic::transport::Channel;

use crate::proto::{
    get_blocks_response, nockchain_block_service_client::NockchainBlockServiceClient,
    GetBlocksRequest, PageRequest,
};

pub struct JammerConfig {
    pub html_root: PathBuf,
    pub jams_dir: PathBuf,
    pub manifest_path: PathBuf,
    pub nockchain_rpc: String,
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

pub async fn stop_service(config: &JammerConfig) -> Result<()> {
    eprintln!("[jammer] Stopping service: {}", config.nockchain_service);
    let service = config.nockchain_service.clone();

    let status = tokio::task::spawn_blocking(move || {
        Command::new("systemctl")
            .args(["stop", &service])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .context("Failed to run systemctl stop")
    })
    .await
    .context("stop task panicked")??;

    eprintln!("[jammer] Service stopped (exit {}): {}", status, config.nockchain_service);
    Ok(())
}

pub async fn start_service(config: &JammerConfig) -> Result<()> {
    eprintln!("[jammer] Starting service: {}", config.nockchain_service);
    let service = config.nockchain_service.clone();

    let status = tokio::task::spawn_blocking(move || {
        Command::new("systemctl")
            .args(["start", "--no-block", &service])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .status()
            .context("Failed to run systemctl start")
    })
    .await
    .context("start task panicked")??;

    if !status.success() {
        bail!("systemctl start failed with exit code {:?}", status.code());
    }

    eprintln!("[jammer] Service started: {}", config.nockchain_service);
    Ok(())
}

pub async fn export_jam(config: &JammerConfig, block_number: u64) -> Result<PathBuf> {
    let jam_path = config.jams_dir.join(format!("{}.jam", block_number));

    if jam_path.exists() {
        eprintln!(
            "[jammer] Jam already exists: {} (skipping export)",
            jam_path.display()
        );
        return Ok(jam_path);
    }

    std::fs::create_dir_all(&config.jams_dir)
        .context("Failed to create jams directory")?;

    eprintln!(
        "[jammer] Exporting state jam to: {} (from {})",
        jam_path.display(),
        config.nockchain_dir.display()
    );

    let user = config.nockchain_user.clone();
    let bin = config.nockchain_bin.clone();
    let dir = config.nockchain_dir.clone();
    let target = jam_path.clone();

    let mut child = tokio::task::spawn_blocking(move || -> Result<Child> {
        let mut cmd = if let Some(user) = &user {
            let mut c = Command::new("sudo");
            c.arg("-u").arg(user)
                .arg(bin.as_os_str())
                .arg("--export-state-jam")
                .arg(&target)
                .current_dir(&dir);
            c
        } else {
            let mut c = Command::new(&bin);
            c.arg("--export-state-jam")
                .arg(&target)
                .current_dir(&dir);
            c
        };

        let child = cmd.stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .context("Failed to spawn nockchain export")?;
        Ok(child)
    })
    .await
    .context("spawn export task panicked")??;

    // Wait for jam file to appear
    let start = tokio::time::Instant::now();
    while !jam_path.exists() && start.elapsed() < Duration::from_secs(15 * 60) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Kill the child process regardless - nockchain doesn't exit properly
    let _ = child.kill().await;

    if !jam_path.exists() {
        bail!("Jam file never appeared at {}", jam_path.display());
    }

    eprintln!("[jammer] Exported: {}", jam_path.display());
    Ok(jam_path)
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

pub async fn write_manifest(config: &JammerConfig) -> Result<()> {
    let html_root = config.html_root.clone();
    let jams_dir = config.jams_dir.clone();
    let manifest_path = config.manifest_path.clone();

    tokio::task::spawn_blocking(move || {
        let files = collect_hashable_files(&html_root, &jams_dir);

        if files.is_empty() {
            bail!("No files found to hash");
        }

        let mut content = String::new();
        for file in &files {
            let rel = file
                .strip_prefix(&html_root)
                .unwrap_or(file)
                .to_string_lossy();
            let hash = hash_file(file)?;
            content.push_str(&format!("{}  {}\n", hash, rel));
        }

        let tmp = manifest_path.with_extension("tmp");
        std::fs::write(&tmp, &content).context("Failed to write temp manifest")?;
        std::fs::rename(&tmp, &manifest_path).context("Failed to rename manifest")?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(
                &manifest_path,
                std::fs::Permissions::from_mode(0o644),
            );
        }

        eprintln!(
            "[jammer] Manifest written: {} ({} files)",
            manifest_path.display(),
            files.len()
        );
        Ok(())
    })
    .await
    .context("Manifest task panicked")?
}

/// Full jam creation flow: get tip -> stop service -> export -> restart -> write manifest.
pub async fn run_jam(config: &JammerConfig) -> Result<String> {
    let tip = get_tip_block(config)
        .await
        .context("Failed to get tip block")?;
    eprintln!("[jammer] Tip block: {}", tip);

    stop_service(config)
        .await
        .context("Failed to stop nockchain service")?;

    export_jam(config, tip).await.context("Failed to export jam")?;

    if let Err(e) = start_service(config).await {
        eprintln!("[jammer] WARNING: Failed to restart service: {}", e);
    }

    write_manifest(config).await.context("Failed to write manifest")?;

    Ok(format!("Exported jam for block {}", tip))
}
