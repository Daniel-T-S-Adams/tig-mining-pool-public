//! The §11 POST-lane limits, as loaded from `config/tig_integration.json`.
//!
//! The read side of §11 lives in `tig-client` and is deliberately separate:
//! that crate performs reads only, and the API key never enters it. These are
//! the write-side values, and they belong with the one component that holds
//! the credential.
//!
//! Like the read limits, there is no compiled fallback. §12 forbids a live
//! protocol value becoming a constant in the binary, and a default would be
//! reached exactly when configuration was missing.

use std::time::Duration;

use serde::Deserialize;

/// §11's POST-lane policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WritePolicy {
    connect_timeout: Duration,
    /// Budget for a whole write, not one attempt: §11 pins a "POST total
    /// timeout" and the write path makes at most one attempt per call, so
    /// the two are the same bound here.
    call_timeout: Duration,
    min_between_initial_writes: Duration,
    retry_interval_after_reconciliation: Duration,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum WritePolicyError {
    #[error("cannot parse the TIG integration config: {0}")]
    Unparseable(String),
    #[error("{path} is missing from the TIG integration config")]
    Missing { path: String },
    /// `write_limits` was present but not a complete POST-lane policy.
    #[error("write_limits is not a complete POST-lane policy: {reason}")]
    Malformed { reason: String },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    connect_timeout_seconds: u64,
    call_timeout_seconds: u64,
    min_seconds_between_initial_writes: u64,
    retry_interval_after_reconciliation_seconds: u64,
}

impl WritePolicy {
    pub fn from_config_json(json: &str) -> Result<Self, WritePolicyError> {
        let root: serde_json::Value =
            serde_json::from_str(json).map_err(|e| WritePolicyError::Unparseable(e.to_string()))?;
        let section = root
            .get("write_limits")
            .ok_or_else(|| WritePolicyError::Missing {
                path: "write_limits".to_string(),
            })?;
        let wire: Wire =
            serde_json::from_value(section.clone()).map_err(|e| WritePolicyError::Malformed {
                reason: e.to_string(),
            })?;
        Ok(Self {
            connect_timeout: Duration::from_secs(wire.connect_timeout_seconds),
            call_timeout: Duration::from_secs(wire.call_timeout_seconds),
            min_between_initial_writes: Duration::from_secs(
                wire.min_seconds_between_initial_writes,
            ),
            retry_interval_after_reconciliation: Duration::from_secs(
                wire.retry_interval_after_reconciliation_seconds,
            ),
        })
    }

    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    pub fn call_timeout(&self) -> Duration {
        self.call_timeout
    }

    /// §11: the POST lane is serialized with a minimum gap between initial
    /// writes.
    pub fn min_between_initial_writes(&self) -> Duration {
        self.min_between_initial_writes
    }

    /// §11: how long to wait before a write is retried, once reconciliation
    /// has established it is safe to retry at all.
    ///
    /// Not a backoff. Nothing in this crate retries a write on a timer;
    /// §10 requires confirmed state to be queried first, so this bounds how
    /// often that whole cycle may repeat.
    pub fn retry_interval_after_reconciliation(&self) -> Duration {
        self.retry_interval_after_reconciliation
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const SHIPPED: &str = include_str!("../../../config/tig_integration.json");

    #[test]
    fn the_shipped_config_carries_the_pinned_post_lane() {
        let policy = WritePolicy::from_config_json(SHIPPED).expect("shipped config parses");
        assert_eq!(policy.connect_timeout(), Duration::from_secs(5));
        assert_eq!(policy.call_timeout(), Duration::from_secs(60));
        assert_eq!(policy.min_between_initial_writes(), Duration::from_secs(5));
        assert_eq!(
            policy.retry_interval_after_reconciliation(),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn a_config_without_write_limits_is_refused() {
        let mut json: serde_json::Value = serde_json::from_str(SHIPPED).unwrap();
        json.as_object_mut().unwrap().remove("write_limits");
        let err = WritePolicy::from_config_json(&json.to_string())
            .expect_err("no compiled fallback exists");
        assert!(format!("{err}").contains("write_limits"), "got: {err}");
    }

    #[test]
    fn a_misspelt_or_absent_limit_is_refused() {
        // `deny_unknown_fields` plus required fields: a typo would otherwise
        // be dropped and the policy would carry a value nobody chose.
        for mutate in [
            |o: &mut serde_json::Map<String, serde_json::Value>| {
                o.remove("call_timeout_seconds");
            },
            |o: &mut serde_json::Map<String, serde_json::Value>| {
                o.insert("call_timeout_secs".to_string(), serde_json::json!(60));
            },
        ] {
            let mut json: serde_json::Value = serde_json::from_str(SHIPPED).unwrap();
            mutate(json["write_limits"].as_object_mut().unwrap());
            assert!(WritePolicy::from_config_json(&json.to_string()).is_err());
        }
    }
}
