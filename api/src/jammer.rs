use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
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

fn run_cmd(program: &str, args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
}

/// Runs the entire stop -> export -> start -> manifest flow on a blocking thread.
/// All subprocess management uses std::process to avoid tokio SIGCHLD issues.
pub async fn run_jam(config: &JammerConfig, log: &JobLog) -> Result<String> {
    let tip = get_tip_block(config)
        .await
        .context("Failed to get tip block")?;
    log.append(&format!("[jammer] Tip block: {}", tip));

    let jam_path = config.jams_dir.join(format!("{}.jam", tip));

    if jam_path.exists() {
        log.append(&format!("[jammer] Jam already exists: {} (skipping)", jam_path.display()));
        write_manifest(config, log).await?;
        return Ok(format!("Jam for block {} already exists", tip));
    }

    let service = config.nockchain_service.clone();
    let user = config.nockchain_user.clone();
    let bin = config.nockchain_bin.clone();
    let dir = config.nockchain_dir.clone();
    let target = jam_path.clone();
    let jams_dir = config.jams_dir.clone();
    let html_root = config.html_root.clone();
    let manifest_path = config.manifest_path.clone();
    let log = log.clone();

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<()>>();

    std::thread::spawn(move || {
        let result = (|| -> Result<()> {
            std::fs::create_dir_all(&jams_dir)
                .context("Failed to create jams directory")?;

            log.append(&format!("[jammer] Stopping service: {}", service));
            match run_cmd("systemctl", &["stop", &service]) {
                Ok(s) => log.append(&format!("[jammer] Service stopped (exit {})", s)),
                Err(e) => log.append(&format!("[jammer] systemctl stop error: {}", e)),
            }

            log.append(&format!("[jammer] Exporting to: {}", target.display()));
            let mut cmd = if let Some(ref user) = user {
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

            use std::os::unix::process::CommandExt;
            unsafe {
                cmd.pre_exec(|| {
                    libc::setpgid(0, 0);
                    Ok(())
                });
            }

            let mut child = cmd
                .stdin(Stdio::null())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .context("Failed to spawn nockchain export")?;

            let status = child.wait().context("Failed to wait on export process")?;
            log.append(&format!("[jammer] Export process exited: {}", status));

            if !target.exists() {
                bail!("Export exited ({}) but no jam file at {}", status, target.display());
            }
            log.append(&format!("[jammer] Export done: {}", target.display()));

            log.append(&format!("[jammer] Starting service: {}", service));
            match run_cmd("systemctl", &["start", "--no-block", &service]) {
                Ok(s) => log.append(&format!("[jammer] Service start issued (exit {})", s)),
                Err(e) => log.append(&format!("[jammer] systemctl start error: {}", e)),
            }

            write_manifest_sync(&html_root, &jams_dir, &manifest_path, &log)?;

            Ok(())
        })();

        let _ = tx.send(result);
    });

    rx.await
        .context("jam thread dropped sender")?
        .context("jam task failed")?;

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

fn write_manifest_sync(html_root: &Path, jams_dir: &Path, manifest_path: &Path, log: &JobLog) -> Result<()> {
    log.append(&format!("[jammer] Writing manifest: {}", manifest_path.display()));
    let files = collect_hashable_files(html_root, jams_dir);

    if files.is_empty() {
        bail!("No files found to hash");
    }

    let mut content = String::new();
    for file in &files {
        let rel = file
            .strip_prefix(html_root)
            .unwrap_or(file)
            .to_string_lossy();
        let hash = hash_file(file)?;
        log.append(&format!("[jammer] Hashed: {}", rel));
        content.push_str(&format!("{}  {}\n", hash, rel));
    }

    let tmp = manifest_path.with_extension("tmp");
    std::fs::write(&tmp, &content).context("Failed to write temp manifest")?;
    std::fs::rename(&tmp, manifest_path).context("Failed to rename manifest")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(
            manifest_path,
            std::fs::Permissions::from_mode(0o644),
        );
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
