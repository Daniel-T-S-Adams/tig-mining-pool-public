//! The §6.3 block-derived challenge-tie draw (`mining_system.md`, ADR 0005).
//!
//! The draw exists so a tie between challenges is reproducible: an auditor
//! re-derives it from the persisted decision record alone. Its inputs are the
//! decision's anchor snapshot block and the challenge ids, and nothing else —
//! deliberately, because a seed carrying any per-decision input would give the
//! pool a re-roll.
//!
//! This lives in `pool-domain` rather than in the decision engine because the
//! *controller* derives the ranks and supplies them as explicit input
//! (`architecture.md` §3, §5.1 step 4). The engine never derives randomness,
//! so it must not be able to.

use crate::Network;

/// The versioned domain string. Persisted with the decision record.
pub const CHALLENGE_TIE_DOMAIN: &str = "tig-pool-challenge-tie-v1";

/// One challenge's draw rank.
///
/// Ordered as a 32-byte unsigned big-endian integer, which for a byte array is
/// the derived lexicographic order — the smallest rank wins its tie.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DrawRank([u8; 32]);

impl DrawRank {
    /// Wraps 32 already-derived bytes.
    ///
    /// Public so a persisted rank map can be read back for audit, and so a
    /// test can stand a small ordered value in for a derived rank.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The 64-character lowercase hex form, as the decision record stores it.
    ///
    /// One rendering, because there were three: the recorder, the audit
    /// comparison and the tests each wrote the same `{b:02x}` fold, and a
    /// rank spelled two ways is a rank that does not compare equal to itself.
    /// Two digits per byte always — `{:x}` would drop the leading zero of a
    /// byte below 0x10 and shorten the value.
    pub fn to_hex(&self) -> String {
        let mut out = String::with_capacity(64);
        for b in self.0 {
            out.push_str(&format!("{b:02x}"));
        }
        out
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// `challenge_tie_seed = BLAKE3(utf8(domain \n network \n block_id))`.
///
/// The three inputs are newline-free ASCII, so the newline-joined encoding is
/// unambiguous: `network` is `testnet` or `mainnet` and `block_id` is a TIG
/// block id.
pub fn challenge_tie_seed(network: Network, block_id: &str) -> [u8; 32] {
    let preimage = format!("{CHALLENGE_TIE_DOMAIN}\n{}\n{block_id}", network.as_str());
    *blake3::hash(preimage.as_bytes()).as_bytes()
}

/// `draw_rank[c] = BLAKE3(challenge_tie_seed || utf8("\n" || challenge_id))`.
///
/// The seed enters as its raw 32 bytes, not as text.
pub fn draw_rank(seed: &[u8; 32], challenge_id: &str) -> DrawRank {
    let mut hasher = blake3::Hasher::new();
    hasher.update(seed);
    hasher.update(b"\n");
    hasher.update(challenge_id.as_bytes());
    DrawRank(*hasher.finalize().as_bytes())
}

/// The complete rank map for one decision.
///
/// §6.3 requires a rank for **every** compute-compatible eligible challenge,
/// not only the tied ones: the full map is the audit evidence, and which
/// challenges were eligible at that instant is not recoverable later.
pub fn draw_ranks<'a>(
    network: Network,
    block_id: &str,
    challenge_ids: impl IntoIterator<Item = &'a str>,
) -> std::collections::BTreeMap<String, DrawRank> {
    let seed = challenge_tie_seed(network, block_id);
    challenge_ids
        .into_iter()
        .map(|id| (id.to_string(), draw_rank(&seed, id)))
        .collect()
}
