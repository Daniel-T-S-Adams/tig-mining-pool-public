//! The W3C trace id a unit of work is correlated by.
//!
//! `architecture.md` §10.1: "Durable jobs and intents store the originating
//! trace ID so work resumed after a restart remains correlated." That is the
//! property this type exists for — a precommit intent admitted by the
//! controller and transmitted by the gateway, possibly after a crash and on a
//! different process, is one story, and the trace id is what makes it
//! readable as one.
//!
//! The shape is W3C's `trace-id`, so a later slice can carry a `traceparent`
//! across the member API without a migration. Slice 1 mints ids rather than
//! receiving them: it has no inbound HTTP, so there is no upstream context to
//! continue.

use std::fmt;
use std::str::FromStr;

/// A 16-byte trace id, rendered as 32 lowercase hex characters.
///
/// Stored rather than derived: §10.1's requirement is about work that resumes
/// in another process, where nothing about the local call stack survives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TraceId([u8; 16]);

impl TraceId {
    /// A new id drawn from the OS.
    ///
    /// Fallible, and no fallback: two units of work that shared a trace id
    /// would read as one story, which is worse than an intent recorded with
    /// no trace id at all — the caller can decide that, and a silent
    /// substitution cannot.
    pub fn draw() -> Result<Self, TraceIdError> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| TraceIdError::Unavailable(e.to_string()))?;
        // All-zero is W3C's "invalid" sentinel, so a drawn id must not be it.
        // Astronomically unlikely and cheap to exclude; left to chance it
        // would produce an id that parses nowhere.
        if bytes == [0u8; 16] {
            return Self::draw();
        }
        Ok(Self(bytes))
    }

    /// The 32-character lowercase hex form. What goes in a log field and in
    /// the `trace_id` column.
    pub fn to_hex(self) -> String {
        let mut s = String::with_capacity(32);
        for b in self.0 {
            // Two hex digits, always: `{:x}` would drop the leading zero of a
            // byte below 0x10 and shorten the id.
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TraceIdError {
    #[error("a trace id is 32 hex characters; got {0} character(s)")]
    WrongLength(usize),
    /// Uppercase included. W3C requires lowercase, and two spellings of one
    /// id would not group in a log search — which is the whole purpose.
    #[error("a trace id is lowercase hex; {0:?} is not")]
    NotLowercaseHex(String),
    /// W3C reserves all-zero as "invalid". Accepting it would let a caller
    /// that failed to obtain an id record one that looks present.
    #[error("a trace id of all zeroes is the W3C invalid sentinel, not an id")]
    AllZero,
    #[error("no trace id could be drawn from the OS: {0}")]
    Unavailable(String),
}

impl FromStr for TraceId {
    type Err = TraceIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 32 {
            return Err(TraceIdError::WrongLength(s.chars().count()));
        }
        if !s
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(TraceIdError::NotLowercaseHex(s.to_owned()));
        }
        let mut bytes = [0u8; 16];
        for (i, byte) in bytes.iter_mut().enumerate() {
            let pair = &s[i * 2..i * 2 + 2];
            *byte = u8::from_str_radix(pair, 16).map_err(|_| {
                // Unreachable given the check above, and reported rather than
                // panicked: a parse that can only be wrong by being edited
                // should say so, not abort a process holding a lease.
                TraceIdError::NotLowercaseHex(s.to_owned())
            })?;
        }
        if bytes == [0u8; 16] {
            return Err(TraceIdError::AllZero);
        }
        Ok(Self(bytes))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_drawn_id_round_trips_through_its_hex_form() {
        let id = TraceId::draw().expect("the OS has randomness");
        let hex = id.to_hex();
        assert_eq!(hex.len(), 32, "{hex}");
        assert_eq!(hex.parse::<TraceId>(), Ok(id));
    }

    #[test]
    fn every_byte_renders_as_two_digits() {
        // A byte below 0x10 rendered with `{:x}` loses its leading zero, and
        // the id silently shortens — which parses nowhere and correlates
        // nothing. Constructed rather than drawn, because drawing one with a
        // low byte in a particular position is not reliable.
        let id = TraceId([
            0x00, 0x0f, 0x10, 0xff, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12,
        ]);
        assert_eq!(id.to_hex(), "000f10ff0102030405060708090a0b0c");
        assert_eq!(id.to_hex().len(), 32);
    }

    #[test]
    fn two_draws_differ() {
        let a = TraceId::draw().expect("the OS has randomness");
        let b = TraceId::draw().expect("the OS has randomness");
        assert_ne!(a, b, "two units of work must not share a trace id");
    }

    #[test]
    fn the_spellings_that_would_not_group_in_a_search_are_refused() {
        let valid = "4bf92f3577b34da6a3ce929d0e0e4736";
        assert!(valid.parse::<TraceId>().is_ok());

        for (bad, why) in [
            ("", "empty"),
            ("4bf92f3577b34da6a3ce929d0e0e473", "31 characters"),
            ("4bf92f3577b34da6a3ce929d0e0e47366", "33 characters"),
            ("4BF92F3577B34DA6A3CE929D0E0E4736", "uppercase"),
            ("4bf92f3577b34da6a3ce929d0e0e473g", "not hex"),
            ("4bf92f3577b34da6a3ce929d0e0e473 ", "trailing space"),
            (
                "00000000000000000000000000000000",
                "the W3C invalid sentinel",
            ),
        ] {
            assert!(
                bad.parse::<TraceId>().is_err(),
                "{why}: {bad:?} must not parse"
            );
        }
    }
}
