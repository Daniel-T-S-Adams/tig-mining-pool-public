//! Proves the `mining_system.md` §6.3 challenge-tie worked example
//! (ADR 0005) against the shipped derivation, not against a copy of it.
//!
//! The helpers this file used to carry were a second implementation of the
//! rule; a typo in the real one would have left this vector green.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use pool_domain::{DrawRank, Network, challenge_tie_seed, draw_rank, draw_ranks};

#[test]
fn section_6_3_worked_example_vector() {
    // fixtures/tig/v1/get-block.json anchor: testnet block_100080 with
    // active challenges c001 and c003.
    let seed = challenge_tie_seed(Network::Testnet, "block_100080");
    assert_eq!(
        hex(&seed),
        "2fa3fc89f50afd1b38d2209af931ae2e1e3d49ab555292f0cb5aa07a6d017c32"
    );

    let rank_c001 = draw_rank(&seed, "c001");
    let rank_c003 = draw_rank(&seed, "c003");
    assert_eq!(
        hex(rank_c001.as_bytes()),
        "5d0f453f4d0c62aa01a37dcdad7ab31caeb7f14f05c32e321d1125ca7d115eb2"
    );
    assert_eq!(
        hex(rank_c003.as_bytes()),
        "f16ab636834eae6322acd3d00286a23830bcc8a0f24cc23d1c2e06bce784b41c"
    );

    // Smallest rank wins under 32-byte big-endian (byte-order) comparison:
    // c001 is selected.
    assert!(rank_c001 < rank_c003);
}

#[test]
fn the_rank_map_covers_every_supplied_challenge() {
    // §6.3 requires a rank for every compute-compatible eligible challenge,
    // not only the tied ones: the full map is the audit evidence, and which
    // challenges were eligible at that instant is not recoverable later.
    let ranks = draw_ranks(Network::Testnet, "block_100080", ["c001", "c002", "c003"]);
    assert_eq!(
        ranks.keys().collect::<Vec<_>>(),
        vec!["c001", "c002", "c003"]
    );
    let seed = challenge_tie_seed(Network::Testnet, "block_100080");
    assert_eq!(ranks["c002"], draw_rank(&seed, "c002"));
}

#[test]
fn the_network_is_part_of_the_seed() {
    // Otherwise a testnet draw would predict the mainnet one for the same
    // block id, and the seed's whole purpose is that nobody can choose it.
    assert_ne!(
        challenge_tie_seed(Network::Testnet, "block_100080"),
        challenge_tie_seed(Network::Mainnet, "block_100080")
    );
}

#[test]
fn the_seed_enters_the_rank_as_bytes_not_as_text() {
    // §6.3: "challenge_tie_seed enters the second hash as its raw 32 bytes."
    // Hashing its hex rendering instead would produce a different, equally
    // deterministic map — and the vector above is what catches that.
    let seed = challenge_tie_seed(Network::Testnet, "block_100080");
    let mut hasher = blake3::Hasher::new();
    hasher.update(hex(&seed).as_bytes());
    hasher.update(b"\nc001");
    assert_ne!(
        draw_rank(&seed, "c001"),
        DrawRank::from_bytes(*hasher.finalize().as_bytes())
    );
}

#[test]
fn ranks_order_as_big_endian_integers() {
    let small = DrawRank::from_bytes([0x00; 32]);
    let mut bytes = [0x00; 32];
    bytes[0] = 0x01;
    let large = DrawRank::from_bytes(bytes);
    assert!(small < large, "the leading byte dominates the comparison");
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
