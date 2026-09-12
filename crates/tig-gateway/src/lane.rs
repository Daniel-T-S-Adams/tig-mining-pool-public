//! The POST lane's pacing (`tig_integration.md` §11): initial writes are
//! serialized with a minimum gap between them.
//!
//! The database serializes *unresolved* precommits — one at a time, network
//! wide (`migrations/0004`). It does not pace resolved ones: an attempt that
//! comes back `ACCEPTED` leaves the lane index at once, so a run with several
//! claimable intents would otherwise send them back to back. TIG rate-limits
//! writes per IP and answers a burst with 429, which the transmitter has to
//! record as ambiguous — it cannot know the write was not applied — closing
//! the lane pool-wide and sending §10's search after a write that never
//! landed. The gap is what keeps that from being the ordinary case.
//!
//! One per process: the gateway is the one writer, so process-wide pacing is
//! pool-wide pacing. The value is policy, loaded with the rest of
//! `write_limits`, and there is no compiled fallback (§12).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::write_policy::WritePolicy;

/// The lane's pacing state, shared by every run of the driver.
#[derive(Debug)]
pub struct PostLane {
    policy: WritePolicy,
    last_initial_write: Mutex<Option<Instant>>,
}

impl PostLane {
    pub fn new(policy: WritePolicy) -> Self {
        Self {
            policy,
            last_initial_write: Mutex::new(None),
        }
    }

    pub fn policy(&self) -> &WritePolicy {
        &self.policy
    }

    /// Wait until the lane's minimum gap since the last initial write has
    /// passed, then take the lane's next slot.
    ///
    /// The slot is taken here, before the caller records its attempt and
    /// sends, rather than after the send returns: pacing bounds when requests
    /// *leave*, and a slot taken on return would let a request that failed
    /// fast be followed at once by another. A caller that takes a slot and
    /// then does not send — the lane index refused its attempt, say — has
    /// spent one gap for nothing, which is the conservative side to err on.
    pub async fn take_slot(&self) {
        let wait = {
            let last = self.lock();
            last.map(|at| {
                self.policy
                    .min_between_initial_writes()
                    .saturating_sub(at.elapsed())
            })
        };
        if let Some(wait) = wait
            && wait > Duration::ZERO
        {
            tokio::time::sleep(wait).await;
        }
        *self.lock() = Some(Instant::now());
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Instant>> {
        match self.last_initial_write.lock() {
            Ok(guard) => guard,
            // A panic while holding an `Option<Instant>` cannot have left it
            // inconsistent; refusing every later write over it would be the
            // worse outcome.
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}
