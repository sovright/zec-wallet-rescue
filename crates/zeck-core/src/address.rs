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
