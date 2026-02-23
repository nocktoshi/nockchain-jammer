use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use nockapp::export::ExportedState;
use nockapp::kernel::form::{LoadState, STATE_AXIS};
use nockapp::noun::slab::NockJammer;
use nockapp::noun::slab::NounSlab;
use nockapp::save::{JammedCheckpointV2, SaveableCheckpoint};
use nockvm::noun::Slots;
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
    pub checkpoints_dir: PathBuf,
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

/// Exports kernel state from the newest of 0.chkjam/1.chkjam (by mtime) to a .jam file (ExportedState format).
/// V2 checkpoints only. Does not start or stop the node.
/// Copies checkpoint files to a temp dir first so the running node can overwrite them safely.
pub fn chkjam_to_jam(checkpoints_dir: &Path, out_jam_path: &Path, log: &JobLog) -> Result<()> {
    let path_0 = checkpoints_dir.join("0.chkjam");
    let path_1 = checkpoints_dir.join("1.chkjam");

    let source_path = [path_0.as_path(), path_1.as_path()]
        .iter()
        .filter(|p| p.exists())
        .filter_map(|p| Some((*p, std::fs::metadata(p).ok()?.modified().ok()?)))
        .max_by_key(|(_, t)| *t)
        .map(|(p, _)| p)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "No checkpoint found in {} (need 0.chkjam or 1.chkjam)",
                checkpoints_dir.display()
            )
        })?;

    let _temp_dir = tempfile::tempdir().context("Failed to create temp dir for checkpoint copy")?;
    let temp_path = _temp_dir.path().join("checkpoint.chkjam");

    std::fs::copy(&source_path, &temp_path)
        .with_context(|| format!("Failed to copy {}", source_path.display()))?;

    let bytes = std::fs::read(temp_path)
        .with_context(|| format!("Failed to read copied checkpoint {}", temp_path.display()))?;

    let jammed = JammedCheckpointV2::decode_from_bytes(&bytes)
        .map_err(|e| anyhow::anyhow!("Checkpoint decode: {:?}", e))?;

    log.append(&format!(
        "[jammer] Using checkpoint event_num {}",
        jammed.event_num
    ));

    let saveable = SaveableCheckpoint::from_jammed_checkpoint_v2::<NockJammer>(jammed, None)
        .map_err(|e| anyhow::anyhow!("Checkpoint decode: {:?}", e))?;

    let arvo_root = unsafe { saveable.state.root() };
    let kernel_noun = arvo_root
        .slot(STATE_AXIS)
        .context("Failed to read kernel state (axis 6) from arvo")?;

    let mut kernel_slab = NounSlab::new();
    kernel_slab.copy_into(kernel_noun);

    let load_state = LoadState {
        ker_hash: saveable.ker_hash,
        event_num: saveable.event_num,
        kernel_state: kernel_slab,
    };

    let bytes = ExportedState::from_loadstate(load_state)
        .encode()
        .context("ExportedState encode")?;

    std::fs::write(out_jam_path, &bytes)
        .with_context(|| format!("Failed to write jam file: {}", out_jam_path.display()))?;

    log.append(&format!(
        "[jammer] Exported jam: {}",
        out_jam_path.display()
    ));
    Ok(())
}

/// Runs the entire export → manifest flow on a blocking thread.
/// Uses standalone chkjam→.jam export (no node stop/start).
pub async fn run_jam(config: &JammerConfig, log: &JobLog) -> Result<String> {
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
        write_manifest(config, log).await?;
        return Ok(format!("Jam for block {} already exists", tip));
    }

    std::fs::create_dir_all(&config.jams_dir).context("Failed to create jams directory")?;
    log.append(&format!(
        "[jammer] Exporting from checkpoints to: {}",
        jam_path.display()
    ));

    let checkpoints_dir = config.checkpoints_dir.clone();
    let out_path = jam_path.to_path_buf();
    let jams_dir = config.jams_dir.clone();
    let html_root = config.html_root.clone();
    let manifest_path = config.manifest_path.clone();
    let log = log.clone();

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<()>>();

    std::thread::spawn(move || {
        let result = chkjam_to_jam(&checkpoints_dir, &out_path, &log);
        if result.is_ok() {
            if let Err(e) = write_manifest_sync(&html_root, &jams_dir, &manifest_path, &log) {
                let _ = tx.send(Err(e));
                return;
            }
        }
        let _ = tx.send(result);
    });

    rx.await
        .context("jam thread dropped sender")?
        .context("jam task failed")?;

    return Ok(format!("Exported jam for block {}", tip));
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

    // Hash all files in parallel
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

    // Collect results in original sorted order
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
