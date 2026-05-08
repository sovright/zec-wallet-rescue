use std::{
    fs,
    path::{Path, PathBuf},
};

use rand_core::OsRng;
use secrecy::{ExposeSecret, SecretString, SecretVec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zcash_client_backend::data_api::{wallet::ConfirmationsPolicy, WalletRead};
use zcash_client_sqlite::{
    chain::init::init_cache_database, util::SystemClock, wallet::init::init_wallet_db, BlockDb,
    WalletDb,
};
use zcash_protocol::consensus::Network;
use zip32::fingerprint::SeedFingerprint;

use crate::{
    derivation::mnemonic_seed,
    error::{ZeckError, ZeckResult},
    models::{RuntimeScanConfig, ZeckNetwork},
};

/// Filename for the per-workspace session metadata sidecar. Lives next to
/// `wallet.sqlite` inside the workspace root.
pub const SESSION_FILE_NAME: &str = "session.json";

/// Current sidecar schema. Bumped whenever fields are added or semantics
/// change so older readers can refuse rather than silently misinterpret.
const SESSION_SCHEMA_VERSION: u32 = 1;

/// On-disk location of a wallet workspace for one (network, seed, birthday,
/// gap-strategy) tuple.
///
/// Resume keying: identical scan args MUST resolve to the same `root`
/// across runs. This module's tests pin that contract. The downstream
/// behavior — that re-running ZECK with the same args actually resumes
/// from `WalletSummary::fully_scanned_height` — depends on `WalletDb`
/// and `BlockDb` initializers being idempotent against existing on-disk
/// workspaces, which is an upstream contract from `zcash_client_sqlite`.
/// That contract is not pinned here; treat it as a thing to verify
/// during dependency bumps.
///
/// Privacy: the per-wallet path component is a SHA-256-derived hash of
/// `(domain, network, seed-fingerprint, birthday, scope)` rather than the
/// literal seed fingerprint string. An attacker with the seed can still
/// recompute the path, but local filesystem inspection no longer surfaces
/// the bech32 fingerprint directly.
#[derive(Debug, Clone)]
pub struct RecoveryWorkspace {
    root: PathBuf,
    /// First path component under `data_dir/<network>` that is private to
    /// this wallet — used to tighten permissions without touching the
    /// generic data_dir or network directories above it.
    private_root: PathBuf,
    wallet_db_path: PathBuf,
    cache_db_path: PathBuf,
}

impl RecoveryWorkspace {
    pub fn from_runtime(config: &RuntimeScanConfig) -> ZeckResult<Self> {
        let seed = mnemonic_seed(&config.seed_phrase)?;
        let fingerprint = SeedFingerprint::from_seed(seed.expose_secret()).ok_or_else(|| {
            ZeckError::Internal("mnemonic seed length is out of the ZIP 32 range".to_owned())
        })?;

        let scope = match config.num_accounts {
            Some(num_accounts) => format!("accounts-{num_accounts}"),
            None => format!("auto-gap-{}", config.gap_limit),
        };

        let workspace_id = derive_workspace_id(config.network, &fingerprint, config.birthday, &scope);
        let private_root = config
            .data_dir
            .join(config.network.label())
            .join(format!("workspace-{workspace_id}"));
        let root = private_root
            .join(format!("birthday-{}", config.birthday))
            .join(&scope);

        Ok(Self {
            wallet_db_path: root.join("wallet.sqlite"),
            cache_db_path: root.join("cache.sqlite"),
            root,
            private_root,
        })
    }

    pub fn initialize(&self, network: ZeckNetwork, seed: &[u8; 64]) -> ZeckResult<()> {
        create_private_dir_all(&self.root)?;
        // recursive create only sets mode on newly-created dirs; explicitly
        // re-tighten every component from the wallet-private root down so
        // resumes don't quietly inherit looser perms set in a previous run.
        tighten_private_perms(&self.private_root, &self.root)?;

        let mut wallet_db = WalletDb::for_path(
            &self.wallet_db_path,
            consensus_network(network),
            SystemClock,
            OsRng,
        )
        .map_err(|err| {
            ZeckError::Storage(format!(
                "opening wallet database {}: {err}",
                self.wallet_db_path.display()
            ))
        })?;
        init_wallet_db(&mut wallet_db, Some(SecretVec::new(seed.to_vec()))).map_err(|err| {
            ZeckError::Wallet(format!(
                "initializing wallet database {}: {err}",
                self.wallet_db_path.display()
            ))
        })?;
        set_private_file_permissions(&self.wallet_db_path)?;

        // Set `auto_vacuum=INCREMENTAL` on a fresh cache file so that the
        // deletions `zcash_client_backend::sync::run` issues after each scan
        // range can release pages back to the OS via `PRAGMA incremental_vacuum`
        // in `SqliteBlockCache::delete`. The pragma only takes effect on a
        // database that has no tables yet, so it must be set before
        // `init_cache_database`. Existing caches are left as-is — converting
        // them would require a full `VACUUM`, which is multi-GB on long scans.
        if !self.cache_db_path.exists() {
            let conn = rusqlite::Connection::open(&self.cache_db_path).map_err(|err| {
                ZeckError::Storage(format!(
                    "preparing cache database {}: {err}",
                    self.cache_db_path.display()
                ))
            })?;
            conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")
                .map_err(|err| {
                    ZeckError::Storage(format!(
                        "enabling auto_vacuum on {}: {err}",
                        self.cache_db_path.display()
                    ))
                })?;
        }

        let cache_db = BlockDb::for_path(&self.cache_db_path).map_err(|err| {
            ZeckError::Storage(format!(
                "opening cache database {}: {err}",
                self.cache_db_path.display()
            ))
        })?;
        init_cache_database(&cache_db).map_err(|err| {
            ZeckError::Wallet(format!(
                "initializing cache database {}: {err}",
                self.cache_db_path.display()
            ))
        })?;
        set_private_file_permissions(&self.cache_db_path)?;

        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn wallet_db_path(&self) -> &Path {
        &self.wallet_db_path
    }

    pub fn cache_db_path(&self) -> &Path {
        &self.cache_db_path
    }
}

pub fn consensus_network(network: ZeckNetwork) -> Network {
    match network {
        ZeckNetwork::Mainnet => Network::MainNetwork,
        ZeckNetwork::Testnet => Network::TestNetwork,
    }
}

fn derive_workspace_id(
    network: ZeckNetwork,
    fingerprint: &SeedFingerprint,
    birthday: u32,
    scope: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"zeck-workspace-v1\0");
    hasher.update(network.label().as_bytes());
    hasher.update(b"\0");
    hasher.update(fingerprint.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(birthday.to_le_bytes());
    hasher.update(b"\0");
    hasher.update(scope.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        out.push_str(&format!("{:02x}", byte));
    }
    out
}

fn create_private_dir_all(path: &Path) -> ZeckResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        // mode(0o700) only applies to dirs created during this call; pre-
        // existing parents keep their mode. We explicitly tighten the
        // wallet-private subtree separately via tighten_private_perms.
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|err| ZeckError::Storage(format!("creating {}: {err}", path.display())))?;
    }

    #[cfg(not(unix))]
    {
        fs::create_dir_all(path)
            .map_err(|err| ZeckError::Storage(format!("creating {}: {err}", path.display())))?;
    }

    Ok(())
}

#[allow(unused_variables)]
fn tighten_private_perms(private_root: &Path, leaf: &Path) -> ZeckResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut current = leaf.to_path_buf();
        loop {
            fs::set_permissions(&current, fs::Permissions::from_mode(0o700)).map_err(|err| {
                ZeckError::Storage(format!(
                    "setting private permissions on {}: {err}",
                    current.display()
                ))
            })?;
            if current == private_root {
                break;
            }
            match current.parent() {
                Some(parent) => current = parent.to_path_buf(),
                None => break,
            }
        }
    }

    Ok(())
}

fn set_private_file_permissions(path: &Path) -> ZeckResult<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|err| {
            ZeckError::Storage(format!(
                "setting private permissions on {}: {err}",
                path.display()
            ))
        })?;
    }

    #[cfg(not(unix))]
    {
        let _ = path;
    }

    Ok(())
}

/// On-disk metadata describing a scan session. Written next to the wallet
/// database when a scan starts and updated as it progresses. The launch-time
/// "resume an unfinished scan" UI keys on `completed == false`.
///
/// Contains nothing sensitive — no seed, no keys. Just enough for the user
/// to recognize their own scan and for the app to identify the workspace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMetadata {
    pub schema_version: u32,
    pub label: String,
    pub network: ZeckNetwork,
    pub birthday: u32,
    /// Chain-tip height recorded at the start of the most recent run. `None`
    /// if the lightwalletd probe never succeeded for this run.
    pub target_height: Option<u32>,
    /// Unix epoch seconds of the most recent run (start or retry).
    pub last_run_at_epoch_seconds: i64,
    pub completed: bool,
}

impl SessionMetadata {
    pub fn new_in_progress(
        label: String,
        network: ZeckNetwork,
        birthday: u32,
        target_height: Option<u32>,
        now_epoch_seconds: i64,
    ) -> Self {
        Self {
            schema_version: SESSION_SCHEMA_VERSION,
            label,
            network,
            birthday,
            target_height,
            last_run_at_epoch_seconds: now_epoch_seconds,
            completed: false,
        }
    }
}

/// Row returned to callers listing incomplete sessions on disk. Mirrors
/// `SessionMetadata` plus the wallet's persisted scan progress, which is
/// readable without the seed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncompleteSession {
    pub workspace_path: PathBuf,
    pub label: String,
    pub network: ZeckNetwork,
    pub birthday: u32,
    pub synced_to_height: Option<u32>,
    pub target_height: Option<u32>,
    pub last_run_at_epoch_seconds: Option<i64>,
}

fn session_path(workspace_root: &Path) -> PathBuf {
    workspace_root.join(SESSION_FILE_NAME)
}

/// Atomic write: write to `<path>.tmp` then rename. Best-effort guarantee
/// that a crash mid-write does not leave a half-written sidecar.
pub fn write_session_metadata(workspace_root: &Path, meta: &SessionMetadata) -> ZeckResult<()> {
    fs::create_dir_all(workspace_root).map_err(|err| {
        ZeckError::Storage(format!("creating {}: {err}", workspace_root.display()))
    })?;
    let final_path = session_path(workspace_root);
    let tmp_path = workspace_root.join(format!("{SESSION_FILE_NAME}.tmp"));
    let bytes = serde_json::to_vec_pretty(meta)
        .map_err(|err| ZeckError::Serialization(err.to_string()))?;
    fs::write(&tmp_path, &bytes)
        .map_err(|err| ZeckError::Storage(format!("writing {}: {err}", tmp_path.display())))?;
    fs::rename(&tmp_path, &final_path).map_err(|err| {
        ZeckError::Storage(format!(
            "renaming {} -> {}: {err}",
            tmp_path.display(),
            final_path.display(),
        ))
    })?;
    Ok(())
}

/// Read the sidecar at `workspace_root/session.json`. Returns `Ok(None)` if
/// missing or unparseable; only filesystem errors other than "not found"
/// surface as `Err`. Treating corrupt sidecars as missing keeps the launch
/// list robust against a bad write — the workspace is still usable, just
/// shows up as legacy.
pub fn read_session_metadata(workspace_root: &Path) -> ZeckResult<Option<SessionMetadata>> {
    let path = session_path(workspace_root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(ZeckError::Storage(format!(
                "reading {}: {err}",
                path.display()
            )));
        }
    };
    Ok(serde_json::from_slice::<SessionMetadata>(&bytes).ok())
}

/// Mark an existing in-progress session as completed. Idempotent: if the
/// sidecar is missing or unparseable, this is a no-op.
pub fn mark_session_completed(workspace_root: &Path, now_epoch_seconds: i64) -> ZeckResult<()> {
    let Some(mut meta) = read_session_metadata(workspace_root)? else {
        return Ok(());
    };
    meta.completed = true;
    meta.last_run_at_epoch_seconds = now_epoch_seconds;
    write_session_metadata(workspace_root, &meta)
}

/// Update `last_run_at` on an existing sidecar. Best-effort — any error is
/// returned to the caller, who is expected to swallow it on the hot path.
pub fn touch_session_last_run(workspace_root: &Path, now_epoch_seconds: i64) -> ZeckResult<()> {
    let Some(mut meta) = read_session_metadata(workspace_root)? else {
        return Ok(());
    };
    meta.last_run_at_epoch_seconds = now_epoch_seconds;
    write_session_metadata(workspace_root, &meta)
}

/// Walk `data_dir` and return any workspaces whose sidecar reports
/// `completed: false` or that have no readable sidecar (legacy). The latter
/// is treated as incomplete because we can't prove otherwise without the seed.
///
/// This function never opens a wallet for writing — the read-only summary
/// query does not need the seed.
pub fn list_incomplete_sessions(data_dir: &Path) -> ZeckResult<Vec<IncompleteSession>> {
    let mut rows = Vec::new();
    let networks = match fs::read_dir(data_dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(rows),
        Err(err) => {
            return Err(ZeckError::Storage(format!(
                "reading {}: {err}",
                data_dir.display()
            )));
        }
    };

    for network_entry in networks.flatten() {
        if !network_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let network_name = network_entry.file_name();
        let network = match network_name.to_str() {
            Some("mainnet") => ZeckNetwork::Mainnet,
            Some("testnet") => ZeckNetwork::Testnet,
            _ => continue,
        };
        let Ok(workspaces) = fs::read_dir(network_entry.path()) else {
            continue;
        };
        for workspace_entry in workspaces.flatten() {
            if !workspace_entry
                .file_type()
                .map(|t| t.is_dir())
                .unwrap_or(false)
            {
                continue;
            }
            // Per-wallet directory is named `workspace-<sha256>`. Skip
            // anything else so we don't try to read sibling files.
            if !workspace_entry
                .file_name()
                .to_str()
                .map(|s| s.starts_with("workspace-"))
                .unwrap_or(false)
            {
                continue;
            }
            let Ok(birthdays) = fs::read_dir(workspace_entry.path()) else {
                continue;
            };
            for birthday_entry in birthdays.flatten() {
                if !birthday_entry
                    .file_type()
                    .map(|t| t.is_dir())
                    .unwrap_or(false)
                {
                    continue;
                }
                let Some(birthday) = parse_birthday_segment(&birthday_entry.file_name()) else {
                    continue;
                };
                let Ok(scopes) = fs::read_dir(birthday_entry.path()) else {
                    continue;
                };
                for scope_entry in scopes.flatten() {
                    if !scope_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        continue;
                    }
                    let workspace_path = scope_entry.path();
                    if let Some(row) =
                        try_build_incomplete_row(&workspace_path, network, birthday)
                    {
                        rows.push(row);
                    }
                }
            }
        }
    }

    // Newest first by last_run_at; legacy rows with no timestamp sort last.
    rows.sort_by(|a, b| {
        b.last_run_at_epoch_seconds
            .cmp(&a.last_run_at_epoch_seconds)
    });
    Ok(rows)
}

fn parse_birthday_segment(name: &std::ffi::OsStr) -> Option<u32> {
    let s = name.to_str()?;
    s.strip_prefix("birthday-")?.parse::<u32>().ok()
}

/// Build one row for the launch-time list. Returns `None` when the candidate
/// workspace is complete, or when the wallet database is missing/unreadable
/// (i.e. there is nothing to resume).
fn try_build_incomplete_row(
    workspace_path: &Path,
    network: ZeckNetwork,
    birthday: u32,
) -> Option<IncompleteSession> {
    let wallet_db_path = workspace_path.join("wallet.sqlite");
    if !wallet_db_path.exists() {
        return None;
    }

    let meta = read_session_metadata(workspace_path).ok().flatten();
    if meta.as_ref().map(|m| m.completed).unwrap_or(false) {
        return None;
    }

    let synced_to_height = read_synced_height(&wallet_db_path, network);

    let (label, target_height, last_run_at) = match meta {
        Some(m) => (m.label, m.target_height, Some(m.last_run_at_epoch_seconds)),
        None => ("(unlabeled scan)".to_owned(), None, None),
    };

    Some(IncompleteSession {
        workspace_path: workspace_path.to_owned(),
        label,
        network,
        birthday,
        synced_to_height,
        target_height,
        last_run_at_epoch_seconds: last_run_at,
    })
}

fn read_synced_height(wallet_db_path: &Path, network: ZeckNetwork) -> Option<u32> {
    let wallet_db = WalletDb::for_path(
        wallet_db_path,
        consensus_network(network),
        SystemClock,
        OsRng,
    )
    .ok()?;
    let summary = wallet_db
        .get_wallet_summary(ConfirmationsPolicy::MIN)
        .ok()
        .flatten()?;
    Some(u32::from(summary.fully_scanned_height()))
}

/// Re-derive the workspace id from `seed_phrase` plus the keying segments
/// in `workspace_path`, and verify it matches the path's `workspace-<id>`
/// segment. Used at resume time so a caller cannot unlock a workspace
/// with the wrong seed.
pub fn verify_seed_for_workspace(
    workspace_path: &Path,
    seed_phrase: &SecretString,
) -> ZeckResult<()> {
    let path_id = extract_workspace_id_segment(workspace_path).ok_or_else(|| {
        ZeckError::InvalidConfig(format!(
            "workspace path {} does not contain a workspace-id segment",
            workspace_path.display()
        ))
    })?;
    let keying = parse_workspace_keying(workspace_path)?;
    let scope = scope_segment(&keying);

    let seed = mnemonic_seed(seed_phrase)?;
    let fingerprint = SeedFingerprint::from_seed(seed.expose_secret()).ok_or_else(|| {
        ZeckError::Internal("mnemonic seed length is out of the ZIP 32 range".to_owned())
    })?;
    let expected_id =
        derive_workspace_id(keying.network, &fingerprint, keying.birthday, &scope);

    if expected_id != path_id {
        return Err(ZeckError::InvalidConfig(
            "this seed phrase does not match the selected scan".to_owned(),
        ));
    }
    Ok(())
}

fn scope_segment(keying: &WorkspaceKeying) -> String {
    match keying.num_accounts {
        Some(num_accounts) => format!("accounts-{num_accounts}"),
        None => format!("auto-gap-{}", keying.gap_limit),
    }
}

/// Layout: `<data_dir>/<network>/workspace-<id>/birthday-N/<scope>`. The
/// `workspace-<id>` segment is the great-great-grandparent of the workspace
/// root's `wallet.sqlite`. Returns just the `<id>` portion (without the
/// `workspace-` prefix) so the caller can compare against
/// `derive_workspace_id` output directly.
fn extract_workspace_id_segment(workspace_path: &Path) -> Option<String> {
    let components: Vec<&std::ffi::OsStr> = workspace_path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    // Walk from the leaf back: scope, birthday-N, workspace-<id>, network.
    if components.len() < 4 {
        return None;
    }
    let len = components.len();
    let raw = components[len - 3].to_str()?;
    raw.strip_prefix("workspace-").map(|s| s.to_owned())
}

/// Reconstruct the runtime keying inputs (network, birthday, num_accounts,
/// gap_limit) from a workspace path. Used by the resume flow so the GUI
/// does not have to remember them across launches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceKeying {
    pub network: ZeckNetwork,
    pub birthday: u32,
    pub num_accounts: Option<u32>,
    pub gap_limit: u32,
}

pub fn parse_workspace_keying(workspace_path: &Path) -> ZeckResult<WorkspaceKeying> {
    let components: Vec<String> = workspace_path
        .components()
        .filter_map(|c| match c {
            std::path::Component::Normal(s) => s.to_str().map(|s| s.to_owned()),
            _ => None,
        })
        .collect();
    if components.len() < 4 {
        return Err(ZeckError::InvalidConfig(format!(
            "workspace path {} is shorter than the expected layout",
            workspace_path.display()
        )));
    }
    let len = components.len();
    let scope = &components[len - 1];
    let birthday_seg = &components[len - 2];
    // fingerprint = components[len - 3]; we don't need it here.
    let network_seg = &components[len - 4];

    let network = match network_seg.as_str() {
        "mainnet" => ZeckNetwork::Mainnet,
        "testnet" => ZeckNetwork::Testnet,
        other => {
            return Err(ZeckError::InvalidConfig(format!(
                "unrecognized network segment {other:?}"
            )));
        }
    };
    let birthday = birthday_seg
        .strip_prefix("birthday-")
        .and_then(|s| s.parse::<u32>().ok())
        .ok_or_else(|| {
            ZeckError::InvalidConfig(format!("malformed birthday segment {birthday_seg:?}"))
        })?;

    // Two scope shapes: `auto-gap-N` (gap_limit driven) or `accounts-N`
    // (explicit num_accounts). These are exclusive; pick whichever matches.
    let (num_accounts, gap_limit) = if let Some(rest) = scope.strip_prefix("auto-gap-") {
        let gap = rest.parse::<u32>().map_err(|_| {
            ZeckError::InvalidConfig(format!("malformed scope segment {scope:?}"))
        })?;
        (None, gap)
    } else if let Some(rest) = scope.strip_prefix("accounts-") {
        let count = rest.parse::<u32>().map_err(|_| {
            ZeckError::InvalidConfig(format!("malformed scope segment {scope:?}"))
        })?;
        // gap_limit is unused when num_accounts is set, but the validator
        // still requires gap_limit >= 1; pick the same value as `count`
        // so it round-trips through the existing config pipeline cleanly.
        (Some(count), count.max(1))
    } else {
        return Err(ZeckError::InvalidConfig(format!(
            "unrecognized scope segment {scope:?}"
        )));
    };

    Ok(WorkspaceKeying {
        network,
        birthday,
        num_accounts,
        gap_limit,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use secrecy::{ExposeSecret, SecretString};
    use zip32::fingerprint::SeedFingerprint;

    use super::*;
    use crate::derivation::mnemonic_seed;
    use crate::models::RuntimeScanConfig;

    fn config(
        seed: &str,
        birthday: u32,
        num_accounts: Option<u32>,
        gap_limit: u32,
        network: ZeckNetwork,
    ) -> RuntimeScanConfig {
        RuntimeScanConfig {
            seed_phrase: SecretString::new(seed.to_owned()),
            birthday,
            num_accounts,
            gap_limit,
            lightwalletd_url: "https://example.invalid:443".to_owned(),
            data_dir: PathBuf::from("/tmp/zeck-test-data"),
            network,
            label: String::new(),
        }
    }

    const SEED: &str = "abandon abandon abandon abandon abandon abandon \
                        abandon abandon abandon abandon abandon abandon \
                        abandon abandon abandon abandon abandon abandon \
                        abandon abandon abandon abandon abandon art";

    const OTHER_SEED: &str = "legal winner thank year wave sausage worth \
                              useful legal winner thank year wave sausage \
                              worth useful legal winner thank year wave \
                              sausage worth title";

    #[test]
    fn identical_args_produce_identical_workspace_path() {
        let a = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        let b = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        assert_eq!(a.root(), b.root());
    }

    #[test]
    fn different_birthday_uses_different_workspace() {
        let a = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        let b = RecoveryWorkspace::from_runtime(&config(
            SEED,
            2_500_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn different_gap_limit_uses_different_workspace() {
        let a = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        let b = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            40,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn explicit_num_accounts_uses_different_workspace_than_gap_limit() {
        let a = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        let b = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            Some(20),
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        assert_ne!(
            a.root(),
            b.root(),
            "auto-gap and explicit num-accounts use different scan strategies and \
             must persist to separate workspaces"
        );
    }

    #[test]
    fn different_network_uses_different_workspace() {
        let a = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        let b = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Testnet,
        ))
        .unwrap();
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn different_seed_uses_different_workspace() {
        let a = RecoveryWorkspace::from_runtime(&config(
            SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        let b = RecoveryWorkspace::from_runtime(&config(
            OTHER_SEED,
            3_280_000,
            None,
            20,
            ZeckNetwork::Mainnet,
        ))
        .unwrap();
        assert_ne!(a.root(), b.root());
    }

    #[test]
    fn workspace_root_lives_under_data_dir() {
        let cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        let ws = RecoveryWorkspace::from_runtime(&cfg).unwrap();
        assert!(
            ws.root().starts_with(&cfg.data_dir),
            "workspace root {} must live under data_dir {}",
            ws.root().display(),
            cfg.data_dir.display(),
        );
    }

    #[test]
    fn workspace_path_does_not_leak_seed_fingerprint() {
        let cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        let ws = RecoveryWorkspace::from_runtime(&cfg).unwrap();
        let path_str = ws.root().display().to_string();

        let seed = mnemonic_seed(&cfg.seed_phrase).expect("seed should derive");
        let fingerprint_str = SeedFingerprint::from_seed(seed.expose_secret())
            .expect("seed fingerprint should derive")
            .to_string();

        assert!(
            !path_str.contains(&fingerprint_str),
            "workspace path must not contain the literal seed fingerprint"
        );
        assert!(
            path_str.contains("workspace-"),
            "workspace path should use the hash-prefixed segment"
        );
    }

    #[test]
    fn session_metadata_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let meta = SessionMetadata::new_in_progress(
            "Old Zecwallet".to_owned(),
            ZeckNetwork::Mainnet,
            3_280_000,
            Some(3_400_000),
            1_715_000_000,
        );
        write_session_metadata(dir.path(), &meta).expect("write");
        let read = read_session_metadata(dir.path()).expect("read").expect("present");
        assert_eq!(read, meta);
    }

    #[test]
    fn read_session_metadata_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(read_session_metadata(dir.path()).expect("read").is_none());
    }

    #[test]
    fn read_session_metadata_returns_none_for_corrupt_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join(SESSION_FILE_NAME), b"not json").expect("write");
        assert!(read_session_metadata(dir.path()).expect("read").is_none());
    }

    #[test]
    fn mark_session_completed_flips_flag() {
        let dir = tempfile::tempdir().expect("tempdir");
        let meta = SessionMetadata::new_in_progress(
            "x".to_owned(),
            ZeckNetwork::Testnet,
            2_000_000,
            None,
            1,
        );
        write_session_metadata(dir.path(), &meta).expect("write");
        mark_session_completed(dir.path(), 2).expect("mark");
        let read = read_session_metadata(dir.path()).expect("read").expect("present");
        assert!(read.completed);
        assert_eq!(read.last_run_at_epoch_seconds, 2);
    }

    #[test]
    fn parse_workspace_keying_auto_gap() {
        let path =
            PathBuf::from("/tmp/data/mainnet/workspace-deadbeef/birthday-3280000/auto-gap-20");
        let keying = parse_workspace_keying(&path).expect("parse");
        assert_eq!(keying.network, ZeckNetwork::Mainnet);
        assert_eq!(keying.birthday, 3_280_000);
        assert_eq!(keying.num_accounts, None);
        assert_eq!(keying.gap_limit, 20);
    }

    #[test]
    fn parse_workspace_keying_explicit_accounts() {
        let path =
            PathBuf::from("/tmp/data/testnet/workspace-cafebabe/birthday-2400000/accounts-15");
        let keying = parse_workspace_keying(&path).expect("parse");
        assert_eq!(keying.network, ZeckNetwork::Testnet);
        assert_eq!(keying.num_accounts, Some(15));
        assert_eq!(keying.gap_limit, 15);
    }

    #[test]
    fn verify_seed_for_workspace_accepts_matching_seed() {
        let cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        let ws = RecoveryWorkspace::from_runtime(&cfg).unwrap();
        verify_seed_for_workspace(ws.root(), &SecretString::new(SEED.to_owned()))
            .expect("matching seed verifies");
    }

    #[test]
    fn verify_seed_for_workspace_rejects_other_seed() {
        let cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        let ws = RecoveryWorkspace::from_runtime(&cfg).unwrap();
        let err = verify_seed_for_workspace(ws.root(), &SecretString::new(OTHER_SEED.to_owned()))
            .expect_err("mismatched seed must fail");
        assert!(matches!(err, ZeckError::InvalidConfig(_)));
    }

    #[test]
    fn list_incomplete_sessions_filters_completed_and_listsboth_legacy_and_marked() {
        let data_dir = tempfile::tempdir().expect("tempdir");
        // Workspace 1: incomplete with sidecar.
        let cfg1 = RuntimeScanConfig {
            seed_phrase: SecretString::new(SEED.to_owned()),
            birthday: 3_280_000,
            num_accounts: None,
            gap_limit: 20,
            lightwalletd_url: "https://example.invalid:443".to_owned(),
            data_dir: data_dir.path().to_owned(),
            network: ZeckNetwork::Mainnet,
            label: String::new(),
        };
        let ws1 = RecoveryWorkspace::from_runtime(&cfg1).unwrap();
        std::fs::create_dir_all(ws1.root()).unwrap();
        // Stand in for wallet.sqlite — we just need the file to exist for
        // the listing to consider this dir; reading the summary will fail
        // gracefully and fall back to None.
        std::fs::write(ws1.root().join("wallet.sqlite"), b"").unwrap();
        write_session_metadata(
            ws1.root(),
            &SessionMetadata::new_in_progress(
                "labeled scan".to_owned(),
                ZeckNetwork::Mainnet,
                3_280_000,
                Some(3_500_000),
                100,
            ),
        )
        .unwrap();

        // Workspace 2: legacy, no sidecar.
        let cfg2 = RuntimeScanConfig {
            seed_phrase: SecretString::new(OTHER_SEED.to_owned()),
            birthday: 3_280_000,
            num_accounts: None,
            gap_limit: 20,
            lightwalletd_url: "https://example.invalid:443".to_owned(),
            data_dir: data_dir.path().to_owned(),
            network: ZeckNetwork::Mainnet,
            label: String::new(),
        };
        let ws2 = RecoveryWorkspace::from_runtime(&cfg2).unwrap();
        std::fs::create_dir_all(ws2.root()).unwrap();
        std::fs::write(ws2.root().join("wallet.sqlite"), b"").unwrap();

        // Workspace 3: completed — must be filtered out.
        let cfg3 = RuntimeScanConfig {
            seed_phrase: SecretString::new(SEED.to_owned()),
            birthday: 2_500_000,
            num_accounts: None,
            gap_limit: 20,
            lightwalletd_url: "https://example.invalid:443".to_owned(),
            data_dir: data_dir.path().to_owned(),
            network: ZeckNetwork::Mainnet,
            label: String::new(),
        };
        let ws3 = RecoveryWorkspace::from_runtime(&cfg3).unwrap();
        std::fs::create_dir_all(ws3.root()).unwrap();
        std::fs::write(ws3.root().join("wallet.sqlite"), b"").unwrap();
        let mut completed = SessionMetadata::new_in_progress(
            "done".to_owned(),
            ZeckNetwork::Mainnet,
            2_500_000,
            Some(3_000_000),
            50,
        );
        completed.completed = true;
        write_session_metadata(ws3.root(), &completed).unwrap();

        let rows = list_incomplete_sessions(data_dir.path()).expect("list");
        assert_eq!(rows.len(), 2, "expected ws1 and ws2, got {rows:?}");
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert!(labels.contains(&"labeled scan"));
        assert!(labels.contains(&"(unlabeled scan)"));
        // Ordering: labeled scan has last_run_at=100, legacy has None — labeled first.
        assert_eq!(rows[0].label, "labeled scan");
    }

    #[test]
    fn list_incomplete_sessions_empty_for_missing_data_dir() {
        let rows =
            list_incomplete_sessions(Path::new("/tmp/zeck-nonexistent-zzz")).expect("list");
        assert!(rows.is_empty());
    }

    #[test]
    fn workspace_path_does_not_change_across_releases() {
        // Snapshot the keying so an accidental change to the path layout
        // shows up as a test failure rather than a silently-orphaned
        // workspace on every user's disk.
        const EXPECTED_WORKSPACE_ID_FOR_TEST_SEED: &str = "b5e2cf2baecd3446f65e96a40159123d";
        let cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        let ws = RecoveryWorkspace::from_runtime(&cfg).unwrap();
        let suffix = ws
            .root()
            .strip_prefix(&cfg.data_dir)
            .unwrap()
            .to_string_lossy()
            .to_string();
        // network / workspace-<hash> / birthday-N / auto-gap-M
        let parts: Vec<&str> = suffix.split('/').collect();
        assert_eq!(parts.len(), 4, "unexpected workspace path layout: {suffix}");
        assert_eq!(parts[0], "mainnet");
        assert_eq!(
            parts[1],
            format!("workspace-{EXPECTED_WORKSPACE_ID_FOR_TEST_SEED}"),
            "workspace id changed for the fixed test seed — if intentional, \
             update EXPECTED_WORKSPACE_ID_FOR_TEST_SEED, but be aware this \
             means every existing user's workspace dir is now orphaned"
        );
        assert_eq!(parts[2], "birthday-3280000");
        assert_eq!(parts[3], "auto-gap-20");
    }

    #[test]
    fn workspace_id_is_deterministic_and_distinct_across_inputs() {
        let cfg = config(SEED, 3_280_000, None, 20, ZeckNetwork::Mainnet);
        let seed = mnemonic_seed(&cfg.seed_phrase).unwrap();
        let fp = SeedFingerprint::from_seed(seed.expose_secret()).unwrap();
        let a = derive_workspace_id(ZeckNetwork::Mainnet, &fp, 3_280_000, "auto-gap-20");
        let b = derive_workspace_id(ZeckNetwork::Mainnet, &fp, 3_280_000, "auto-gap-20");
        assert_eq!(a, b);
        let c = derive_workspace_id(ZeckNetwork::Mainnet, &fp, 3_280_001, "auto-gap-20");
        assert_ne!(a, c);
        let d = derive_workspace_id(ZeckNetwork::Testnet, &fp, 3_280_000, "auto-gap-20");
        assert_ne!(a, d);
    }
}
