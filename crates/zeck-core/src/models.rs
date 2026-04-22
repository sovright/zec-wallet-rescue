use std::path::PathBuf;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ZeckNetwork {
    #[default]
    Mainnet,
    Testnet,
}

impl ZeckNetwork {
    pub fn coin_type(self) -> u32 {
        match self {
            Self::Mainnet => 133,
            Self::Testnet => 1,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mainnet => "mainnet",
            Self::Testnet => "testnet",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressScope {
    External,
    Internal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressInfo {
    pub encoded: String,
    pub is_unified: bool,
    pub has_orchard: bool,
    pub has_sapling: bool,
    pub has_transparent: bool,
    pub destination_ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivedTransparentAddress {
    pub index: u32,
    pub scope: AddressScope,
    pub path: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DerivedAccount {
    pub index: u32,
    pub sapling_path: String,
    pub orchard_path: String,
    pub transparent_receive_path: String,
    pub transparent_change_path: String,
    pub sapling_address: String,
    pub unified_address: String,
    pub transparent_receive_address: String,
    pub transparent_change_address: String,
}

#[derive(Debug, Clone)]
pub struct RuntimeScanConfig {
    pub seed_phrase: SecretString,
    pub birthday: u32,
    pub num_accounts: Option<u32>,
    pub gap_limit: u32,
    pub lightwalletd_url: String,
    pub data_dir: PathBuf,
    pub network: ZeckNetwork,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanConfig {
    pub birthday: u32,
    pub num_accounts: Option<u32>,
    pub gap_limit: u32,
    pub lightwalletd_url: String,
    pub data_dir: PathBuf,
    pub network: ZeckNetwork,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanHandle {
    pub id: String,
}

impl ScanHandle {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Default for ScanHandle {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanPhase {
    Idle,
    ValidatingSeed,
    DerivingKeys,
    ProbingLightwalletd,
    ScanningTransparent,
    ScanningShielded,
    Complete,
    Cancelled,
    Error,
}

impl ScanPhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Complete | Self::Cancelled | Self::Error)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountBalancePreview {
    pub account_index: u32,
    pub sapling_address: String,
    pub unified_address: String,
    pub transparent_receive_address: String,
    pub transparent_change_address: String,
    pub transparent_utxo_count: u32,
    pub sapling_zatoshis: u64,
    pub orchard_zatoshis: u64,
    pub transparent_zatoshis: u64,
    pub total_zatoshis: u64,
    /// Whether this account has any historical note activity (received notes,
    /// including spent ones). Used instead of current balance for gap-limit
    /// decisions so that accounts that received and fully spent funds are still
    /// detected as active.
    pub has_activity: bool,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LightwalletdProbe {
    pub endpoint: String,
    pub vendor: Option<String>,
    pub chain_name: Option<String>,
    pub latest_block_height: Option<u64>,
    pub sapling_activation_height: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanSummary {
    pub total_zatoshis: u64,
    pub authoritative_balances: bool,
    pub note: String,
    pub workspace_dir: String,
}

/// A funded position discovered on a derived account during scanning.
/// Emitted as soon as a refresh tick observes a new non-zero balance for an
/// (account, pool) pair, so users see "Found X ZEC on account N" without
/// waiting for the scan to complete. The list is append-only across the
/// scan; once a discovery is appended it stays put even if the balance
/// later drops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanDiscovery {
    pub account_index: u32,
    pub pool: DiscoveryPool,
    pub zatoshis: u64,
    /// Block height of the most recent refresh tick that produced this
    /// discovery — useful for "found at block 3,289,541" UX.
    pub at_block_height: u64,
    /// User-facing address for the pool: the unified address for orchard,
    /// the sapling z-addr for sapling, the transparent receive t-addr for
    /// transparent.
    pub address: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveryPool {
    Transparent,
    Sapling,
    Orchard,
}

impl DiscoveryPool {
    pub fn label(self) -> &'static str {
        match self {
            Self::Transparent => "transparent",
            Self::Sapling => "sapling",
            Self::Orchard => "orchard",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanProgress {
    pub handle: ScanHandle,
    pub phase: ScanPhase,
    pub blocks_scanned: u64,
    pub blocks_total: u64,
    /// Absolute Zcash chain height the wallet workspace has scanned up to,
    /// or `None` before the first authoritative refresh. Use this (not
    /// `blocks_scanned`, which is a delta from `effective_birthday`) when
    /// mapping scan progress to calendar era or mined-block context.
    #[serde(default)]
    pub synced_to_height: Option<u64>,
    pub elapsed_seconds: Option<u64>,
    pub estimated_remaining_seconds: Option<u64>,
    pub accounts: Vec<AccountBalancePreview>,
    /// Append-only log of (account, pool) discoveries observed during the
    /// scan. Consumers render new entries as incremental "found X" toasts
    /// without waiting for the scan to finish.
    #[serde(default)]
    pub discoveries: Vec<ScanDiscovery>,
    pub summary: Option<ScanSummary>,
    pub server: Option<LightwalletdProbe>,
    pub message: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepRequest {
    pub destination: String,
    pub memo: Option<String>,
    /// Maximum total fee in zatoshis across all sweep transactions. If actual
    /// fees exceed this the sweep is aborted with `MaxFeeExceeded`. `None`
    /// means no limit — use only when the caller has already reviewed the
    /// proposal fee and explicitly accepted it.
    pub max_fee_zatoshis: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProposedTxKind {
    ShieldTransparent,
    SweepShielded,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedTx {
    pub kind: ProposedTxKind,
    pub source_account: u32,
    pub destination: String,
    pub gross_zatoshis: u64,
    pub fee_zatoshis: u64,
    pub net_zatoshis: u64,
    pub note: String,
    pub memo: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkippedSweepAccount {
    pub account_index: u32,
    pub gross_zatoshis: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepProposal {
    pub transactions: Vec<ProposedTx>,
    pub skipped_accounts: Vec<SkippedSweepAccount>,
    pub total_send_zatoshis: u64,
    pub total_fee_zatoshis: u64,
    pub net_received_zatoshis: u64,
    pub dry_run_default: bool,
    pub warning: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BirthdayDetectResult {
    pub birthday: u32,
    /// "transparent" | "shielded_probe" | "no_activity"
    pub method: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TxBroadcastResult {
    pub source_account: u32,
    pub txid: Option<String>,
    pub status: String,
    pub detail: String,
    pub confirmed_height: Option<u32>,
}
