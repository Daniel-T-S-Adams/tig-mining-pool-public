//! ADR 0011's member account: a Base address, proved by a signature.
//!
//! "A member signs in by connecting a wallet, and that wallet is where their
//! money goes." The address is the identity and the withdrawal destination in
//! one, so proving control of it is the whole of authentication for a member
//! account — there is no password, no session secret, and no recovery.
//!
//! `accounting.md` §12.2 owns the rule and names what the signature must
//! carry: "a Base address proved by a domain-separated EIP-191 or EIP-712
//! signature containing pool domain, chain ID, address, random nonce,
//! purpose, and expiry", with "one-time nonces and exact-domain validation to
//! prevent signature reuse on a different pool or chain". It also says the
//! address "*is* the member identity, so it is not carried separately in the
//! signed payload" — the address this module returns is **recovered from the
//! signature**, never read from the request.
//!
//! EIP-191 (`personal_sign`) rather than EIP-712: every wallet implements it,
//! the payload below is already domain-separated by its first line the way
//! every other signing string in this crate is, and a member reading the text
//! in their wallet can see what they are agreeing to.

use k256::ecdsa::{RecoveryId, Signature, VerifyingKey};
use sha3::{Digest as _, Keccak256};

use crate::{IdentityError, IdentityErrorCode, IdentityResult};

fn refused(detail: impl Into<String>) -> IdentityError {
    IdentityError::new(IdentityErrorCode::NotAuthenticated, detail)
}

/// What a member is proving control of their address *for*.
///
/// §12.2 lists "purpose" among the signed fields, and a caller states which
/// one it expects: a signature obtained for one purpose must not authorise
/// another, and comparing against an expectation is what makes that true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginPurpose {
    /// Authorises an enrollment ticket (`member_protocol.md` §3.1).
    WorkerEnrollment,
    /// Authorises a worker-recovery ticket (§3.3). ADR 0011: there is no
    /// account recovery, so this recovers a worker and never the account.
    WorkerRecovery,
}

impl LoginPurpose {
    /// The token that appears in the signed text.
    pub fn as_str(self) -> &'static str {
        match self {
            LoginPurpose::WorkerEnrollment => "WORKER_ENROLLMENT",
            LoginPurpose::WorkerRecovery => "WORKER_RECOVERY",
        }
    }
}

/// The exact bytes a member signs.
///
/// Domain-separated by its first line, like every other signing string here
/// (§3.1's `TIG-POOL-ENROLLMENT-V1`, §3.2's `TIG-POOL-REQUEST-V1`). UTF-8, no
/// final newline.
///
/// ```text
/// TIG-POOL-LOGIN-V1
/// <pool domain>
/// <chain id>
/// <purpose>
/// <nonce>
/// <expires at, RFC 3339 UTC, whole seconds>
/// ```
///
/// The address is deliberately absent: §12.2 says it "is the member identity,
/// so it is not carried separately in the signed payload". Including it would
/// invite reading the address from the request and checking it against the
/// recovered one, which is the same answer with one more thing to get wrong.
#[must_use]
pub fn login_signing_string(
    pool_domain: &str,
    chain_id: u64,
    purpose: LoginPurpose,
    nonce: &str,
    expires_at_rfc3339: &str,
) -> String {
    format!(
        "TIG-POOL-LOGIN-V1\n{pool_domain}\n{chain_id}\n{}\n{nonce}\n{expires_at_rfc3339}",
        purpose.as_str()
    )
}

/// What the pool requires of a login signature, stated by the caller.
///
/// A struct rather than a pile of arguments because every field is a thing
/// the *pool* fixes: a value taken from the request instead would be the
/// caller choosing which pool, which chain, or which purpose they are
/// signing for.
#[derive(Debug, Clone)]
pub struct LoginExpectation<'a> {
    /// §12.2's "exact-domain validation". Compared byte for byte, never by
    /// suffix: `tig-pool.example.com.attacker.test` ends with the real one.
    pub pool_domain: &'a str,
    /// Base. A signature for another chain is for another pool's world.
    pub chain_id: u64,
    pub purpose: LoginPurpose,
    /// The nonce this signature must carry. Single use is the caller's to
    /// enforce durably (`pool.wallet_login_nonce`); this only checks that the
    /// signature is over the one expected.
    pub nonce: &'a str,
    /// The expiry the signature carries, as it appears in the text.
    pub expires_at_rfc3339: &'a str,
}

/// A proved member address: lowercase `0x` hex, as `pool.member` stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvedAddress(String);

impl ProvedAddress {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Recover the address that signed `expectation`, or refuse.
///
/// `signature` is the 65-byte `r || s || v` a wallet returns, as unprefixed
/// or `0x`-prefixed hex. `now_unix` is server time: §12.2 requires an expiry,
/// and the expiry means nothing unless something compares it to a clock the
/// member does not control.
///
/// Every refusal is the same error code. A member who signed for another pool
/// learns only that the pool would not take it — the same as every other
/// authentication failure in this crate.
pub fn recover_login_address(
    expectation: &LoginExpectation<'_>,
    signature: &str,
    now_unix: u64,
) -> IdentityResult<ProvedAddress> {
    // The expiry is checked before the cryptography, because it needs no
    // secret and an expired signature is not worth a recovery. It is parsed
    // from the same text that was signed, so a caller cannot present one
    // expiry and sign another.
    let expires_at = parse_rfc3339_seconds(expectation.expires_at_rfc3339)
        .ok_or_else(|| refused("expiry is not RFC 3339 with whole seconds"))?;
    if now_unix >= expires_at {
        return Err(refused("the signature has expired"));
    }

    let message = login_signing_string(
        expectation.pool_domain,
        expectation.chain_id,
        expectation.purpose,
        expectation.nonce,
        expectation.expires_at_rfc3339,
    );

    let bytes = decode_hex(signature).ok_or_else(|| refused("signature is not hex"))?;
    // `Signature::from_slice` below requires exactly 64 bytes, so the length
    // is checked there rather than twice; this split only separates the
    // recovery byte from the rest.
    let [r_s @ .., v] = bytes.as_slice() else {
        return Err(refused("signature is empty"));
    };

    // Wallets emit `v` as 27 or 28; some libraries emit 0 or 1. Both are the
    // same recovery id, and anything else is not a signature this pool reads.
    let recovery = match v {
        0 | 27 => RecoveryId::from_byte(0),
        1 | 28 => RecoveryId::from_byte(1),
        _ => None,
    }
    .ok_or_else(|| refused("signature recovery id is not 0, 1, 27, or 28"))?;

    let signature =
        Signature::from_slice(r_s).map_err(|_| refused("signature r/s is not a valid scalar"))?;
    let key = VerifyingKey::recover_from_prehash(&eip191_digest(&message), &signature, recovery)
        .map_err(|_| refused("no public key recovers from this signature"))?;

    Ok(address_of(&key))
}

/// EIP-191 `personal_sign`: keccak256 over the prefix, the decimal byte
/// length, and the message.
fn eip191_digest(message: &str) -> [u8; 32] {
    let mut hasher = Keccak256::new();
    hasher.update(b"\x19Ethereum Signed Message:\n");
    hasher.update(message.len().to_string().as_bytes());
    hasher.update(message.as_bytes());
    hasher.finalize().into()
}

/// The last 20 bytes of keccak256 over the uncompressed public key, without
/// its `0x04` tag — the Ethereum address derivation, which Base shares.
fn address_of(key: &VerifyingKey) -> ProvedAddress {
    let point = key.to_encoded_point(false);
    let digest = Keccak256::digest(&point.as_bytes()[1..]);
    let mut address = String::with_capacity(42);
    address.push_str("0x");
    for byte in &digest[12..] {
        address.push_str(&format!("{byte:02x}"));
    }
    ProvedAddress(address)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let text = text.strip_prefix("0x").unwrap_or(text);
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// `YYYY-MM-DDTHH:MM:SSZ` to a Unix second.
///
/// Deliberately strict and deliberately tiny: this crate has no date library,
/// and the only shape that reaches it is the one the pool itself wrote into
/// the text it asked a member to sign. A fractional second or an offset is
/// refused rather than normalised, because two spellings of one instant would
/// be two different signed strings.
fn parse_rfc3339_seconds(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() != 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    if bytes[13] != b':' || bytes[16] != b':' || bytes[19] != b'Z' {
        return None;
    }
    let field = |from: usize, to: usize| text.get(from..to)?.parse::<i64>().ok();
    let (year, month, day) = (field(0, 4)?, field(5, 7)?, field(8, 10)?);
    let (hour, minute, second) = (field(11, 13)?, field(14, 16)?, field(17, 19)?);
    if !(1..=12).contains(&month) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    // Against the real calendar, not against 31. The algorithm below is
    // `civil_from_days` run backwards, and it is *total*: handed 31 February
    // it returns a perfectly good instant three days into March. Normalising
    // an impossible date into a real one is what `member_protocol.md` §14
    // refuses — "an unknown enum, missing required field, lossy integer,
    // invalid digest, or schema mismatch is incompatible input, not a value
    // to coerce" — and here it would also mean two spellings of one instant,
    // when the point of this text is that there is exactly one.
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        // February, which is the whole reason this is a match and not a
        // constant: 2000 has 29 days and 2100 has 28.
        _ => {
            if leap {
                29
            } else {
                28
            }
        }
    };
    if !(1..=days_in_month).contains(&day) {
        return None;
    }

    // Days since the Unix epoch, by the civil-from-days algorithm (Howard
    // Hinnant's), which is exact for the proleptic Gregorian calendar.
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;

    u64::try_from(days * 86_400 + hour * 3_600 + minute * 60 + second).ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_expiry_parser_agrees_with_the_calendar() {
        // Hand-rolled, because this crate has no date library — so it is
        // checked against instants computed elsewhere rather than against
        // itself. The leap years are the cases a wrong algorithm gets wrong:
        // 2000 is a leap year (divisible by 400) and 2100 is not (divisible
        // by 100 but not 400), and an implementation that mishandles either
        // is off by a day from that point on.
        for (text, expected) in [
            ("1970-01-01T00:00:00Z", 0),
            ("2000-02-29T12:00:00Z", 951_825_600),
            ("2024-02-29T23:59:59Z", 1_709_251_199),
            ("2026-03-20T09:51:40Z", 1_774_000_300),
            ("2038-01-19T03:14:07Z", 2_147_483_647),
            ("2100-03-01T00:00:00Z", 4_107_542_400),
            // The last day of each month length, so the bound above is a
            // calendar and not a blanket refusal of anything past the 28th.
            ("2026-01-31T00:00:00Z", 1_769_817_600),
            ("2026-02-28T00:00:00Z", 1_772_236_800),
            ("2026-04-30T00:00:00Z", 1_777_507_200),
            ("2000-02-29T00:00:00Z", 951_782_400),
        ] {
            assert_eq!(parse_rfc3339_seconds(text), Some(expected), "{text}");
        }
    }

    #[test]
    fn the_expiry_parser_refuses_what_the_pool_would_not_write() {
        for bad in [
            "",
            "1970-01-01T00:00:00",
            "1970-01-01T00:00:00.000Z",
            "1970-01-01T00:00:00+00:00",
            "1970-01-01 00:00:00Z",
            "1970-13-01T00:00:00Z",
            "1970-00-01T00:00:00Z",
            "1970-01-32T00:00:00Z",
            // Days that do not exist, each of which the civil-from-days
            // algorithm would happily turn into a real date in the next
            // month.
            "2026-02-29T00:00:00Z",
            "2026-02-31T00:00:00Z",
            "2024-02-30T00:00:00Z",
            "2100-02-29T00:00:00Z",
            "2026-04-31T00:00:00Z",
            "2026-06-31T00:00:00Z",
            "2026-09-31T00:00:00Z",
            "2026-11-31T00:00:00Z",
            "2026-01-00T00:00:00Z",
            "1970-01-01T24:00:00Z",
            "1970-01-01T00:60:00Z",
            "1970-01-01T00:00:60Z",
            "19700-1-01T00:00:00Z",
            // Before the epoch: a `u64` second cannot hold it, and a pool
            // expiry in 1969 is not a value to salvage.
            "1969-12-31T23:59:59Z",
        ] {
            assert_eq!(parse_rfc3339_seconds(bad), None, "{bad:?}");
        }
    }
}
