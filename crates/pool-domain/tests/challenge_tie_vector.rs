//! Proves the `mining_system.md` §6.3 challenge-tie worked example
//! (ADR 0005): the block-derived draw for the `fixtures/tig/v1` anchor
//! block with its two active challenges tied.

/// `challenge_tie_seed = BLAKE3(utf8(domain \n network \n block_id))`.
fn challenge_tie_seed(network: &str, block_id: &str) -> [u8; 32] {
    let preimage = format!("tig-pool-challenge-tie-v1\n{network}\n{block_id}");
    *blake3::hash(preimage.as_bytes()).as_bytes()
}

/// `draw_rank[c] = BLAKE3(seed_bytes || utf8(\n challenge_id))`.
fn draw_rank(seed: &[u8; 32], challenge_id: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(seed);
    hasher.update(format!("\n{challenge_id}").as_bytes());
    *hasher.finalize().as_bytes()
}

#[test]
fn section_6_3_worked_example_vector() {
    // fixtures/tig/v1/get-block.json anchor: testnet block_100080 with
    // active challenges c001 and c003.
    let seed = challenge_tie_seed("testnet", "block_100080");
    assert_eq!(
        hex(&seed),
        "2fa3fc89f50afd1b38d2209af931ae2e1e3d49ab555292f0cb5aa07a6d017c32"
    );

    let rank_c001 = draw_rank(&seed, "c001");
    let rank_c003 = draw_rank(&seed, "c003");
    assert_eq!(
        hex(&rank_c001),
        "5d0f453f4d0c62aa01a37dcdad7ab31caeb7f14f05c32e321d1125ca7d115eb2"
    );
    assert_eq!(
        hex(&rank_c003),
        "f16ab636834eae6322acd3d00286a23830bcc8a0f24cc23d1c2e06bce784b41c"
    );

    // Smallest rank wins under 32-byte big-endian (byte-order) comparison:
    // c001 is selected.
    assert!(rank_c001 < rank_c003);
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
