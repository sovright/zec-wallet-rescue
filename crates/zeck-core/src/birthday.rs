use time::{Date, Duration, Month};

use crate::error::{ZeckError, ZeckResult};

const SAPLING_ACTIVATION_HEIGHT: u32 = 419_200;
const SAPLING_ACTIVATION_DATE: (i32, Month, u8) = (2018, Month::October, 28);
const AVERAGE_BLOCK_SECONDS: i64 = 75;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sapling_activation_date_returns_activation_height() {
        // Exact Sapling activation date → activation height, not below it
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
        // 365 days × 86400 s/day ÷ 75 s/block ≈ 420 480 blocks above activation
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
        // Just sanity-checks that a far-future date gives a large block number
        let h = estimate_birthday_from_date("2030-01-01").unwrap();
        assert!(h > 2_000_000, "expected large height, got {h}");
    }

    #[test]
    fn leap_year_february_29_is_handled() {
        let h = estimate_birthday_from_date("2020-02-29").unwrap();
        assert!(h > SAPLING_ACTIVATION_HEIGHT, "expected height above activation");
    }
}
