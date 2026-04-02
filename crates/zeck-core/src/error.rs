use thiserror::Error;

pub type ZeckResult<T> = Result<T, ZeckError>;

#[derive(Debug, Error)]
pub enum ZeckError {
    #[error("invalid mnemonic: {0}")]
    InvalidMnemonic(String),

    #[error("invalid destination address: {0}")]
    InvalidAddress(String),

    #[error("destination must include an Orchard or Sapling receiver")]
    UnsupportedDestination,

    #[error("invalid scan configuration: {0}")]
    InvalidConfig(String),

    #[error("failed to parse date: {0}")]
    InvalidDate(String),

    #[error("lightwalletd probe failed: {0}")]
    Lightwalletd(String),

    #[error("scan session not found")]
    UnknownScanHandle,

    #[error("sweep execution is not implemented yet")]
    SweepNotImplemented,

    #[error("serialization error: {0}")]
    Serialization(String),

    #[error("internal error: {0}")]
    Internal(String),
}
