//! §6.7 bundle counts, chosen per track because TIG picks the final track
//! during precommit processing.

use std::collections::BTreeMap;

use crate::challenge::OfferedCompute;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackSizing {
    pub num_nonces_per_bundle: u64,
    /// From §6.5. It changes only the CPU rule.
    pub selected_algorithm_best_on_track: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleSizingInput {
    pub offered_compute: OfferedCompute,
    pub min_num_bundles: u64,
    pub tracks: BTreeMap<String, TrackSizing>,
}

/// The alignment derivation, kept so the decision record can show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Derivation {
    pub gcd_cores_nonces: u64,
    pub bundle_multiple: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundleSizing {
    pub num_bundles: BTreeMap<String, u64>,
    /// Present only for tracks that took the CPU alignment branch.
    pub derivation: BTreeMap<String, Derivation>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BundleSizingError {
    #[error("a CPU offer with zero cores cannot align a bundle count")]
    ZeroCores,
    #[error("bundle count overflowed for track {track_id}")]
    Overflow { track_id: String },
}

/// §6.7. GPU takes the minimum; so does a CPU member whose selected algorithm
/// is not best on the track. Otherwise the count is the smallest multiple of
/// `cores / gcd(cores, nonces)` at or above the minimum, which is exactly the
/// smallest count at or above the minimum for which
/// `(num_bundles * num_nonces_per_bundle) mod cores == 0`.
pub fn size_bundles(input: &BundleSizingInput) -> Result<BundleSizing, BundleSizingError> {
    let mut num_bundles = BTreeMap::new();
    let mut derivation = BTreeMap::new();

    for (track_id, track) in &input.tracks {
        let cores = match (
            &input.offered_compute,
            track.selected_algorithm_best_on_track,
        ) {
            (OfferedCompute::Gpu, _) | (OfferedCompute::Cpu { .. }, false) => {
                num_bundles.insert(track_id.clone(), input.min_num_bundles);
                continue;
            }
            (OfferedCompute::Cpu { cores }, true) => *cores,
        };
        if cores == 0 {
            return Err(BundleSizingError::ZeroCores);
        }

        let divisor = gcd(cores, track.num_nonces_per_bundle);
        let bundle_multiple = cores / divisor;
        // ceil(m / bundle_multiple) * bundle_multiple, without floating point
        // and without a guard: §6.7 is the formula, and a `.max(1)` would
        // make `min_num_bundles = 0` return `bundle_multiple` here while the
        // GPU and not-best branches returned 0 for the same input.
        let steps = input.min_num_bundles.div_ceil(bundle_multiple);
        let sized =
            steps
                .checked_mul(bundle_multiple)
                .ok_or_else(|| BundleSizingError::Overflow {
                    track_id: track_id.clone(),
                })?;

        derivation.insert(
            track_id.clone(),
            Derivation {
                gcd_cores_nonces: divisor,
                bundle_multiple,
            },
        );
        num_bundles.insert(track_id.clone(), sized);
    }

    Ok(BundleSizing {
        num_bundles,
        derivation,
    })
}

fn gcd(a: u64, b: u64) -> u64 {
    let (mut a, mut b) = (a, b);
    while b != 0 {
        let remainder = a % b;
        a = b;
        b = remainder;
    }
    a
}
