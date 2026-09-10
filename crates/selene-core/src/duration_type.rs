//! Duration type qualifiers and duration field-family helpers.

/// ISO temporal duration type qualifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, serde::Deserialize, serde::Serialize)]
pub enum DurationTypeQualifier {
    /// `YEAR TO MONTH`.
    YearToMonth,
    /// `DAY TO SECOND`.
    DayToSecond,
}

/// Field family carried by a concrete duration value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DurationValueFamily {
    /// All fields are zero.
    Zero,
    /// Only year/month fields are non-zero.
    YearMonth,
    /// Only day/time fields are non-zero.
    DayTime,
}

/// Ordered key used for duration comparisons and duration-backed indexes.
///
/// The components are total months and total day/time nanoseconds. Comparable
/// values have at most one nonzero component; year/month and day/time values
/// are separate comparison groups. This transient key does not change the
/// stored field representation or assign a fixed length to a calendar month.
pub type DurationOrderKey = (i64, i128);

impl DurationTypeQualifier {
    /// Canonical GQL spelling for this qualifier.
    #[must_use]
    pub const fn gql_name(self) -> &'static str {
        match self {
            Self::YearToMonth => "YEAR TO MONTH",
            Self::DayToSecond => "DAY TO SECOND",
        }
    }

    /// Return true when `value` conforms to this qualified duration type.
    #[must_use]
    pub fn matches_span(self, value: &jiff::Span) -> bool {
        matches!(
            (self, duration_value_family(value)),
            (_, Some(DurationValueFamily::Zero))
                | (Self::YearToMonth, Some(DurationValueFamily::YearMonth))
                | (Self::DayToSecond, Some(DurationValueFamily::DayTime))
        )
    }
}

/// Return the duration field family, or `None` when year/month and day/time fields
/// are mixed in one span.
#[must_use]
pub fn duration_value_family(value: &jiff::Span) -> Option<DurationValueFamily> {
    let (months, nanos) = duration_order_key(value);
    let has_year_month = months != 0;
    let has_day_time = nanos != 0;
    match (has_year_month, has_day_time) {
        (false, false) => Some(DurationValueFamily::Zero),
        (true, false) => Some(DurationValueFamily::YearMonth),
        (false, true) => Some(DurationValueFamily::DayTime),
        (true, true) => None,
    }
}

/// Return the canonical ordered key for a duration span.
#[must_use]
pub fn duration_order_key(value: &jiff::Span) -> DurationOrderKey {
    let months = i64::from(value.get_years()) * 12 + i64::from(value.get_months());
    // Even treating every field as a full i64, eight fields times the largest
    // multiplier (604_800_000_000_000) fit below 2^116. Real jiff bounds are
    // smaller, so this exact i128 accumulation cannot overflow.
    let nanos = i128::from(value.get_weeks()) * 604_800_000_000_000
        + i128::from(value.get_days()) * 86_400_000_000_000
        + i128::from(value.get_hours()) * 3_600_000_000_000
        + i128::from(value.get_minutes()) * 60_000_000_000
        + i128::from(value.get_seconds()) * 1_000_000_000
        + i128::from(value.get_milliseconds()) * 1_000_000
        + i128::from(value.get_microseconds()) * 1_000
        + i128::from(value.get_nanoseconds());
    (months, nanos)
}

/// Whether two canonical duration keys belong to a common comparison group.
/// Zero belongs to both groups; a mixed-group value belongs to neither.
#[must_use]
pub fn duration_keys_comparable(left: DurationOrderKey, right: DurationOrderKey) -> bool {
    (left.0 == 0 && right.0 == 0) || (left.1 == 0 && right.1 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_units_zero_and_mixed_groups() {
        for (left, right) in [
            ("P1Y", "P12M"),
            ("P1W", "P7D"),
            ("PT1H", "PT60M"),
            ("-PT1H", "-PT60M"),
        ] {
            assert_eq!(
                duration_order_key(&left.parse().unwrap()),
                duration_order_key(&right.parse().unwrap())
            );
        }
        let zero = jiff::Span::new()
            .hours(1)
            .checked_sub(jiff::Span::new().minutes(60))
            .unwrap();
        assert_eq!(duration_order_key(&zero), (0, 0));
        assert_eq!(
            duration_value_family(&zero),
            Some(DurationValueFamily::Zero)
        );
        assert!(duration_keys_comparable((0, 0), (12, 0)));
        assert!(duration_keys_comparable((0, 0), (0, 1)));
        assert!(!duration_keys_comparable((1, 0), (0, 1)));
        assert!(!duration_keys_comparable((1, 1), (0, 0)));
    }
}
