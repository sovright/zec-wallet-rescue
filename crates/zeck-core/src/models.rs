use std::path::PathBuf;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ZeckNetwork {
    Mainnet,
    Testnet,
}

impl Default for ZeckNetwork {
    fn default() -> Self {
        Self::Mainnet
    }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanProgress {
    pub handle: ScanHandle,
    pub phase: ScanPhase,
    pub blocks_scanned: u64,
    pub blocks_total: u64,
    pub elapsed_seconds: Option<u64>,
    pub estimated_remaining_seconds: Option<u64>,
    pub accounts: Vec<AccountBalancePreview>,
    pub summary: Option<ScanSummary>,
    pub server: Option<LightwalletdProbe>,
    pub message: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SweepRequest {
    pub destination: String,
    pub memo: Option<String>,
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
pub struct TxBroadcastResult {
    pub source_account: u32,
    pub txid: Option<String>,
    pub status: String,
    pub detail: String,
    pub confirmed_height: Option<u32>,
}
