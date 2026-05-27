//! Donation feature: baked-in recipient and pure split/memo helpers.
//!
//! An empty `DONATION_ADDRESS` disables the feature everywhere — donation
//! outputs are never created and the GUI hides its donation affordances.

use crate::error::{ZeckError, ZeckResult};

/// Baked-in donation recipient. MUST be a shielded Unified Address (memos
/// require a shielded output). Empty string disables the donation feature
/// everywhere. Set this to the real address to activate.
pub const DONATION_ADDRESS: &str = "";

/// Fixed label so all sweep-sourced donations are identifiable when the
/// project scans the donation address's memos.
pub const DONATION_MEMO_TAG: &str = "Argos sweep donation v1";

/// Suggested default donation share, shown pre-filled in the GUI.
pub const DEFAULT_DONATION_RATE: f64 = 0.10;

/// Below this, no donation output is created for a transaction (0.001 ZEC).
/// Comfortably above the marginal ZIP-317 cost of one extra output.
pub const MIN_DONATION_ZATOSHIS: u64 = 100_000;

/// Max donor email length in bytes. Tag (23 bytes) + newline + 480 = 504,
/// under the 512-byte Zcash memo limit.
pub const MAX_DONOR_EMAIL_BYTES: usize = 480;

/// Whether the donation feature is active for a given baked address.
pub fn feature_enabled(address: &str) -> bool {
    !address.trim().is_empty()
}

/// Donation amount (zatoshis) for one account's send amount.
///
/// Returns 0 (no donation output) when the feature is disabled, the rate is
/// absent/zero, or the computed donation is below `MIN_DONATION_ZATOSHIS`.
/// The `donation >= send_amount` guard ensures the user's remainder stays
/// strictly positive even for callers that bypass `validate_donation_rate`.
/// Callers are responsible for skipping the feature on testnet.
pub fn donation_for_send_amount(address: &str, rate: Option<f64>, send_amount: u64) -> u64 {
    if !feature_enabled(address) {
        return 0;
    }
    let rate = match rate {
        Some(r) if r > 0.0 => r,
        _ => return 0,
    };
    let donation = (send_amount as f64 * rate).round() as u64;
    if donation < MIN_DONATION_ZATOSHIS || donation >= send_amount {
        return 0;
    }
    donation
}

/// Memo body for the donation output: tag alone, or tag + email line.
pub fn donation_memo_body(email: Option<&str>) -> String {
    match email.map(str::trim).filter(|e| !e.is_empty()) {
        Some(email) => format!("{DONATION_MEMO_TAG}\n{email}"),
        None => DONATION_MEMO_TAG.to_owned(),
    }
}

/// Validate the requested donation rate. `None` is valid (skip).
///
/// `Some(0.0)` is accepted and is equivalent to `None` at the computation
/// layer (no donation output). The rate must be `>= 0.0` and `< 1.0`: the
/// user cannot donate their entire balance and must keep some funds.
pub fn validate_donation_rate(rate: Option<f64>) -> ZeckResult<()> {
    match rate {
        None => Ok(()),
        Some(r) if (0.0..1.0).contains(&r) => Ok(()),
        Some(r) => Err(ZeckError::InvalidConfig(format!(
            "donation rate {r} must be >= 0.0 and < 1.0"
        ))),
    }
}

/// Lenient email validation. Empty/None is valid (no receipt requested).
pub fn validate_donor_email(email: Option<&str>) -> ZeckResult<()> {
    match email.map(str::trim).filter(|e| !e.is_empty()) {
        None => Ok(()),
        Some(e) if e.len() > MAX_DONOR_EMAIL_BYTES => Err(ZeckError::InvalidConfig(format!(
            "email too long (max {MAX_DONOR_EMAIL_BYTES} bytes)"
        ))),
        Some(e) if e.contains('@') && !e.starts_with('@') && !e.ends_with('@') => Ok(()),
        Some(e) => Err(ZeckError::InvalidConfig(format!("invalid email: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_when_address_empty() {
        assert!(!feature_enabled(""));
        assert_eq!(donation_for_send_amount("", Some(0.10), 1_000_000), 0);
    }

    #[test]
    fn no_donation_when_rate_none_or_zero() {
        assert_eq!(donation_for_send_amount(SOME_ADDR, None, 1_000_000), 0);
        assert_eq!(donation_for_send_amount(SOME_ADDR, Some(0.0), 1_000_000), 0);
    }

    #[test]
    fn donation_is_rate_times_send_amount_when_above_threshold() {
        // 10% of 2_000_000 = 200_000 >= MIN_DONATION_ZATOSHIS (100_000)
        assert_eq!(donation_for_send_amount(SOME_ADDR, Some(0.10), 2_000_000), 200_000);
    }

    #[test]
    fn donation_suppressed_below_threshold() {
        // 10% of 500_000 = 50_000 < 100_000 → no donation
        assert_eq!(donation_for_send_amount(SOME_ADDR, Some(0.10), 500_000), 0);
    }

    #[test]
    fn donation_zero_when_it_would_consume_entire_send_amount() {
        // rounds to 1_000_000 == send_amount → guard returns 0
        assert_eq!(donation_for_send_amount(SOME_ADDR, Some(0.9999999), 1_000_000), 0);
    }

    #[test]
    fn memo_is_tag_only_without_email() {
        assert_eq!(donation_memo_body(None), DONATION_MEMO_TAG.to_owned());
    }

    #[test]
    fn memo_appends_email_line_when_present() {
        let body = donation_memo_body(Some("a@b.com"));
        assert_eq!(body, format!("{DONATION_MEMO_TAG}\na@b.com"));
    }

    #[test]
    fn memo_omits_blank_email() {
        assert_eq!(donation_memo_body(Some("   ")), DONATION_MEMO_TAG.to_owned());
    }

    #[test]
    fn rate_validation_rejects_out_of_range() {
        assert!(validate_donation_rate(Some(1.5)).is_err());
        assert!(validate_donation_rate(Some(1.0)).is_err());
        assert!(validate_donation_rate(Some(-0.1)).is_err());
        assert!(validate_donation_rate(Some(0.99)).is_ok());
        assert!(validate_donation_rate(Some(0.10)).is_ok());
        assert!(validate_donation_rate(None).is_ok());
    }

    #[test]
    fn email_validation_is_lenient_but_requires_at() {
        assert!(validate_donor_email(None).is_ok());
        assert!(validate_donor_email(Some("")).is_ok());
        assert!(validate_donor_email(Some("a@b.com")).is_ok());
        assert!(validate_donor_email(Some("notanemail")).is_err());
    }

    #[test]
    fn email_validation_rejects_overlong() {
        let local = "a".repeat(MAX_DONOR_EMAIL_BYTES);
        let email = format!("{local}@b.com");
        assert!(email.len() > MAX_DONOR_EMAIL_BYTES);
        assert!(validate_donor_email(Some(&email)).is_err());
    }

    // A syntactically valid mainnet UA for tests (NOT the real donation address).
    const SOME_ADDR: &str = "u1nvgt6yr35mhc9wdf4wckvl38476vqy96dx3cwkfdwy4jet9300l5v8l2yg27ql7w9qwm0lf8kncnj9nus4mgete06j3cu3mhrqvstg6swvdya6xgzwhh6a9xxdhxkavvvmztqeuaurjtqfk3dzetuzgnu0zjvmdpe8ehvj53sy6yhzxj";
}
