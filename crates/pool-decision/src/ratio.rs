//! Exact non-negative rationals for the §6.3 and §6.5 comparisons.
//!
//! `mining_system.md` writes factors and rates as real-valued ratios and does
//! not fix an arithmetic. Floating point would make a comparison's outcome
//! depend on a rounding direction, and §6.3's whole purpose is that two
//! challenges either tie or do not — a tie is what triggers the recorded draw,
//! so getting one wrong silently substitutes an unrecorded decision for a
//! recorded one. These are exact.
//!
//! Every operation is checked. An overflow returns an error rather than
//! wrapping or panicking: a wrapped qualifier count would select the wrong
//! challenge and no test would see it.

use std::cmp::Ordering;

/// A non-negative exact rational, kept in lowest terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratio {
    numerator: u128,
    denominator: u128,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RatioError {
    #[error("a ratio cannot have a zero denominator")]
    ZeroDenominator,
    #[error("exact arithmetic overflowed: {0}")]
    Overflow(&'static str),
}

impl Ratio {
    pub const ZERO: Self = Self {
        numerator: 0,
        denominator: 1,
    };

    /// `numerator / denominator`, reduced.
    pub fn new(numerator: u128, denominator: u128) -> Result<Self, RatioError> {
        if denominator == 0 {
            return Err(RatioError::ZeroDenominator);
        }
        let divisor = gcd(numerator, denominator);
        Ok(Self {
            numerator: numerator / divisor,
            denominator: denominator / divisor,
        })
    }

    pub fn whole(value: u128) -> Self {
        Self {
            numerator: value,
            denominator: 1,
        }
    }

    pub fn is_zero(self) -> bool {
        self.numerator == 0
    }

    pub fn numerator(self) -> u128 {
        self.numerator
    }

    pub fn denominator(self) -> u128 {
        self.denominator
    }

    pub fn checked_add(self, other: Self) -> Result<Self, RatioError> {
        let divisor = gcd(self.denominator, other.denominator);
        let other_scale = other.denominator / divisor;
        let left = self
            .numerator
            .checked_mul(other_scale)
            .ok_or(RatioError::Overflow("add: left numerator"))?;
        let right = other
            .numerator
            .checked_mul(self.denominator / divisor)
            .ok_or(RatioError::Overflow("add: right numerator"))?;
        let numerator = left
            .checked_add(right)
            .ok_or(RatioError::Overflow("add: numerator"))?;
        let denominator = self
            .denominator
            .checked_mul(other_scale)
            .ok_or(RatioError::Overflow("add: denominator"))?;
        Self::new(numerator, denominator)
    }

    pub fn checked_mul_int(self, factor: u128) -> Result<Self, RatioError> {
        let numerator = self
            .numerator
            .checked_mul(factor)
            .ok_or(RatioError::Overflow("mul: numerator"))?;
        Self::new(numerator, self.denominator)
    }

    /// Compares by cross-multiplication, which is exact for reduced terms.
    pub fn checked_cmp(self, other: Self) -> Result<Ordering, RatioError> {
        let left = self
            .numerator
            .checked_mul(other.denominator)
            .ok_or(RatioError::Overflow("cmp: left"))?;
        let right = other
            .numerator
            .checked_mul(self.denominator)
            .ok_or(RatioError::Overflow("cmp: right"))?;
        Ok(left.cmp(&right))
    }
}

fn gcd(a: u128, b: u128) -> u128 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let remainder = a % b;
        a = b;
        b = remainder;
    }
    // gcd(0, 0) would be 0 and is never a valid divisor; `new` only calls this
    // with a non-zero denominator, so `a` is non-zero here.
    if a == 0 { 1 } else { a }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn equal_values_written_differently_compare_equal() {
        // The §6.3 tie test: 2/20 and 3/30 are the same factor, and the
        // fixture's two_way_tie case depends on exactly this.
        let a = Ratio::new(2, 20).unwrap();
        let b = Ratio::new(3, 30).unwrap();
        assert_eq!(a.checked_cmp(b).unwrap(), Ordering::Equal);
        assert_eq!(a, b, "reduced terms make equal ratios structurally equal");
    }

    #[test]
    fn the_closest_fixture_comparison_is_exact() {
        // fixtures/decision-engine/v1/README.md names 29/137 vs 1/5 as the
        // closest comparison in the set. 29*5 = 145 > 137, so 29/137 > 1/5.
        let a = Ratio::new(29, 137).unwrap();
        let b = Ratio::new(1, 5).unwrap();
        assert_eq!(a.checked_cmp(b).unwrap(), Ordering::Greater);
    }

    #[test]
    fn addition_of_unlike_denominators_is_exact() {
        let sum = Ratio::new(1, 3)
            .unwrap()
            .checked_add(Ratio::new(1, 6).unwrap())
            .unwrap();
        assert_eq!(sum, Ratio::new(1, 2).unwrap());
    }

    #[test]
    fn zero_denominator_is_an_error_not_a_panic() {
        assert_eq!(Ratio::new(1, 0), Err(RatioError::ZeroDenominator));
    }

    #[test]
    fn overflow_is_reported_rather_than_wrapped() {
        let huge = Ratio::new(u128::MAX, 1).unwrap();
        assert!(matches!(
            huge.checked_mul_int(2),
            Err(RatioError::Overflow(_))
        ));
        assert!(matches!(
            huge.checked_add(Ratio::new(1, 2).unwrap()),
            Err(RatioError::Overflow(_))
        ));
        assert!(matches!(
            huge.checked_cmp(Ratio::new(1, 2).unwrap()),
            Err(RatioError::Overflow(_))
        ));
    }
}
