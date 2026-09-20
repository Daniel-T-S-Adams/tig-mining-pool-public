//! Slice-2 criterion B8: the member account is a Base address, proved by a
//! signature (ADR 0011, `accounting.md` §12.2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use k256::ecdsa::{RecoveryId, Signature, SigningKey};
use pool_identity::wallet::{
    LoginExpectation, LoginPurpose, ProvedAddress, login_signing_string, recover_login_address,
};
use sha3::{Digest as _, Keccak256};

const POOL_DOMAIN: &str = "bench-pool.example";
/// Base mainnet. A signature for another chain is for another world.
const BASE_CHAIN_ID: u64 = 8453;
const NONCE: &str = "3f1a9c0e5b2d47a8bc6f91e0d3247a5b";
const EXPIRES: &str = "2026-03-20T09:51:40Z";
/// Five minutes before `EXPIRES`.
const NOW: u64 = 1_774_000_000;

/// TEST-ONLY, and deliberately the most widely published secp256k1 key there
/// is: the scalar 1. Its address appears in every "weak key" list and in
/// countless test fixtures, so deriving it here checks this crate against the
/// rest of the world rather than against itself.
///
/// The scalar 2 is included below for the same reason — one matching pair
/// could be a coincidence of a wrong constant, two cannot.
const KNOWN_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const KNOWN_ADDRESS: &str = "0x7e5f4552091a69125d5dfcb7b8c2659029395bdf";
const SECOND_KEY: &str = "0000000000000000000000000000000000000000000000000000000000000002";
const SECOND_ADDRESS: &str = "0x2b5ad5c4795c026514f8317c7a215e218dccd6cf";

fn key(hex: &str) -> SigningKey {
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect();
    SigningKey::from_slice(&bytes).unwrap()
}

/// Sign `message` the way a wallet's `personal_sign` does, returning
/// `r || s || v` as hex with `v` in {27, 28}.
fn personal_sign(signing_key: &SigningKey, message: &str) -> String {
    let mut hasher = Keccak256::new();
    hasher.update(b"\x19Ethereum Signed Message:\n");
    hasher.update(message.len().to_string().as_bytes());
    hasher.update(message.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();

    let (signature, recovery): (Signature, RecoveryId) =
        signing_key.sign_prehash_recoverable(&digest).unwrap();
    let mut out = String::from("0x");
    for byte in signature.to_bytes() {
        out.push_str(&format!("{byte:02x}"));
    }
    out.push_str(&format!("{:02x}", recovery.to_byte() + 27));
    out
}

/// Whether a recovery proves control of `address`.
///
/// Recovery almost always *succeeds* — ECDSA recovers some public key from
/// nearly any well-formed signature — so a signature over different terms
/// does not produce an error, it produces a **different address**. That is the
/// whole security argument: the pool looks a member up by the address the
/// signature recovers to, and reaching a particular member's row means
/// forging ECDSA for that address.
///
/// Tests below therefore assert "does not prove this member" rather than "is
/// refused", which is the true statement.
fn proves(result: Result<ProvedAddress, pool_identity::IdentityError>, address: &str) -> bool {
    matches!(result, Ok(proved) if proved.as_str() == address)
}

fn expectation() -> LoginExpectation<'static> {
    LoginExpectation {
        pool_domain: POOL_DOMAIN,
        chain_id: BASE_CHAIN_ID,
        purpose: LoginPurpose::WorkerEnrollment,
        nonce: NONCE,
        expires_at_rfc3339: EXPIRES,
    }
}

fn signed_for(expectation: &LoginExpectation<'_>, signing_key: &SigningKey) -> String {
    personal_sign(
        signing_key,
        &login_signing_string(
            expectation.pool_domain,
            expectation.chain_id,
            expectation.purpose,
            expectation.nonce,
            expectation.expires_at_rfc3339,
        ),
    )
}

#[test]
fn the_address_derivation_agrees_with_published_keys_and_addresses() {
    // The one step here that could be subtly wrong and still self-consistent:
    // `Keccak256` rather than `Sha3_256`, the uncompressed point with its
    // `0x04` tag dropped, the last 20 bytes. Any of those wrong gives a
    // perfectly stable address that no wallet would agree with.
    //
    // Two published pairs, because the first version of this test used a
    // key/address pair recalled from memory and the pairing was wrong — the
    // code was right and the test said otherwise. These are checkable against
    // any Ethereum tool.
    for (hex, address) in [(KNOWN_KEY, KNOWN_ADDRESS), (SECOND_KEY, SECOND_ADDRESS)] {
        let signature = signed_for(&expectation(), &key(hex));
        let proved =
            recover_login_address(&expectation(), &signature, NOW).expect("a valid signature");
        assert_eq!(proved.as_str(), address, "for key {hex}");
    }
}

#[test]
fn a_valid_signature_authenticates_exactly_the_address_that_signed_it() {
    let alice = key(KNOWN_KEY);
    let bob = key(SECOND_KEY);

    let from_alice =
        recover_login_address(&expectation(), &signed_for(&expectation(), &alice), NOW)
            .expect("alice");
    let from_bob =
        recover_login_address(&expectation(), &signed_for(&expectation(), &bob), NOW).expect("bob");

    assert_ne!(from_alice, from_bob);
    assert_eq!(from_alice.as_str(), KNOWN_ADDRESS);
    // Lowercase `0x` hex, which is the only spelling `pool.member` accepts:
    // its `wallet_address` check refuses a mixed-case address rather than
    // folding it, so two spellings of one account cannot both be inserted.
    assert!(
        from_bob
            .as_str()
            .strip_prefix("0x")
            .is_some_and(|hex| hex.len() == 40
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
        "{}",
        from_bob.as_str()
    );
}

#[test]
fn a_signature_for_another_pool_or_another_chain_is_refused() {
    // §12.2: "exact-domain validation to prevent signature reuse on a
    // different pool or chain". The member really did sign each of these —
    // what they did not sign is *this* pool's text.
    let signing_key = key(KNOWN_KEY);

    let elsewhere = LoginExpectation {
        pool_domain: "other-pool.example",
        ..expectation()
    };
    let signature = signed_for(&elsewhere, &signing_key);
    assert!(
        !proves(
            recover_login_address(&expectation(), &signature, NOW),
            KNOWN_ADDRESS
        ),
        "another pool's signature must not prove this member"
    );

    // A domain this one merely ends with. Suffix matching would accept it.
    let lookalike = LoginExpectation {
        pool_domain: "attacker.test.bench-pool.example",
        ..expectation()
    };
    assert!(
        !proves(
            recover_login_address(&expectation(), &signed_for(&lookalike, &signing_key), NOW),
            KNOWN_ADDRESS
        ),
        "a lookalike domain must not prove this member"
    );

    let other_chain = LoginExpectation {
        chain_id: 1,
        ..expectation()
    };
    assert!(
        !proves(
            recover_login_address(&expectation(), &signed_for(&other_chain, &signing_key), NOW),
            KNOWN_ADDRESS
        ),
        "another chain's signature must not prove this member"
    );
}

#[test]
fn a_signature_for_another_purpose_or_nonce_is_refused() {
    // A member who authorised an enrollment ticket has not authorised a
    // worker recovery, and a nonce is what makes one signature one use.
    let signing_key = key(KNOWN_KEY);

    for different in [
        LoginExpectation {
            purpose: LoginPurpose::WorkerRecovery,
            ..expectation()
        },
        LoginExpectation {
            nonce: "00000000000000000000000000000000",
            ..expectation()
        },
    ] {
        assert!(
            !proves(
                recover_login_address(&expectation(), &signed_for(&different, &signing_key), NOW),
                KNOWN_ADDRESS
            ),
            "a signature over different terms must not prove this member"
        );
    }
}

#[test]
fn an_expired_signature_is_refused_and_the_edge_is_not() {
    let signing_key = key(KNOWN_KEY);
    let signature = signed_for(&expectation(), &signing_key);
    let expires_at = 1_774_000_300;

    // The last second it is good for.
    recover_login_address(&expectation(), &signature, expires_at - 1)
        .expect("one second before expiry");

    // And the first it is not. `>=`, because an expiry is the instant it
    // stops being valid rather than the last instant it is.
    for at in [expires_at, expires_at + 1, expires_at + 86_400] {
        assert!(
            recover_login_address(&expectation(), &signature, at).is_err(),
            "expired at {at}"
        );
    }
}

#[test]
fn a_signature_cannot_be_replayed_with_a_later_expiry() {
    // The expiry is in the signed text, so extending one means signing again.
    // Without it there, a signature obtained once would be good for as long
    // as whoever presented it cared to claim.
    let signing_key = key(KNOWN_KEY);
    let signed_for_soon = signed_for(&expectation(), &signing_key);

    let much_later = LoginExpectation {
        expires_at_rfc3339: "2030-01-01T00:00:00Z",
        ..expectation()
    };
    assert!(
        !proves(
            recover_login_address(&much_later, &signed_for_soon, NOW),
            KNOWN_ADDRESS
        ),
        "a signature must not carry an expiry it did not sign"
    );

    // And the member can of course sign the later one themselves.
    assert!(
        proves(
            recover_login_address(&much_later, &signed_for(&much_later, &signing_key), NOW),
            KNOWN_ADDRESS
        ),
        "the later expiry, actually signed, proves the member"
    );
}

#[test]
fn a_malformed_signature_is_refused_rather_than_guessed_at() {
    let signing_key = key(KNOWN_KEY);
    let good = signed_for(&expectation(), &signing_key);

    for (name, signature) in [
        ("empty", String::new()),
        ("not hex", "0xzz".to_owned() + &good[4..]),
        ("odd length", good[..good.len() - 1].to_owned()),
        ("too short", good[..60].to_owned()),
        ("too long", good.clone() + "00"),
        // Anything outside {0, 1, 27, 28}. Recovery ids 2 and 3 exist in
        // ECDSA for the case where `r` overflows the curve order, and no
        // Ethereum wallet emits them; accepting one would recover a different
        // key from the same bytes.
        ("recovery id 2", good[..good.len() - 2].to_owned() + "02"),
        ("recovery id 3", good[..good.len() - 2].to_owned() + "03"),
        ("recovery id 29", good[..good.len() - 2].to_owned() + "1d"),
        ("recovery id 7", good[..good.len() - 2].to_owned() + "07"),
    ] {
        assert!(
            recover_login_address(&expectation(), &signature, NOW).is_err(),
            "{name} must be refused"
        );
    }

    // The unmodified one still works, so the rejections are about what
    // changed.
    recover_login_address(&expectation(), &good, NOW).expect("the original");
}

#[test]
fn both_spellings_of_the_recovery_byte_are_accepted() {
    // Wallets emit `v` as 27 or 28; some libraries emit 0 or 1. They are the
    // same recovery id, and refusing one would reject a correctly signed
    // member for their library's convention.
    let signing_key = key(KNOWN_KEY);
    let good = signed_for(&expectation(), &signing_key);

    let v = u8::from_str_radix(&good[good.len() - 2..], 16).unwrap();
    assert!(v == 27 || v == 28, "the fixture signs with 27 or 28");
    let lowered = format!("{}{:02x}", &good[..good.len() - 2], v - 27);

    let from_high = recover_login_address(&expectation(), &good, NOW).expect("27/28");
    let from_low = recover_login_address(&expectation(), &lowered, NOW).expect("0/1");
    assert_eq!(from_high, from_low);
    assert_eq!(from_high.as_str(), KNOWN_ADDRESS);
}

#[test]
fn an_expiry_that_is_not_the_shape_the_pool_wrote_is_refused() {
    // The expiry is parsed from the same text that was signed, so a caller
    // cannot present one expiry and sign another. A shape the pool would
    // never write is refused rather than normalised: two spellings of one
    // instant are two different signed strings.
    let signing_key = key(KNOWN_KEY);
    for bad in [
        "",
        "2026-03-20T09:51:40.000Z",
        "2026-03-20T09:51:40+00:00",
        "2026-03-20 09:51:40Z",
        "2026-13-20T09:51:40Z",
        "2026-03-20T24:51:40Z",
        "not-a-time",
    ] {
        let expectation = LoginExpectation {
            expires_at_rfc3339: bad,
            ..expectation()
        };
        let signature = signed_for(&expectation, &signing_key);
        assert!(
            recover_login_address(&expectation, &signature, NOW).is_err(),
            "expiry {bad:?} must be refused"
        );
    }
}
