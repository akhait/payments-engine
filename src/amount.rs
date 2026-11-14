//! Fixed-point money amounts.

use std::fmt;
use std::str::FromStr;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer};

/// Number of decimal places an amount carries.
const DECIMALS: usize = 4;

/// Number of internal units in one whole currency unit (10^DECIMALS).
const SCALE: i64 = 10_i64.pow(DECIMALS as u32);

/// A money amount stored as a whole number of ten-thousandths.
///
/// `1.5` is stored as `15_000`. Integer storage keeps arithmetic exact, and the
/// only way to combine amounts is through [`Amount::checked_add`] and
/// [`Amount::checked_sub`], so an overflow can never go unnoticed.
///
/// The value is signed because an account's available balance can go negative
/// after a dispute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Amount(i64);

/// Why a string could not be read as an [`Amount`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseAmountError {
    /// The input was empty or only whitespace.
    Empty,
    /// The input had a decimal point but no digits at all.
    NoDigits,
    /// The input contained something other than digits and one decimal point.
    InvalidCharacter,
    /// The input had more than four digits after the decimal point.
    TooManyDecimals,
    /// The value does not fit in the internal representation.
    OutOfRange,
}

impl fmt::Display for ParseAmountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Empty => "amount is empty",
            Self::NoDigits => "amount has no digits",
            Self::InvalidCharacter => "amount contains a character that is not a digit or '.'",
            Self::TooManyDecimals => "amount has more than four decimal places",
            Self::OutOfRange => "amount is too large",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ParseAmountError {}

impl Amount {
    /// Adds two amounts, returning `None` if the result would overflow.
    pub fn checked_add(self, rhs: Amount) -> Option<Amount> {
        self.0.checked_add(rhs.0).map(Amount)
    }

    /// Subtracts `rhs` from `self`, returning `None` if the result would overflow.
    pub fn checked_sub(self, rhs: Amount) -> Option<Amount> {
        self.0.checked_sub(rhs.0).map(Amount)
    }
}

impl FromStr for Amount {
    type Err = ParseAmountError;

    /// Accepts digits with an optional decimal point and at most four digits
    /// after it, surrounded by optional whitespace. Signs, exponents and
    /// thousands separators are rejected.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        if s.is_empty() {
            return Err(ParseAmountError::Empty);
        }

        let (whole, fraction) = s.split_once('.').unwrap_or((s, ""));
        if whole.is_empty() && fraction.is_empty() {
            return Err(ParseAmountError::NoDigits);
        }
        // A second '.' ends up in `fraction`, so it is rejected here as well.
        if !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|b| b.is_ascii_digit())
        {
            return Err(ParseAmountError::InvalidCharacter);
        }
        if fraction.len() > DECIMALS {
            return Err(ParseAmountError::TooManyDecimals);
        }

        // Only digits remain, so parsing can fail solely by overflowing.
        let whole_units: i64 = if whole.is_empty() {
            0
        } else {
            whole.parse().map_err(|_| ParseAmountError::OutOfRange)?
        };
        let fraction_units: i64 = if fraction.is_empty() {
            0
        } else {
            // "5" means five tenths, i.e. 5000 ten-thousandths, so pad on the right.
            let digits: i64 = fraction.parse().expect("at most four ASCII digits");
            digits * 10_i64.pow((DECIMALS - fraction.len()) as u32)
        };

        whole_units
            .checked_mul(SCALE)
            .and_then(|units| units.checked_add(fraction_units))
            .map(Amount)
            .ok_or(ParseAmountError::OutOfRange)
    }
}

/// Formats with exactly four decimals, for example `1.5000`.
///
/// A negative amount is printed with a leading `-`. That form is meant for
/// reports only: [`FromStr`] rejects signs on purpose, so it cannot be parsed
/// back.
impl fmt::Display for Amount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `unsigned_abs` is used so that `i64::MIN` does not overflow on negation.
        let sign = if self.0 < 0 { "-" } else { "" };
        let units = self.0.unsigned_abs();
        let scale = SCALE.unsigned_abs();
        write!(
            f,
            "{sign}{}.{:0width$}",
            units / scale,
            units % scale,
            width = DECIMALS
        )
    }
}

impl<'de> Deserialize<'de> for Amount {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct AmountVisitor;

        impl Visitor<'_> for AmountVisitor {
            type Value = Amount;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a decimal amount with at most four decimal places")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Amount, E> {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(AmountVisitor)
    }
}

#[cfg(test)]
mod tests {
    use serde::de::IntoDeserializer;
    use serde::de::value::{Error as ValueError, StrDeserializer};

    use super::*;

    fn deserialize(input: &str) -> Result<Amount, ValueError> {
        let deserializer: StrDeserializer<'_, ValueError> = input.into_deserializer();
        Amount::deserialize(deserializer)
    }

    #[test]
    fn parses_valid_amounts_into_ten_thousandths() {
        let cases = [
            ("1.5", 15_000),
            ("1", 10_000),
            ("0.0001", 1),
            (" 2.0 ", 20_000),
            ("0", 0),
            ("0.0", 0),
            ("12.3456", 123_456),
            (".5", 5_000),
            ("5.", 50_000),
            ("00012.50", 125_000),
            ("922337203685477.5807", i64::MAX),
        ];
        for (input, units) in cases {
            assert_eq!(
                input.parse::<Amount>(),
                Ok(Amount(units)),
                "input {input:?}"
            );
        }
    }

    #[test]
    fn rejects_malformed_amounts() {
        use ParseAmountError::*;
        let cases = [
            ("", Empty),
            ("   ", Empty),
            (".", NoDigits),
            ("abc", InvalidCharacter),
            ("-1", InvalidCharacter),
            ("+1", InvalidCharacter),
            ("1e3", InvalidCharacter),
            ("1.2.3", InvalidCharacter),
            ("1,5", InvalidCharacter),
            ("1.0 0", InvalidCharacter),
            ("1.00001", TooManyDecimals),
            ("922337203685477.5808", OutOfRange),
            ("99999999999999999999", OutOfRange),
            // Whole part fits in an i64 but overflows once scaled by 10_000.
            ("922337203685478", OutOfRange),
            ("9223372036854775807", OutOfRange),
        ];
        for (input, expected) in cases {
            assert_eq!(input.parse::<Amount>(), Err(expected), "input {input:?}");
        }
    }

    #[test]
    fn displays_with_exactly_four_decimals() {
        let cases = [
            (15_000, "1.5000"),
            (1, "0.0001"),
            (0, "0.0000"),
            (-50_000, "-5.0000"),
            (-1, "-0.0001"),
            (i64::MAX, "922337203685477.5807"),
            (i64::MIN, "-922337203685477.5808"),
        ];
        for (units, expected) in cases {
            assert_eq!(Amount(units).to_string(), expected);
        }
    }

    #[test]
    fn displayed_non_negative_amounts_parse_back_to_the_same_value() {
        for units in [0, 1, 9_999, 10_000, 123_456_789, i64::MAX] {
            let amount = Amount(units);
            assert_eq!(amount.to_string().parse::<Amount>(), Ok(amount));
        }
    }

    #[test]
    fn checked_add_adds_amounts() {
        assert_eq!(
            Amount(15_000).checked_add(Amount(5_000)),
            Some(Amount(20_000))
        );
    }

    #[test]
    fn checked_add_returns_none_on_overflow() {
        assert_eq!(Amount(i64::MAX).checked_add(Amount(1)), None);
        assert_eq!(Amount(i64::MIN).checked_add(Amount(-1)), None);
    }

    #[test]
    fn checked_sub_can_go_negative() {
        assert_eq!(
            Amount(10_000).checked_sub(Amount(15_000)),
            Some(Amount(-5_000))
        );
    }

    #[test]
    fn checked_sub_returns_none_on_overflow() {
        assert_eq!(Amount(i64::MIN).checked_sub(Amount(1)), None);
        assert_eq!(Amount(i64::MAX).checked_sub(Amount(-1)), None);
    }

    #[test]
    fn checked_arithmetic_succeeds_when_landing_exactly_on_the_limits() {
        assert_eq!(
            Amount(i64::MAX - 1).checked_add(Amount(1)),
            Some(Amount(i64::MAX))
        );
        assert_eq!(
            Amount(i64::MIN + 1).checked_sub(Amount(1)),
            Some(Amount(i64::MIN))
        );
    }

    #[test]
    fn checked_sub_returns_none_when_subtracting_the_minimum_from_zero() {
        assert_eq!(Amount(0).checked_sub(Amount(i64::MIN)), None);
    }

    #[test]
    fn deserializes_from_a_string() {
        assert_eq!(deserialize("1.5"), Ok(Amount(15_000)));
    }

    #[test]
    fn deserialize_reports_why_parsing_failed() {
        let error = deserialize("1.00001").unwrap_err();
        assert!(error.to_string().contains("more than four"), "got: {error}");
    }
}
