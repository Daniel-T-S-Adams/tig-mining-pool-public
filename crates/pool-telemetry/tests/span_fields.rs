//! Proves the mechanism `init` relies on: that `service`, `deployment` and
//! `network` survive being emitted from inside a *nested* span.
//!
//! `with_current_span(true)` alone serialises only the innermost span, so a
//! nested span would hide the root's fields and every line inside it would
//! violate `architecture.md` §10.1. This asserts the configuration actually
//! prevents that, rather than trusting the reasoning.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io;
use std::sync::{Arc, Mutex};

/// Collects formatted output so the test can inspect it.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn root_span_fields_survive_nesting() {
    let capture = Capture::default();

    // The same formatter configuration `pool_telemetry::init` installs.
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(true)
        .with_writer(capture.clone())
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        let root = tracing::info_span!(
            "service",
            service = "pool-admin migrate",
            deployment = "test",
            network = "testnet",
        );
        let _root = root.enter();

        // A nested span with none of the three fields — the shape that would
        // hide them if only the innermost span were serialised.
        let inner = tracing::info_span!("snapshot", block_id = "abc123");
        let _inner = inner.enter();

        tracing::info!(event = "inside.nested.span", "emitted from a nested span");
    });

    let output = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let line = output
        .lines()
        .find(|l| l.contains("inside.nested.span"))
        .unwrap_or_else(|| panic!("event not captured; output was: {output}"));
    let value: serde_json::Value = serde_json::from_str(line).unwrap();

    let mut scopes: Vec<&serde_json::Value> = Vec::new();
    if let Some(current) = value.get("span") {
        scopes.push(current);
    }
    if let Some(list) = value.get("spans").and_then(|s| s.as_array()) {
        scopes.extend(list.iter());
    }

    for field in ["service", "deployment", "network"] {
        assert!(
            scopes.iter().any(|s| s.get(field).is_some()),
            "{field} was lost inside a nested span (architecture.md §10.1): {line}"
        );
    }

    // And the nested span's own context is still there.
    assert!(
        scopes.iter().any(|s| s.get("block_id").is_some()),
        "the nested span's own fields must also survive: {line}"
    );
}
