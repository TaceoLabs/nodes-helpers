//! Serde deserialization of [`Duration`] from human-friendly strings such as
//! `"10s"`, `"1m 30s"`, `"500ms"` or `"1.5h"`.
//!
//! A value is a sequence of `<number><unit>` spans, optionally separated by whitespace. The
//! number is a non-negative decimal, the unit one of:
//!
//! * `nanos`, `nsec`, `ns`
//! * `usec`, `us`, `µs`
//! * `millis`, `msec`, `ms`
//! * `seconds`, `second`, `secs`, `sec`, `s`
//! * `minutes`, `minute`, `mins`, `min`, `m`
//! * `hours`, `hour`, `hrs`, `hr`, `h`
//! * `days`, `day`, `d`
//! * `weeks`, `week`, `wks`, `wk`, `w`
//! * `months`, `month`, `M` (30.44 days)
//! * `years`, `year`, `yrs`, `yr`, `y` (365.25 days)
//!
//! The bare value `"0"` is accepted as a zero duration. Fractions must be exactly representable in
//! nanoseconds.
//!
//! Use [`deserialize`] for `Duration` fields and [`option::deserialize`] for `Option<Duration>`
//! fields via `#[serde(with = "...")]`/`#[serde(deserialize_with = "...")]`.

use std::{fmt, time::Duration};

use serde::{Deserializer, de};

const NANOS_PER_SEC: u128 = 1_000_000_000;

/// Deserializes a [`Duration`] from a human-friendly string.
///
/// # Errors
///
/// Returns an error if the input is not a string or is not a valid duration.
pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
where
    D: Deserializer<'de>,
{
    deserializer.deserialize_str(DurationVisitor)
}

/// Deserialization of `Option<Duration>`.
#[cfg(feature = "web3")]
pub(crate) mod option {
    use std::{fmt, time::Duration};

    use serde::{Deserializer, de};

    use super::DurationVisitor;

    /// Deserializes an `Option<Duration>` from a human-friendly string or `null`.
    ///
    /// # Errors
    ///
    /// Returns an error if the input is neither `null` nor a valid duration string.
    pub(crate) fn deserialize<'de, D>(deserializer: D) -> Result<Option<Duration>, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OptionVisitor;

        impl<'de> de::Visitor<'de> for OptionVisitor {
            type Value = Option<Duration>;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration or null")
            }

            fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
                Ok(None)
            }

            fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Self::Value, D::Error> {
                d.deserialize_str(DurationVisitor).map(Some)
            }
        }

        deserializer.deserialize_option(OptionVisitor)
    }
}

struct DurationVisitor;

impl de::Visitor<'_> for DurationVisitor {
    type Value = Duration;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a duration such as \"10s\" or \"1m 30s\"")
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Duration, E> {
        parse(value).map_err(|reason| E::custom(format!("invalid duration {value:?}: {reason}")))
    }
}

fn parse(input: &str) -> Result<Duration, &'static str> {
    if input == "0" {
        return Ok(Duration::ZERO);
    }
    let mut rest = input.trim_start();
    if rest.is_empty() {
        return Err("value is empty");
    }
    let mut total_nanos: u128 = 0;
    while !rest.is_empty() {
        let (span_nanos, remainder) = parse_span(rest)?;
        total_nanos = total_nanos
            .checked_add(span_nanos)
            .ok_or("value is too large")?;
        rest = remainder.trim_start();
    }
    let secs = u64::try_from(total_nanos / NANOS_PER_SEC).map_err(|_| "value is too large")?;
    let subsec_nanos = u32::try_from(total_nanos % NANOS_PER_SEC).expect("below one second");
    Ok(Duration::new(secs, subsec_nanos))
}

/// Parses one `<number><unit>` span and returns its length in nanoseconds and the remaining input.
fn parse_span(input: &str) -> Result<(u128, &str), &'static str> {
    let (integer, input) = split_digits(input);
    if integer.is_empty() {
        return Err("expected a number");
    }
    let (fraction, input) = match input.strip_prefix('.') {
        Some(after_dot) => {
            let (fraction, input) = split_digits(after_dot);
            if fraction.is_empty() {
                return Err("expected digits after the decimal point");
            }
            (fraction, input)
        }
        None => ("", input),
    };

    let input = input.trim_start();
    let unit_len = input
        .find(|c: char| !(c.is_ascii_alphabetic() || c == 'µ'))
        .unwrap_or(input.len());
    let (unit, remainder) = input.split_at(unit_len);
    if unit.is_empty() {
        return Err("expected a time unit");
    }
    let unit_nanos = unit_nanos(unit).ok_or("unknown time unit")?;

    let integer: u128 = integer.parse().map_err(|_| "value is too large")?;
    let mut nanos = integer
        .checked_mul(unit_nanos)
        .ok_or("value is too large")?;
    if !fraction.is_empty() {
        let numerator: u128 = fraction.parse().map_err(|_| "value is too precise")?;
        let denominator = u32::try_from(fraction.len())
            .ok()
            .and_then(|digits| 10u128.checked_pow(digits))
            .ok_or("value is too precise")?;
        let scaled = numerator
            .checked_mul(unit_nanos)
            .ok_or("value is too large")?;
        if scaled % denominator != 0 {
            return Err("value is too precise");
        }
        nanos = nanos
            .checked_add(scaled / denominator)
            .ok_or("value is too large")?;
    }
    Ok((nanos, remainder))
}

fn split_digits(input: &str) -> (&str, &str) {
    let end = input
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(input.len());
    input.split_at(end)
}

fn unit_nanos(unit: &str) -> Option<u128> {
    const MICRO: u128 = 1_000;
    const MILLI: u128 = 1_000_000;
    const SEC: u128 = NANOS_PER_SEC;
    const MIN: u128 = 60 * SEC;
    const HOUR: u128 = 60 * MIN;
    const DAY: u128 = 24 * HOUR;
    const WEEK: u128 = 7 * DAY;
    const MONTH: u128 = 2_630_016 * SEC;
    const YEAR: u128 = 31_557_600 * SEC;
    let nanos = match unit {
        "nanos" | "nsec" | "ns" => 1,
        "usec" | "us" | "µs" => MICRO,
        "millis" | "msec" | "ms" => MILLI,
        "seconds" | "second" | "secs" | "sec" | "s" => SEC,
        "minutes" | "minute" | "mins" | "min" | "m" => MIN,
        "hours" | "hour" | "hrs" | "hr" | "h" => HOUR,
        "days" | "day" | "d" => DAY,
        "weeks" | "week" | "wks" | "wk" | "w" => WEEK,
        "months" | "month" | "M" => MONTH,
        "years" | "year" | "yrs" | "yr" | "y" => YEAR,
        _ => return None,
    };
    Some(nanos)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;
    use serde_json::json;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct Required {
        #[serde(deserialize_with = "deserialize")]
        value: Duration,
    }

    #[derive(Debug, Deserialize)]
    struct Optional {
        #[serde(default, deserialize_with = "option::deserialize")]
        value: Option<Duration>,
    }

    #[test]
    fn parses_supported_formats() {
        let cases = [
            ("0", Duration::ZERO),
            ("0s", Duration::ZERO),
            ("10s", Duration::from_secs(10)),
            ("500ms", Duration::from_millis(500)),
            ("1m 30s", Duration::from_secs(90)),
            ("1m30s", Duration::from_secs(90)),
            ("  2h 37min  ", Duration::from_mins(157)),
            ("1 hour 2 minutes", Duration::from_mins(62)),
            ("4.2s", Duration::from_millis(4200)),
            ("1.5h", Duration::from_mins(90)),
            ("250µs", Duration::from_micros(250)),
            ("7ns", Duration::from_nanos(7)),
            ("2d", Duration::from_hours(48)),
            ("1w", Duration::from_hours(168)),
            ("1M", Duration::from_secs(2_630_016)),
            ("1y", Duration::from_hours(8766)),
        ];
        for (input, expected) in cases {
            assert_eq!(parse(input), Ok(expected), "input {input:?}");
        }
    }

    #[test]
    fn rejects_invalid_formats() {
        let cases = [
            "",
            "   ",
            "10",
            "s",
            "-1s",
            "1x",
            "1.s",
            "1.5ns",
            "0.0000000001s",
            "1s 2",
            "1 m s",
            "99999999999999999999999999999999999999y",
        ];
        for input in cases {
            parse(input).expect_err("must be rejected");
        }
    }

    #[test]
    fn deserializes_required_field() -> eyre::Result<()> {
        let parsed: Required = serde_json::from_value(json!({ "value": "1m 30s" }))?;
        assert_eq!(parsed.value, Duration::from_secs(90));
        serde_json::from_value::<Required>(json!({ "value": 10 })).expect_err("must be rejected");
        serde_json::from_value::<Required>(json!({ "value": "nope" }))
            .expect_err("must be rejected");
        serde_json::from_value::<Required>(json!({})).expect_err("must be rejected");
        Ok(())
    }

    #[test]
    fn deserializes_optional_field() -> eyre::Result<()> {
        let some: Optional = serde_json::from_value(json!({ "value": "500ms" }))?;
        assert_eq!(some.value, Some(Duration::from_millis(500)));
        let null: Optional = serde_json::from_value(json!({ "value": null }))?;
        assert_eq!(null.value, None);
        let missing: Optional = serde_json::from_value(json!({}))?;
        assert_eq!(missing.value, None);
        serde_json::from_value::<Optional>(json!({ "value": "nope" }))
            .expect_err("must be rejected");
        Ok(())
    }
}
