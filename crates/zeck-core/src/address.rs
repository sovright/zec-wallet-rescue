use std::str::FromStr;

use zcash_address::unified::{self, Encoding};
use zcash_address::ZcashAddress;
use zcash_protocol::PoolType;

use crate::{
    error::{ZeckError, ZeckResult},
    models::AddressInfo,
};

pub fn validate_destination_address(address: &str) -> ZeckResult<AddressInfo> {
    let parsed = ZcashAddress::from_str(address)
        .or_else(|_| ZcashAddress::try_from_encoded(address))
        .map_err(|err| ZeckError::InvalidAddress(err.to_string()))?;

    let has_orchard = parsed.can_receive_as(PoolType::ORCHARD);
    let has_sapling = parsed.can_receive_as(PoolType::SAPLING);
    let has_transparent = parsed.can_receive_as(PoolType::TRANSPARENT);
    let is_unified = unified::Address::decode(address).is_ok();

    if !is_unified {
        return Err(ZeckError::DestinationMustBeUnified);
    }

    let destination_ok = has_orchard || has_sapling;

    if !destination_ok {
        return Err(ZeckError::UnsupportedDestination);
    }

    Ok(AddressInfo {
        encoded: address.to_owned(),
        is_unified,
        has_orchard,
        has_sapling,
        has_transparent,
        destination_ok,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real mainnet unified address with Orchard + Sapling receivers derived from the
    // BIP-39 all-"abandon" test vector (account 0, produced by zeck-cli show-keys).
    const UA_ORCHARD_SAPLING: &str =
        "u1nvgt6yr35mhc9wdf4wckvl38476vqy96dx3cwkfdwy4jet9300l5v8l2yg27ql7w9qwm0lf8kncnj9nus4mgete06j3cu3mhrqvstg6swvdya6xgzwhh6a9xxdhxkavvvmztqeuaurjtqfk3dzetuzgnu0zjvmdpe8ehvj53sy6yhzxj";

    #[test]
    fn valid_unified_address_accepted() {
        let info = validate_destination_address(UA_ORCHARD_SAPLING).unwrap();
        assert!(info.is_unified);
        assert!(info.has_orchard);
        assert!(info.has_sapling);
        assert!(info.destination_ok);
        assert!(!info.has_transparent);
    }

    #[test]
    fn transparent_address_rejected() {
        // t1 address — not a unified address at all
        let err = validate_destination_address("t1dUDJ62ANtmebE8drFg7g2MWYwXHQ6Xu3F").unwrap_err();
        assert!(
            matches!(err, ZeckError::DestinationMustBeUnified),
            "got {err:?}"
        );
    }

    #[test]
    fn sapling_address_rejected() {
        // zs1 address — valid Zcash address but not unified
        let err = validate_destination_address(
            "zs16uhd4mux24se6wkm74vld0ec63d4dxt3d7m80l5xytreplkkllrrf9c7fj859mhp8tkcq9hxfvj",
        )
        .unwrap_err();
        assert!(
            matches!(err, ZeckError::DestinationMustBeUnified),
            "got {err:?}"
        );
    }

    #[test]
    fn garbage_string_rejected() {
        let err = validate_destination_address("not-an-address").unwrap_err();
        assert!(
            matches!(err, ZeckError::InvalidAddress(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn empty_string_rejected() {
        let err = validate_destination_address("").unwrap_err();
        assert!(
            matches!(err, ZeckError::InvalidAddress(_)),
            "got {err:?}"
        );
    }
}
