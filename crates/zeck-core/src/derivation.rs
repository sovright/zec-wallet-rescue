use bip0039::{English, Mnemonic};
use secrecy::{ExposeSecret, SecretString};
use zcash_address::unified::{Address as UnifiedAddress, Encoding, Receiver};
use zcash_keys::{encoding::AddressCodec, keys::sapling};
use zcash_protocol::consensus::{MAIN_NETWORK, TEST_NETWORK};
use zcash_transparent::keys::{AccountPrivKey, IncomingViewingKey, NonHardenedChildIndex};
use zip32::AccountId;

use crate::{
    error::{ZeckError, ZeckResult},
    models::{AddressScope, DerivedAccount, ZeckNetwork},
};

pub fn validate_mnemonic_words(words: &[String]) -> ZeckResult<()> {
    let phrase = normalize_words(words)?;
    let _ = Mnemonic::<English>::from_phrase(phrase)
        .map_err(|err| ZeckError::InvalidMnemonic(err.to_string()))?;
    Ok(())
}

pub fn derive_accounts(
    seed_phrase: &SecretString,
    network: ZeckNetwork,
    account_count: u32,
) -> ZeckResult<Vec<DerivedAccount>> {
    let mnemonic = Mnemonic::<English>::from_phrase(seed_phrase.expose_secret())
        .map_err(|err| ZeckError::InvalidMnemonic(err.to_string()))?;
    let seed = mnemonic.to_seed("");

    let transparent_account = match network {
        ZeckNetwork::Mainnet => AccountPrivKey::from_seed(&MAIN_NETWORK, &seed, AccountId::ZERO),
        ZeckNetwork::Testnet => AccountPrivKey::from_seed(&TEST_NETWORK, &seed, AccountId::ZERO),
    }
    .map_err(|err| ZeckError::Internal(err.to_string()))?;

    let external_ivk = transparent_account
        .to_account_pubkey()
        .derive_external_ivk()
        .map_err(|err| ZeckError::Internal(err.to_string()))?;
    let internal_ivk = transparent_account
        .to_account_pubkey()
        .derive_internal_ivk()
        .map_err(|err| ZeckError::Internal(err.to_string()))?;

    (0..account_count)
        .map(|index| derive_account(index, network, &seed, &external_ivk, &internal_ivk))
        .collect()
}

fn derive_account(
    index: u32,
    network: ZeckNetwork,
    seed: &[u8; 64],
    external_ivk: &zcash_transparent::keys::ExternalIvk,
    internal_ivk: &zcash_transparent::keys::InternalIvk,
) -> ZeckResult<DerivedAccount> {
    let zip32_index = AccountId::try_from(index)
        .map_err(|_| ZeckError::InvalidConfig(format!("account index {index} is out of range")))?;

    let sapling_extsk = sapling::spending_key(seed, network.coin_type(), zip32_index);
    let sapling_address = sapling_extsk
        .to_diversifiable_full_viewing_key()
        .default_address()
        .1;

    let orchard_sk =
        orchard::keys::SpendingKey::from_zip32_seed(seed, network.coin_type(), zip32_index)
            .map_err(|err| ZeckError::Internal(err.to_string()))?;
    let orchard_fvk = orchard::keys::FullViewingKey::from(&orchard_sk);
    let orchard_address = orchard_fvk.address_at(0u32, orchard::keys::Scope::External);

    let unified = UnifiedAddress::try_from_items(vec![
        Receiver::Orchard(orchard_address.to_raw_address_bytes()),
        Receiver::Sapling(sapling_address.to_bytes()),
    ])
    .map_err(|err| ZeckError::Internal(err.to_string()))?;

    let child_index = NonHardenedChildIndex::from_index(index).ok_or_else(|| {
        ZeckError::InvalidConfig(format!("transparent index {index} is out of range"))
    })?;

    let transparent_receive = external_ivk
        .derive_address(child_index)
        .map_err(|err| ZeckError::Internal(err.to_string()))?;
    let transparent_change = internal_ivk
        .derive_address(child_index)
        .map_err(|err| ZeckError::Internal(err.to_string()))?;

    let sapling_encoded = match network {
        ZeckNetwork::Mainnet => sapling_address.encode(&MAIN_NETWORK),
        ZeckNetwork::Testnet => sapling_address.encode(&TEST_NETWORK),
    };
    let unified_encoded = unified.encode(&match network {
        ZeckNetwork::Mainnet => zcash_protocol::consensus::NetworkType::Main,
        ZeckNetwork::Testnet => zcash_protocol::consensus::NetworkType::Test,
    });
    let transparent_receive_encoded = match network {
        ZeckNetwork::Mainnet => transparent_receive.encode(&MAIN_NETWORK),
        ZeckNetwork::Testnet => transparent_receive.encode(&TEST_NETWORK),
    };
    let transparent_change_encoded = match network {
        ZeckNetwork::Mainnet => transparent_change.encode(&MAIN_NETWORK),
        ZeckNetwork::Testnet => transparent_change.encode(&TEST_NETWORK),
    };

    Ok(DerivedAccount {
        index,
        sapling_path: format!("m_Sapling / 32' / {}' / {}'", network.coin_type(), index),
        orchard_path: format!("m_Orchard / 32' / {}' / {}'", network.coin_type(), index),
        transparent_receive_path: transparent_path(
            AddressScope::External,
            index,
            network.coin_type(),
        ),
        transparent_change_path: transparent_path(
            AddressScope::Internal,
            index,
            network.coin_type(),
        ),
        sapling_address: sapling_encoded,
        unified_address: unified_encoded,
        transparent_receive_address: transparent_receive_encoded,
        transparent_change_address: transparent_change_encoded,
    })
}

fn transparent_path(scope: AddressScope, index: u32, coin_type: u32) -> String {
    let scope_number = match scope {
        AddressScope::External => 0,
        AddressScope::Internal => 1,
    };

    format!("m / 44' / {coin_type}' / 0' / {scope_number} / {index}")
}

fn normalize_words(words: &[String]) -> ZeckResult<String> {
    if words.len() != 24 {
        return Err(ZeckError::InvalidMnemonic(format!(
            "expected 24 words, got {}",
            words.len()
        )));
    }

    Ok(words
        .iter()
        .map(|word| word.trim())
        .collect::<Vec<_>>()
        .join(" "))
}
