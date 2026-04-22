use rusqlite::Connection;
use secrecy::{ExposeSecret, SecretString};
use time::{Date, Duration, Month};
use tonic::transport::Channel;
use zcash_client_backend::{
    data_api::AccountBirthday,
    proto::service::{
        compact_tx_streamer_client::CompactTxStreamerClient, BlockId, GetAddressUtxosArg,
    },
};
use uuid::Uuid;

use crate::{
    derivation::{derive_accounts, legacy_transparent_account_key, mnemonic_seed},
    error::{ZeckError, ZeckResult},
    lightwalletd::probe_lightwalletd_endpoints,
    models::{BirthdayDetectResult, RuntimeScanConfig, ZeckNetwork},
    scan::{import_probe_account, run_wallet_sync},
    workspace::{consensus_network, RecoveryWorkspace},
};

const SAPLING_ACTIVATION_HEIGHT: u32 = 419_200;
const SAPLING_ACTIVATION_DATE: (i32, Month, u8) = (2018, Month::October, 28);
const AVERAGE_BLOCK_SECONDS: i64 = 75;

/// Approximate blocks per year at 75 s/block.
const PROBE_YEAR_STEP: u32 = 420_480;
/// Wall-clock limit per shielded probe window (seconds).
const PROBE_TIMEOUT_SECS: u64 = 45;
/// Safety margin subtracted from the detected transparent activity height.
const BIRTHDAY_BUFFER_BLOCKS: u32 = 10_000;
/// Number of accounts (and their transparent addresses) to check for transparent activity.
const PROBE_ACCOUNT_COUNT: u32 = 5;

pub fn estimate_birthday_from_date(date: &str) -> ZeckResult<u32> {
    let format = time::macros::format_description!("[year]-[month]-[day]");
    let parsed =
        Date::parse(date, &format).map_err(|err| ZeckError::InvalidDate(err.to_string()))?;
    let anchor = Date::from_calendar_date(
        SAPLING_ACTIVATION_DATE.0,
        SAPLING_ACTIVATION_DATE.1,
        SAPLING_ACTIVATION_DATE.2,
    )
    .map_err(|err| ZeckError::InvalidDate(err.to_string()))?;

    if parsed <= anchor {
        return Ok(SAPLING_ACTIVATION_HEIGHT);
    }

    let seconds = (parsed - anchor).whole_days() * Duration::DAY.whole_seconds();
    let estimated_blocks = seconds / AVERAGE_BLOCK_SECONDS;

    Ok(SAPLING_ACTIVATION_HEIGHT.saturating_add(estimated_blocks.max(0) as u32))
}

/// Probe the blockchain to find the earliest block height where account-0 shows
/// any activity (transparent UTXOs or shielded note decryptions).
///
/// Phase 1: Calls `GetAddressUtxos` for the first `PROBE_ACCOUNT_COUNT` account
/// transparent addresses — instant, O(1) per address.
///
/// Phase 2: For each year-interval probe point (activation, +1yr, +2yr, …),
/// creates a temporary workspace, imports account-0 with that birthday, and runs
/// a time-limited compact-block sync (≤45 s).  Returns as soon as activity is
/// found, setting the birthday to the previous probe year for safety.
///
/// Falls back to Sapling activation if nothing is found anywhere.
pub async fn detect_birthday<F>(
    seed_phrase: &SecretString,
    network: ZeckNetwork,
    lightwalletd_url: &str,
    on_progress: F,
) -> ZeckResult<BirthdayDetectResult>
where
    F: Fn(&str) + Send,
{
    let _ = rustls::crypto::ring::default_provider().install_default();

    on_progress("Connecting to lightwalletd…");
    let (mut client, _endpoint, chain_info) =
        probe_lightwalletd_endpoints(lightwalletd_url).await?;
    let chain_tip = u32::try_from(chain_info.block_height)
        .map_err(|_| ZeckError::Lightwalletd("chain tip height overflowed u32".to_owned()))?;
    let sapling_floor = u32::try_from(chain_info.sapling_activation_height)
        .unwrap_or(SAPLING_ACTIVATION_HEIGHT)
        .saturating_add(1);

    // ── Phase 1: transparent probe ────────────────────────────────────────────
    on_progress("Checking transparent address history (instant)…");
    if let Ok(Some(earliest)) = probe_transparent(&mut client, seed_phrase, network).await {
        let birthday = earliest
            .saturating_sub(BIRTHDAY_BUFFER_BLOCKS)
            .max(sapling_floor);
        on_progress(&format!(
            "Transparent activity found at block {earliest}. Birthday → {birthday}."
        ));
        return Ok(BirthdayDetectResult {
            birthday,
            method: "transparent".to_owned(),
            message: format!(
                "Transparent activity detected at block {earliest}. \
                 Birthday set {BIRTHDAY_BUFFER_BLOCKS} blocks earlier to {birthday}."
            ),
        });
    }

    // ── Phase 2: stepped shielded probe ──────────────────────────────────────
    on_progress("No transparent history found. Probing shielded history by year…");
    let seed = mnemonic_seed(seed_phrase)?;
    let transparent_account = legacy_transparent_account_key(seed_phrase, network)?;

    // Build probe points: sapling_floor, +1yr, +2yr, … stop when too close to tip.
    let probe_heights: Vec<u32> = (0u32..)
        .map(|i| sapling_floor.saturating_add(PROBE_YEAR_STEP.saturating_mul(i)))
        .take_while(|&h| h.saturating_add(5_000) < chain_tip)
        .collect();

    for (i, &probe_height) in probe_heights.iter().enumerate() {
        let year_label = 2018usize.saturating_add(i);
        on_progress(&format!(
            "Probing ~{year_label} window (block {probe_height})…"
        ));

        let found = probe_shielded_window(
            &mut client,
            lightwalletd_url,
            seed_phrase,
            &seed,
            &transparent_account,
            network,
            probe_height,
            sapling_floor,
        )
        .await
        .unwrap_or(false);

        if found {
            // Set birthday to the PREVIOUS probe year so we don't miss any
            // notes from the window before the first detected activity.
            let birthday = if i == 0 {
                sapling_floor
            } else {
                probe_heights[i - 1]
            };
            on_progress(&format!(
                "Shielded activity detected near block {probe_height}. Birthday → {birthday}."
            ));
            return Ok(BirthdayDetectResult {
                birthday,
                method: "shielded_probe".to_owned(),
                message: format!(
                    "Shielded activity detected near year ~{year_label} \
                     (block {probe_height}). Birthday set to {birthday}."
                ),
            });
        }
    }

    on_progress("No activity found in any probe window. Using Sapling activation.");
    Ok(BirthdayDetectResult {
        birthday: sapling_floor,
        method: "no_activity".to_owned(),
        message: format!(
            "No activity found in any probe window. \
             Using Sapling activation ({sapling_floor}) for a complete scan."
        ),
    })
}

/// Call `GetAddressUtxos` for the first `PROBE_ACCOUNT_COUNT` accounts (both
/// external and internal transparent addresses).  Returns the earliest UTXO
/// block height, or `None` if no UTXOs exist or the RPC is unavailable.
async fn probe_transparent(
    client: &mut CompactTxStreamerClient<Channel>,
    seed_phrase: &SecretString,
    network: ZeckNetwork,
) -> ZeckResult<Option<u32>> {
    let accounts = derive_accounts(seed_phrase, network, PROBE_ACCOUNT_COUNT)?;
    let addresses: Vec<String> = accounts
        .iter()
        .flat_map(|a| {
            [
                a.transparent_receive_address.clone(),
                a.transparent_change_address.clone(),
            ]
        })
        .collect();

    let reply = client
        .get_address_utxos(GetAddressUtxosArg {
            addresses,
            start_height: 0,
            max_entries: 1_000,
        })
        .await
        .map_err(|err| ZeckError::Lightwalletd(err.to_string()))?
        .into_inner();

    let earliest = reply
        .address_utxos
        .iter()
        .filter_map(|utxo| u32::try_from(utxo.height).ok())
        .filter(|&h| h > 0)
        .min();

    Ok(earliest)
}

/// Create a temporary workspace at `probe_height`, import account-0, run a
/// time-limited compact-block sync (≤`PROBE_TIMEOUT_SECS` seconds), then check
/// whether any notes were written to the wallet DB.  Cleans up the temp
/// directory before returning.
async fn probe_shielded_window(
    client: &mut CompactTxStreamerClient<Channel>,
    lightwalletd_url: &str,
    seed_phrase: &SecretString,
    seed: &[u8; 64],
    transparent_account: &zcash_transparent::keys::AccountPrivKey,
    network: ZeckNetwork,
    probe_height: u32,
    sapling_floor: u32,
) -> ZeckResult<bool> {
    let probe_dir = std::env::temp_dir().join(format!("zeck_probe_{}", Uuid::new_v4()));
    let effective_height = probe_height.max(sapling_floor.saturating_add(1));

    let probe_config = RuntimeScanConfig {
        seed_phrase: SecretString::new(seed_phrase.expose_secret().to_owned()),
        birthday: effective_height,
        num_accounts: Some(1),
        gap_limit: 1,
        lightwalletd_url: lightwalletd_url.to_owned(),
        data_dir: probe_dir.clone(),
        network,
    };

    let workspace = RecoveryWorkspace::from_runtime(&probe_config)?;
    workspace.initialize(network, seed)?;

    let treestate = client
        .get_tree_state(BlockId {
            height: u64::from(effective_height.saturating_sub(1)),
            hash: vec![],
        })
        .await
        .map_err(|err| ZeckError::Lightwalletd(err.to_string()))?
        .into_inner();

    let account_birthday = AccountBirthday::from_treestate(treestate, None).map_err(|_| {
        ZeckError::Wallet("constructing probe account birthday from treestate".to_owned())
    })?;

    import_probe_account(&workspace, network, seed, &account_birthday, transparent_account)?;

    let net = consensus_network(network);
    let timed_out = tokio::time::timeout(
        std::time::Duration::from_secs(PROBE_TIMEOUT_SECS),
        run_wallet_sync(&workspace, &net, client),
    )
    .await
    .is_err();

    if timed_out {
        // Reconnect so the next probe starts with a clean channel.
        if let Ok((new_client, _, _)) = probe_lightwalletd_endpoints(lightwalletd_url).await {
            *client = new_client;
        }
    }

    let has_activity = check_probe_activity(workspace.wallet_db_path()).unwrap_or(false);
    let _ = std::fs::remove_dir_all(&probe_dir);
    Ok(has_activity)
}

fn check_probe_activity(wallet_db_path: &std::path::Path) -> ZeckResult<bool> {
    let conn = Connection::open_with_flags(
        wallet_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|err| {
        ZeckError::Storage(format!("opening probe wallet for activity check: {err}"))
    })?;

    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sapling_received_notes)
          OR EXISTS(SELECT 1 FROM orchard_received_notes)
          OR EXISTS(SELECT 1 FROM transparent_received_outputs)",
        [],
        |row| row.get(0),
    )
    .map_err(|err| ZeckError::Wallet(format!("checking probe activity: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sapling_activation_date_returns_activation_height() {
        let h = estimate_birthday_from_date("2018-10-28").unwrap();
        assert_eq!(h, SAPLING_ACTIVATION_HEIGHT);
    }

    #[test]
    fn pre_sapling_date_clamps_to_activation_height() {
        let h = estimate_birthday_from_date("2016-01-01").unwrap();
        assert_eq!(h, SAPLING_ACTIVATION_HEIGHT);
    }

    #[test]
    fn one_year_after_sapling_gives_plausible_height() {
        let h = estimate_birthday_from_date("2019-10-28").unwrap();
        let expected_min = SAPLING_ACTIVATION_HEIGHT + 400_000;
        let expected_max = SAPLING_ACTIVATION_HEIGHT + 450_000;
        assert!(
            h >= expected_min && h <= expected_max,
            "height {h} outside [{expected_min}, {expected_max}]"
        );
    }

    #[test]
    fn invalid_date_format_is_rejected() {
        assert!(estimate_birthday_from_date("28-10-2018").is_err());
        assert!(estimate_birthday_from_date("2018/10/28").is_err());
        assert!(estimate_birthday_from_date("not-a-date").is_err());
        assert!(estimate_birthday_from_date("").is_err());
    }

    #[test]
    fn future_date_produces_height_above_current_chain() {
        let h = estimate_birthday_from_date("2030-01-01").unwrap();
        assert!(h > 2_000_000, "expected large height, got {h}");
    }

    #[test]
    fn leap_year_february_29_is_handled() {
        let h = estimate_birthday_from_date("2020-02-29").unwrap();
        assert!(h > SAPLING_ACTIVATION_HEIGHT, "expected height above activation");
    }
}
