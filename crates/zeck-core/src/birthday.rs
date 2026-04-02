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
