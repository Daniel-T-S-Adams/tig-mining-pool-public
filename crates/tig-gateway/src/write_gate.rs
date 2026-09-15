//! The runtime side of `WRITE_READY` (slice-1 criterion B3).
//!
//! [`readiness`](crate::readiness) decides whether writes may be enabled at
//! all. This holds that decision while the process runs, and takes it away
//! when TIG stops accepting the credential or the schema stops matching —
//! the two causes `architecture.md` §10.3 pages on ("TIG writes are disabled
//! by compatibility or authentication failure").
//!
//! The rule B3 states is narrow and easy to get backwards: losing
//! `WRITE_READY` blocks **new** writes *without corrupting in-flight
//! intents*. So revocation is not a cancellation. A write that was already
//! admitted holds a [`WritePermit`] and keeps it: `tig_integration.md` §7
//! advances a workflow from confirmed evidence rather than from a local
//! decision, so abandoning a request already sent to TIG would not undo it —
//! it would only lose track of an intent whose outcome is now unknown, which
//! is the ambiguity §10 exists to avoid creating.

use std::sync::{Arc, Mutex, PoisonError};

use crate::readiness::{Check, WriteReady};

/// Why `WRITE_READY` was lost.
///
/// The two categories `architecture.md` §10.3 alerts on, kept apart because
/// they call for different operator action: a credential is replaced, an
/// incompatibility is an operator task under §13 (compare the pinned
/// upstream, update models and fixtures, re-run the spike).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Revocation {
    /// TIG refused the credential.
    Authentication { detail: String },
    /// A §13 check that passed at startup no longer does.
    Compatibility { failed: Vec<Check>, detail: String },
}

impl Revocation {
    /// The §10.3 category, for the alert and for an operator scanning logs.
    pub fn category(&self) -> &'static str {
        match self {
            Revocation::Authentication { .. } => "authentication",
            Revocation::Compatibility { .. } => "compatibility",
        }
    }

    /// The §13 checks that regressed, if the cause was a compatibility loss.
    ///
    /// §13 makes a failed check an operator task — compare the pinned
    /// upstream, update models and fixtures, re-run the spike — and which
    /// checks failed is the part that says where to start. Kept out of the
    /// free-text detail so it survives as data rather than prose.
    pub fn failed_checks(&self) -> &[Check] {
        match self {
            Revocation::Authentication { .. } => &[],
            Revocation::Compatibility { failed, .. } => failed,
        }
    }

    /// The failed checks by §13 number, for the alert and the refusal.
    ///
    /// Empty string when there are none, so a caller can include it
    /// unconditionally.
    pub fn failed_check_numbers(&self) -> String {
        self.failed_checks()
            .iter()
            .map(|check| check.number().to_string())
            .collect::<Vec<_>>()
            .join(",")
    }

    fn detail(&self) -> &str {
        match self {
            Revocation::Authentication { detail } => detail,
            Revocation::Compatibility { detail, .. } => detail,
        }
    }
}

/// A write was refused because `WRITE_READY` is not held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blocked {
    pub revocation: Revocation,
}

impl std::fmt::Display for Blocked {
    /// Names the specific §13 checks when there are any. A refusal that said
    /// only "compatibility" would leave an operator to rediscover which of
    /// the nine regressed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "writes are disabled: {} ({})",
            self.revocation.category(),
            self.revocation.detail()
        )?;
        let checks = self.revocation.failed_check_numbers();
        if !checks.is_empty() {
            write!(f, " [§13 checks {checks}]")?;
        }
        Ok(())
    }
}

impl std::error::Error for Blocked {}

#[derive(Debug)]
enum State {
    Ready(Box<WriteReady>),
    Revoked(Box<Revocation>),
}

#[derive(Debug)]
struct Inner {
    state: State,
    in_flight: usize,
}

/// Holds `WRITE_READY` for the life of the process.
#[derive(Debug, Clone)]
pub struct WriteGate {
    inner: Arc<Mutex<Inner>>,
}

/// Say, every time the gate is granted, that §13 check 3 passed on an
/// acknowledgement rather than on evidence.
///
/// `tig_integration.md` §13.2 permits that deviation on exactly one ground:
/// the acknowledgement stays visible, so a pass granted without resolving a
/// digest never looks like one granted with. An accessor nobody calls does
/// not make it visible — the first version of this shipped only that, and
/// the document asserted a reporting that did not exist.
///
/// At `warn`, and at both entry points. This is the single moment the pool
/// takes permission to write while a §13 check went unperformed; once per
/// gate opening is not a volume that buries anything, and a lower level
/// would put it beneath the threshold an operator runs in production.
fn report_unresolved_containers(ready: &WriteReady) {
    if let Some(reason) = ready.containers_unresolved() {
        tracing::warn!(
            // Its own event name, not the opening's. Reusing
            // `gateway.write_ready.restored` put two records under one name
            // on every restore with an acknowledgement active, so a
            // dashboard counting restorations (§10.2) double-counted in
            // exactly the deviation case.
            event = "gateway.write_ready.containers_unresolved",
            check = "container_digests",
            reason,
            "writes enabled with §13 check 3 unperformed; see tig_integration.md §13.2"
        );
    }
}

impl WriteGate {
    /// Open the gate with proof that all nine §13 checks passed.
    ///
    /// Takes [`WriteReady`], which has no public constructor, so a gate
    /// cannot be opened by code that skipped the compatibility evaluation.
    pub fn open(ready: WriteReady) -> Self {
        report_unresolved_containers(&ready);
        Self {
            inner: Arc::new(Mutex::new(Inner {
                state: State::Ready(Box::new(ready)),
                in_flight: 0,
            })),
        }
    }

    /// A lock this process poisoned is not a reason to keep writing.
    ///
    /// Recovering the guard rather than propagating the panic would let
    /// writes continue against state a panicking thread left half-updated;
    /// the counter behind it decides whether an intent is in flight.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Admit one write, if `WRITE_READY` is held.
    ///
    /// The returned permit is what keeps the write in flight; dropping it
    /// releases the slot.
    pub fn begin_write(&self) -> Result<WritePermit, Blocked> {
        let mut inner = self.lock();
        match &inner.state {
            State::Revoked(revocation) => Err(Blocked {
                revocation: (**revocation).clone(),
            }),
            State::Ready(ready) => {
                let ready = (**ready).clone();
                inner.in_flight += 1;
                Ok(WritePermit {
                    inner: Arc::clone(&self.inner),
                    ready,
                })
            }
        }
    }

    /// Lose `WRITE_READY`, blocking new writes and raising the §10.3 alert.
    ///
    /// Deliberately does not wait for in-flight writes to drain and does not
    /// cancel them: see the module doc. It reports how many are still in
    /// flight, because that is what an operator needs to know before
    /// deciding anything — those outcomes are still arriving.
    ///
    /// Returns whether this call was the one that lost it. Re-revoking an
    /// already-revoked gate keeps the FIRST cause and does not alert again:
    /// a failing credential produces one alert, not one per rejected
    /// request, and the first cause is the one that explains the rest.
    pub fn revoke(&self, revocation: Revocation) -> bool {
        let mut inner = self.lock();
        if let State::Revoked(existing) = &inner.state {
            tracing::debug!(
                event = "gateway.write_ready.already_lost",
                category = revocation.category(),
                first_category = existing.category(),
            );
            return false;
        }
        tracing::error!(
            event = "gateway.write_ready.lost",
            category = revocation.category(),
            detail = revocation.detail(),
            // Which of the nine regressed, as data. §13 makes this an
            // operator task and this is the part that says where to start.
            failed_checks = revocation.failed_check_numbers(),
            in_flight = inner.in_flight,
            "TIG writes are disabled (architecture.md §10.3)"
        );
        inner.state = State::Revoked(Box::new(revocation));
        true
    }

    /// Regain `WRITE_READY` after a fresh evaluation.
    ///
    /// Requires a new [`WriteReady`], so recovery runs all nine §13 checks
    /// again rather than clearing a flag — a credential that started working
    /// again says nothing about whether the schema still matches.
    pub fn restore(&self, ready: WriteReady) -> bool {
        let mut inner = self.lock();
        if matches!(inner.state, State::Ready(_)) {
            return false;
        }
        tracing::info!(
            event = "gateway.write_ready.restored",
            in_flight = inner.in_flight,
        );
        report_unresolved_containers(&ready);
        inner.state = State::Ready(Box::new(ready));
        true
    }

    /// Whether new writes are currently admitted.
    pub fn is_ready(&self) -> bool {
        matches!(self.lock().state, State::Ready(_))
    }

    /// Why writes are disabled, if they are.
    pub fn revocation(&self) -> Option<Revocation> {
        match &self.lock().state {
            State::Revoked(revocation) => Some((**revocation).clone()),
            State::Ready(_) => None,
        }
    }

    /// Writes admitted and not yet finished.
    ///
    /// Stays meaningful after revocation: §10.3 alerts while an outcome is
    /// ambiguous, and an operator deciding what to do about a revoked gate
    /// needs to know whether anything is still outstanding.
    pub fn in_flight(&self) -> usize {
        self.lock().in_flight
    }
}

/// One admitted write, held until the write reaches an outcome.
///
/// Survives revocation by design (B3, and the module doc). Dropping it is
/// what marks the write no longer in flight, so it is held across the whole
/// attempt rather than released once a request has been sent.
#[derive(Debug)]
pub struct WritePermit {
    inner: Arc<Mutex<Inner>>,
    ready: WriteReady,
}

impl WritePermit {
    /// The gate decision this write was admitted under.
    ///
    /// Carried on the permit rather than read back off the gate, so a write
    /// records the upstream commit and network that were in force when it
    /// was admitted — not whatever the gate holds by the time the attempt is
    /// written down, which may already have been revoked and restored
    /// against a different pin.
    pub fn admitted_under(&self) -> &WriteReady {
        &self.ready
    }
}

impl Drop for WritePermit {
    fn drop(&mut self) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        // Saturating rather than wrapping: an underflow here would report a
        // huge number of in-flight writes to the §10.3 alert, which is
        // exactly when the number is being relied on.
        inner.in_flight = inner.in_flight.saturating_sub(1);
    }
}
