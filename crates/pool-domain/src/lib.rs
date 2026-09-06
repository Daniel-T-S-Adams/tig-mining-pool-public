//! Shared domain types for the TIG mining pool.
//!
//! Grows one type at a time as implemented behavior requires it
//! (docs/pre_build_checklist.md: no speculative scaffolding).

use std::fmt;
use std::str::FromStr;

pub mod challenge_tie;

pub use challenge_tie::{
    CHALLENGE_TIE_DOMAIN, DrawRank, challenge_tie_seed, draw_rank, draw_ranks,
};

/// The TIG network a process operates against.
///
/// Deliberately has no `Default` impl: docs/architecture.md §9 requires that
/// network has no production default — every binary must be told its network
/// explicitly and fail closed otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Network {
    Testnet,
    Mainnet,
}

impl Network {
    pub fn as_str(self) -> &'static str {
        match self {
            Network::Testnet => "testnet",
            Network::Mainnet => "mainnet",
        }
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error returned when parsing an unrecognized network name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownNetwork(pub String);

impl fmt::Display for UnknownNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "unknown network {:?}; expected \"testnet\" or \"mainnet\"",
            self.0
        )
    }
}

impl std::error::Error for UnknownNetwork {}

impl FromStr for Network {
    type Err = UnknownNetwork;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "testnet" => Ok(Network::Testnet),
            "mainnet" => Ok(Network::Mainnet),
            other => Err(UnknownNetwork(other.to_owned())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_networks() {
        assert_eq!("testnet".parse::<Network>(), Ok(Network::Testnet));
        assert_eq!("mainnet".parse::<Network>(), Ok(Network::Mainnet));
    }

    #[test]
    fn rejects_unknown_and_non_canonical_names() {
        for bad in ["", "Testnet", "MAINNET", "prod", "local"] {
            assert!(bad.parse::<Network>().is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn display_round_trips() {
        for net in [Network::Testnet, Network::Mainnet] {
            assert_eq!(net.to_string().parse::<Network>(), Ok(net));
        }
    }
}
