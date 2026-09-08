//! The confirmation-driven workflow state machine
//! (`tig_integration.md` §7, `architecture.md` §7.5).
//!
//! One rule shapes this module: **local state advances only from confirmed
//! reads.** §7 puts it plainly — a recorded transport attempt and response is
//! "not confirmation". So every transition here takes a piece of *confirmed
//! TIG evidence* as its argument, and there is no function that advances a
//! workflow from an HTTP status. A caller holding a 200 has nothing to call.
//!
//! That is why the evidence types below carry a `block_confirmed`: §7's whole
//! mapping keys on "matching entry ... with non-null `state.block_confirmed`",
//! and a type that could be built without one would let a caller pass off a
//! submission as a confirmation.
//!
//! Transitions are compare-and-set on `revision` (§7.5 step 3). A claimant that
//! read the row, did slow work, and came back to commit is refused if anything
//! moved underneath it — which is what makes a late result from a lost lease
//! unable to land (invariant 8).

use pool_domain::Network;
use sqlx::{PgPool, Row};

/// Who owns a workflow, and therefore its benchmark's faults
/// (`mining_system.md` §2, §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// A pool-owned bootstrap workflow, created before members exist.
    ///
    /// Permanent. `mining_system.md` §10 invariant 1 carves these out
    /// explicitly, because a row that could later be re-owned is a row whose
    /// past faults could be re-attributed to whoever owns it next.
    PoolBootstrap,
    /// A member-owned workflow. No slice-1 row is one; the variant exists so
    /// the negative test has something to assert against and so the
    /// registration slice adds rows rather than changing this type.
    Member,
}

/// The one pool-owned id. Deliberately not member-shaped.
pub const POOL_BOOTSTRAP_OWNER: &str = "pool-bootstrap";

impl Owner {
    pub fn as_str(self) -> &'static str {
        match self {
            Owner::PoolBootstrap => "POOL_BOOTSTRAP",
            Owner::Member => "MEMBER",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "POOL_BOOTSTRAP" => Some(Owner::PoolBootstrap),
            "MEMBER" => Some(Owner::Member),
            _ => None,
        }
    }
}

/// The states `mining_system.md` §4.5 names, as far as slice 1 can reach them.
///
/// §4.5 owns the protocol lifecycle. The `*Submitted` states are in its ladder
/// and record that a write was sent; §4.5 says in the same section that "TIG
/// HTTP acceptance alone is not protocol confirmation", which is exactly the
/// distinction. A submission is a local fact worth knowing — it is what §10
/// reconciles against, and what separates "decided but unsent" from "sent and
/// awaiting an answer" — and it is not evidence of anything at TIG.
///
/// Each `*Confirmed` state names confirmed TIG evidence, per
/// `tig_integration.md` §7, and the only way to reach one is that evidence.
///
/// §4.5's member- and artifact-facing states (`ASSIGNED`, `COMPUTING`,
/// `PACKAGING`, the `PACKAGE_*` ladder, `PROOF_BUILDING`, `PROOF_READY`,
/// `VERIFYING`, `ACTIVE`) are absent because slice 1 has neither members nor
/// artifacts to reach them; they arrive with their own slices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowState {
    Decided,
    /// The precommit write was sent. A local fact, not a confirmation.
    PrecommitSubmitted,
    PrecommitConfirmed,
    BenchmarkSubmitted,
    BenchmarkConfirmed,
    ProofSubmitted,
    ProofConfirmed,
    Verified,
    /// A confirmed benchmark with `details.stopped` true. §7: no proof is sent.
    Stopped,
    Fraudulent,
    /// A terminal pool-side failure (§4.5's `FAILED` branch).
    Failed,
    /// A deadline passed with no confirming evidence.
    ///
    /// The pool's own conclusion, not TIG's. `tig_integration.md` §7 makes
    /// confirmed reads the only lifecycle authority and says TIG's sets are
    /// "authoritative even if a locally calculated activation height differs",
    /// so this is the one state confirmed evidence may still supersede — see
    /// [`WorkflowState::is_terminal`].
    Expired,
}

impl WorkflowState {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkflowState::Decided => "DECIDED",
            WorkflowState::PrecommitSubmitted => "PRECOMMIT_SUBMITTED",
            WorkflowState::PrecommitConfirmed => "PRECOMMIT_CONFIRMED",
            WorkflowState::BenchmarkSubmitted => "BENCHMARK_SUBMITTED",
            WorkflowState::BenchmarkConfirmed => "BENCHMARK_CONFIRMED",
            WorkflowState::ProofSubmitted => "PROOF_SUBMITTED",
            WorkflowState::ProofConfirmed => "PROOF_CONFIRMED",
            WorkflowState::Verified => "VERIFIED",
            WorkflowState::Stopped => "STOPPED",
            WorkflowState::Fraudulent => "FRAUDULENT",
            WorkflowState::Expired => "EXPIRED",
            WorkflowState::Failed => "FAILED",
        }
    }

    /// Public so callers reading the column directly can go through the
    /// enum rather than re-encoding its state list.
    pub fn parse_state(text: &str) -> Option<Self> {
        match text {
            "DECIDED" => Some(WorkflowState::Decided),
            "PRECOMMIT_SUBMITTED" => Some(WorkflowState::PrecommitSubmitted),
            "PRECOMMIT_CONFIRMED" => Some(WorkflowState::PrecommitConfirmed),
            "BENCHMARK_SUBMITTED" => Some(WorkflowState::BenchmarkSubmitted),
            "BENCHMARK_CONFIRMED" => Some(WorkflowState::BenchmarkConfirmed),
            "PROOF_SUBMITTED" => Some(WorkflowState::ProofSubmitted),
            "PROOF_CONFIRMED" => Some(WorkflowState::ProofConfirmed),
            "VERIFIED" => Some(WorkflowState::Verified),
            "STOPPED" => Some(WorkflowState::Stopped),
            "FRAUDULENT" => Some(WorkflowState::Fraudulent),
            "EXPIRED" => Some(WorkflowState::Expired),
            "FAILED" => Some(WorkflowState::Failed),
            _ => None,
        }
    }

    /// Whether the workflow has reached one of §4.5's terminal branches.
    ///
    /// §4.5 names exactly four: `STOPPED`, `EXPIRED`, `FAILED`, `FRAUDULENT`.
    /// `Verified` is **not** one of them — §4.5's ladder continues
    /// `PROOF_CONFIRMED -> VERIFYING -> ACTIVE`, and `mining_system.md` §9 and
    /// §10 invariant 17 treat *active* as distinct from *terminal*. Closing
    /// §6.1's unverified interval at verification is a separate fact and still
    /// happens; it is what stops the benchmark consuming concurrency, not a
    /// claim that the workflow is finished.
    ///
    /// An earlier version of this module made `Expired` non-terminal so that
    /// confirmed TIG evidence could supersede a local deadline. That was a
    /// reinterpretation of the owning document rather than an implementation
    /// of it: §4.5 calls EXPIRED terminal, §6.1 ends the unverified interval
    /// when a benchmark "reaches a terminal ... expired ... state", and
    /// `tig_integration.md` §10 step 1 reloads only *nonterminal* workflows —
    /// so that supersession could never have fired through the documented
    /// reconciliation anyway.
    ///
    /// Confirmed evidence arriving for a terminal workflow is a
    /// **discrepancy**, and this module reports it as one
    /// ([`WorkflowError::TerminalStateContradicted`]). §10's answer to the
    /// pool's record and TIG's disagreeing is to stop for operator resolution,
    /// which is the same shape E4 uses for an ambiguous precommit — not to
    /// rewrite history in place.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            WorkflowState::Stopped
                | WorkflowState::Fraudulent
                | WorkflowState::Expired
                | WorkflowState::Failed
        )
    }
}

/// One workflow row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workflow {
    pub workflow_id: String,
    pub network: Network,
    pub state: WorkflowState,
    pub revision: i32,
    pub owner: Owner,
    pub owner_id: String,
    pub benchmark_id: Option<String>,
    /// §6.1's interval opens at creation, so this is always set.
    pub unverified_from_block: i64,
    pub unverified_to_block: Option<i64>,
    pub confirmed_track_id: Option<String>,
    pub confirmed_settings: Option<serde_json::Value>,
    pub precommit_confirmed_block: Option<i64>,
    pub terminal_reason: Option<String>,
}

/// A confirmed precommit, as §7 defines one.
///
/// Constructing it requires the `block_confirmed` height, because that is
/// precisely what distinguishes a confirmed entry from a submitted one. There
/// is no constructor that omits it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedPrecommit {
    pub benchmark_id: String,
    pub block_confirmed: i64,
    /// The track TIG selected, which the pool did not choose.
    pub track_id: String,
    /// The settings TIG recorded. These **replace** the proposed ones (F2).
    pub settings: serde_json::Value,
}

/// A confirmed benchmark entry (§7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedBenchmark {
    pub benchmark_id: String,
    pub block_confirmed: i64,
    /// §7: when true, no proof is sent.
    pub stopped: bool,
}

/// A confirmed proof entry (§7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedProof {
    pub benchmark_id: String,
    pub block_confirmed: i64,
}

/// A confirmed fraud entry (§7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmedFraud {
    pub benchmark_id: String,
    pub block_confirmed: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkflowError {
    #[error("workflow store unavailable: {0}")]
    Unavailable(String),
    #[error("{network} workflow {workflow_id} does not exist")]
    NotFound {
        network: Network,
        workflow_id: String,
    },
    /// §7.5 step 3: the compare-and-set failed.
    #[error(
        "{network} workflow {workflow_id} moved: expected revision {expected}, \
         found {found}"
    )]
    StaleRevision {
        network: Network,
        workflow_id: String,
        expected: i32,
        found: i32,
    },
    /// The evidence names a different benchmark than this workflow owns.
    #[error(
        "{network} workflow {workflow_id} owns benchmark {owned:?}, \
         not {offered}"
    )]
    BenchmarkMismatch {
        network: Network,
        workflow_id: String,
        owned: Option<String>,
        offered: String,
    },
    /// The transition is not one §7's mapping allows from here.
    #[error("{network} workflow {workflow_id} cannot go {from} -> {to}")]
    NotAllowed {
        network: Network,
        workflow_id: String,
        from: &'static str,
        to: &'static str,
    },
    /// F4b: slice 1 records no member fault.
    #[error("terminal reason {reason:?} attributes fault to a member, which slice 1 cannot do")]
    MemberFaultNotAvailable { reason: String },
    /// Confirmed TIG evidence arrived for a workflow already terminal.
    ///
    /// §4.5 makes the terminal branches terminal, so this is a **discrepancy**
    /// between the pool's record and TIG's, not a transition. §10's answer to
    /// that is to stop for operator resolution — the same shape E4 uses for an
    /// ambiguous precommit — rather than rewrite a settled history.
    #[error(
        "{network} workflow {workflow_id} is already {state}; confirmed TIG \
         evidence contradicting it needs operator resolution"
    )]
    TerminalStateContradicted {
        network: Network,
        workflow_id: String,
        state: &'static str,
    },
    /// Another workflow already owns this benchmark.
    ///
    /// A permanent conflict, not an outage: reported distinctly so a caller
    /// does not retry it forever.
    #[error("{network} benchmark {benchmark_id} already belongs to another workflow")]
    BenchmarkAlreadyClaimed {
        network: Network,
        benchmark_id: String,
    },
    #[error("stored workflow row is unreadable: {0}")]
    Corrupt(String),
}

fn unavailable(e: sqlx::Error) -> WorkflowError {
    WorkflowError::Unavailable(e.to_string())
}

/// Create the workflow row for a decision (`architecture.md` §5.1 step 4).
///
/// The owner is fixed here and never again: F6 requires the mapping to exist
/// from creation, and the trigger makes it immutable.
///
/// `unverified_from_block` opens `mining_system.md` §6.1's interval, which runs
/// "from creation of its pool precommit intent" — so it opens **here**, at the
/// decision's anchor height, and not at confirmation. §7.6's concurrency
/// counting reads it, and opening it later would leave the blocks between the
/// intent and its confirmation uncounted, under-counting in the direction §10
/// invariants 22 and 23 forbid.
pub async fn create(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
    owner: Owner,
    owner_id: &str,
    unverified_from_block: i64,
) -> Result<Workflow, WorkflowError> {
    let row = sqlx::query(
        "INSERT INTO pool.workflow
             (workflow_id, network, owner_kind, owner_id, unverified_from_block)
         VALUES ($1, $2, $3, $4, $5)
         RETURNING workflow_id, network, state, revision, owner_kind, owner_id,
                   benchmark_id, unverified_from_block, unverified_to_block,
                   confirmed_track_id, confirmed_settings,
                   precommit_confirmed_block, terminal_reason",
    )
    .bind(workflow_id)
    .bind(network.as_str())
    .bind(owner.as_str())
    .bind(owner_id)
    .bind(unverified_from_block)
    .fetch_one(pool)
    .await
    .map_err(unavailable)?;
    row_to_workflow(&row)
}

pub async fn find(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
) -> Result<Option<Workflow>, WorkflowError> {
    let row = sqlx::query(
        "SELECT workflow_id, network, state, revision, owner_kind, owner_id,
                benchmark_id, unverified_from_block, unverified_to_block,
                confirmed_track_id, confirmed_settings,
                precommit_confirmed_block, terminal_reason
         FROM pool.workflow WHERE network = $1 AND workflow_id = $2",
    )
    .bind(network.as_str())
    .bind(workflow_id)
    .fetch_optional(pool)
    .await
    .map_err(unavailable)?;
    row.as_ref().map(row_to_workflow).transpose()
}

fn row_to_workflow(row: &sqlx::postgres::PgRow) -> Result<Workflow, WorkflowError> {
    let corrupt = |what: &str| WorkflowError::Corrupt(what.to_string());
    let network: String = row.try_get("network").map_err(unavailable)?;
    let state: String = row.try_get("state").map_err(unavailable)?;
    let owner_kind: String = row.try_get("owner_kind").map_err(unavailable)?;
    Ok(Workflow {
        workflow_id: row.try_get("workflow_id").map_err(unavailable)?,
        network: network.parse().map_err(|_| corrupt("network"))?,
        state: WorkflowState::parse_state(&state).ok_or_else(|| corrupt("state"))?,
        revision: row.try_get("revision").map_err(unavailable)?,
        owner: Owner::parse(&owner_kind).ok_or_else(|| corrupt("owner_kind"))?,
        owner_id: row.try_get("owner_id").map_err(unavailable)?,
        benchmark_id: row.try_get("benchmark_id").map_err(unavailable)?,
        unverified_from_block: row.try_get("unverified_from_block").map_err(unavailable)?,
        unverified_to_block: row.try_get("unverified_to_block").map_err(unavailable)?,
        confirmed_track_id: row.try_get("confirmed_track_id").map_err(unavailable)?,
        confirmed_settings: row.try_get("confirmed_settings").map_err(unavailable)?,
        precommit_confirmed_block: row
            .try_get("precommit_confirmed_block")
            .map_err(unavailable)?,
        terminal_reason: row.try_get("terminal_reason").map_err(unavailable)?,
    })
}

/// Which write was sent (§4.5's `*_SUBMITTED` states).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Submitted {
    Precommit,
    Benchmark,
    Proof,
}

/// Record that a write was sent (`mining_system.md` §4.5).
///
/// This advances no *protocol* state: it says the pool put a request on the
/// wire, which is what `tig_integration.md` §10 reconciles against and what
/// separates "decided but unsent" from "sent and awaiting an answer". §4.5
/// puts these states in its ladder and says in the same section that "TIG HTTP
/// acceptance alone is not protocol confirmation" — both halves are true at
/// once, and nothing here claims otherwise: every confirmation function below
/// requires confirmed TIG evidence and none accepts a submission instead.
///
/// The controller records this from the gateway's attempt ledger, which is why
/// it takes no evidence argument. That a request was sent is a local fact the
/// pool already owns.
pub async fn record_submitted(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
    at_revision: i32,
    which: Submitted,
) -> Result<Workflow, WorkflowError> {
    let (from, to) = match which {
        Submitted::Precommit => (WorkflowState::Decided, WorkflowState::PrecommitSubmitted),
        Submitted::Benchmark => (
            WorkflowState::PrecommitConfirmed,
            WorkflowState::BenchmarkSubmitted,
        ),
        Submitted::Proof => (
            WorkflowState::BenchmarkConfirmed,
            WorkflowState::ProofSubmitted,
        ),
    };
    transition(pool, network, workflow_id, at_revision, to, |current| {
        if current.state != from {
            return Err(WorkflowError::NotAllowed {
                network,
                workflow_id: workflow_id.to_string(),
                from: current.state.as_str(),
                to: to.as_str(),
            });
        }
        Ok(Fields::default())
    })
    .await
}

/// §7: a confirmed precommit. The confirmed settings **replace** the proposed
/// ones (F2). §6.1's unverified interval is already open — it opened when the
/// workflow was created.
pub async fn confirm_precommit(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
    at_revision: i32,
    evidence: &ConfirmedPrecommit,
) -> Result<Workflow, WorkflowError> {
    transition(
        pool,
        network,
        workflow_id,
        at_revision,
        WorkflowState::PrecommitConfirmed,
        |current| {
            // From DECIDED or PRECOMMIT_SUBMITTED: a confirmation can arrive
            // without the pool having recorded the submission, after a
            // restart. Not from a terminal state — §4.5 makes those terminal,
            // and evidence contradicting one is a discrepancy for an operator.
            if current.state.is_terminal() {
                return Err(WorkflowError::TerminalStateContradicted {
                    network,
                    workflow_id: workflow_id.to_string(),
                    state: current.state.as_str(),
                });
            }
            if !matches!(
                current.state,
                WorkflowState::Decided | WorkflowState::PrecommitSubmitted
            ) {
                return Err(WorkflowError::NotAllowed {
                    network,
                    workflow_id: workflow_id.to_string(),
                    from: current.state.as_str(),
                    to: WorkflowState::PrecommitConfirmed.as_str(),
                });
            }
            Ok(Fields {
                benchmark_id: Some(evidence.benchmark_id.clone()),
                confirmed: Some((
                    evidence.track_id.clone(),
                    evidence.settings.clone(),
                    evidence.block_confirmed,
                )),
                // The interval is already open: §6.1 opened it at creation.
                ..Fields::default()
            })
        },
    )
    .await
}

/// §7: a confirmed benchmark. `details.stopped` decides whether a proof is
/// ever sent, so it decides the state rather than being recorded beside it.
pub async fn confirm_benchmark(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
    at_revision: i32,
    evidence: &ConfirmedBenchmark,
) -> Result<Workflow, WorkflowError> {
    let to = if evidence.stopped {
        WorkflowState::Stopped
    } else {
        WorkflowState::BenchmarkConfirmed
    };
    let closes = evidence.stopped.then_some(evidence.block_confirmed);
    transition(pool, network, workflow_id, at_revision, to, |current| {
        expect_benchmark(network, workflow_id, current, &evidence.benchmark_id)?;
        if current.state.is_terminal() {
            return Err(WorkflowError::TerminalStateContradicted {
                network,
                workflow_id: workflow_id.to_string(),
                state: current.state.as_str(),
            });
        }
        if !matches!(
            current.state,
            WorkflowState::PrecommitConfirmed | WorkflowState::BenchmarkSubmitted
        ) {
            return Err(WorkflowError::NotAllowed {
                network,
                workflow_id: workflow_id.to_string(),
                from: current.state.as_str(),
                to: to.as_str(),
            });
        }
        Ok(Fields {
            unverified_to_block: closes,
            terminal_reason: evidence
                .stopped
                .then(|| "TIG stopped the benchmark; §7 sends no proof".to_string()),
            ..Fields::default()
        })
    })
    .await
}

/// §7: a confirmed proof.
pub async fn confirm_proof(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
    at_revision: i32,
    evidence: &ConfirmedProof,
) -> Result<Workflow, WorkflowError> {
    transition(
        pool,
        network,
        workflow_id,
        at_revision,
        WorkflowState::ProofConfirmed,
        |current| {
            expect_benchmark(network, workflow_id, current, &evidence.benchmark_id)?;
            if current.state.is_terminal() {
                return Err(WorkflowError::TerminalStateContradicted {
                    network,
                    workflow_id: workflow_id.to_string(),
                    state: current.state.as_str(),
                });
            }
            if !matches!(
                current.state,
                WorkflowState::BenchmarkConfirmed | WorkflowState::ProofSubmitted
            ) {
                return Err(WorkflowError::NotAllowed {
                    network,
                    workflow_id: workflow_id.to_string(),
                    from: current.state.as_str(),
                    to: WorkflowState::ProofConfirmed.as_str(),
                });
            }
            Ok(Fields::default())
        },
    )
    .await
}

/// §7: the benchmark id appears in `block.data.confirmed_ids.verified`. The
/// unverified interval closes here — this is the event §7.6's concurrency
/// counting waits for.
pub async fn confirm_verified(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
    at_revision: i32,
    benchmark_id: &str,
    at_block: i64,
) -> Result<Workflow, WorkflowError> {
    transition(
        pool,
        network,
        workflow_id,
        at_revision,
        WorkflowState::Verified,
        |current| {
            expect_benchmark(network, workflow_id, current, benchmark_id)?;
            if current.state.is_terminal() {
                return Err(WorkflowError::TerminalStateContradicted {
                    network,
                    workflow_id: workflow_id.to_string(),
                    state: current.state.as_str(),
                });
            }
            if current.state != WorkflowState::ProofConfirmed {
                return Err(WorkflowError::NotAllowed {
                    network,
                    workflow_id: workflow_id.to_string(),
                    from: current.state.as_str(),
                    to: WorkflowState::Verified.as_str(),
                });
            }
            Ok(Fields {
                unverified_to_block: Some(at_block),

                ..Fields::default()
            })
        },
    )
    .await
}

/// §7: a confirmed fraud entry.
///
/// Reachable from any non-terminal state that owns a benchmark: TIG can
/// record fraud against a benchmark whose proof has confirmed or has not.
/// The terminal reason names the TIG evidence and **not** a member, which is
/// F4b's rule — slice 1 has no members to attribute fault to.
pub async fn confirm_fraud(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
    at_revision: i32,
    evidence: &ConfirmedFraud,
) -> Result<Workflow, WorkflowError> {
    transition(
        pool,
        network,
        workflow_id,
        at_revision,
        WorkflowState::Fraudulent,
        |current| {
            expect_benchmark(network, workflow_id, current, &evidence.benchmark_id)?;
            if current.state.is_terminal() {
                return Err(WorkflowError::TerminalStateContradicted {
                    network,
                    workflow_id: workflow_id.to_string(),
                    state: current.state.as_str(),
                });
            }
            Ok(Fields {
                unverified_to_block: Some(evidence.block_confirmed),
                terminal_reason: Some("TIG confirmed a fraud entry".to_string()),
                ..Fields::default()
            })
        },
    )
    .await
}

/// §4.5's `FAILED` branch: the write was refused, so this workflow will never
/// produce a benchmark.
///
/// Without it §6.1's interval has no closing path for a workflow whose
/// precommit TIG rejected — and since [`crate::decision::admit_precommit`]
/// counts open intervals, that slot would be consumed for the lifetime of the
/// database. Enough rejections would wedge admission entirely.
pub async fn fail(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
    at_revision: i32,
    reason: &str,
    at_block: i64,
) -> Result<Workflow, WorkflowError> {
    reject_member_fault(reason)?;
    transition(
        pool,
        network,
        workflow_id,
        at_revision,
        WorkflowState::Failed,
        |current| {
            if current.state.is_terminal() {
                return Err(WorkflowError::NotAllowed {
                    network,
                    workflow_id: workflow_id.to_string(),
                    from: current.state.as_str(),
                    to: WorkflowState::Failed.as_str(),
                });
            }
            Ok(Fields {
                unverified_to_block: Some(at_block),
                terminal_reason: Some(reason.to_string()),
                ..Fields::default()
            })
        },
    )
    .await
}

/// A deadline passed with no confirming evidence (F5).
///
/// The reason is checked against F4b: slice 1 may record that the pool's own
/// deadline expired, never that a member failed.
pub async fn expire(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
    at_revision: i32,
    reason: &str,
    at_block: i64,
) -> Result<Workflow, WorkflowError> {
    reject_member_fault(reason)?;
    transition(
        pool,
        network,
        workflow_id,
        at_revision,
        WorkflowState::Expired,
        |current| {
            // A TIG-confirmed terminal state, or an expiry already recorded.
            // The pool's deadline never overrules TIG's record and never
            // fires twice.
            if current.state.is_terminal() {
                return Err(WorkflowError::NotAllowed {
                    network,
                    workflow_id: workflow_id.to_string(),
                    from: current.state.as_str(),
                    to: WorkflowState::Expired.as_str(),
                });
            }
            Ok(Fields {
                unverified_to_block: Some(at_block),
                terminal_reason: Some(reason.to_string()),
                ..Fields::default()
            })
        },
    )
    .await
}

/// F4b, asserted in code rather than only in prose.
///
/// `mining_system.md` §8 allows a chargeable tier failure only after fault
/// attribution classifies the outcome as `MEMBER`, and slice 1 has no members
/// to classify. A terminal reason that names member fault would be a fault
/// record against a benchmark whose owner is the pool itself.
fn reject_member_fault(reason: &str) -> Result<(), WorkflowError> {
    let lowered = reason.to_ascii_lowercase();
    for marker in ["member", "chargeable", "slash"] {
        if lowered.contains(marker) {
            return Err(WorkflowError::MemberFaultNotAvailable {
                reason: reason.to_string(),
            });
        }
    }
    Ok(())
}

fn expect_benchmark(
    network: Network,
    workflow_id: &str,
    current: &Workflow,
    offered: &str,
) -> Result<(), WorkflowError> {
    if current.benchmark_id.as_deref() != Some(offered) {
        return Err(WorkflowError::BenchmarkMismatch {
            network,
            workflow_id: workflow_id.to_string(),
            owned: current.benchmark_id.clone(),
            offered: offered.to_string(),
        });
    }
    Ok(())
}

/// The fields a transition sets. `None` leaves the stored value alone.
#[derive(Debug, Default)]
struct Fields {
    benchmark_id: Option<String>,
    confirmed: Option<(String, serde_json::Value, i64)>,
    unverified_to_block: Option<i64>,
    terminal_reason: Option<String>,
}

/// One short transition: lock the row, check the guard, compare-and-set the
/// revision (`architecture.md` §7.5).
async fn transition(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
    at_revision: i32,
    to: WorkflowState,
    decide: impl FnOnce(&Workflow) -> Result<Fields, WorkflowError>,
) -> Result<Workflow, WorkflowError> {
    let mut tx = pool.begin().await.map_err(unavailable)?;

    // FOR UPDATE, so the guard reads what the update will write against.
    let row = sqlx::query(
        "SELECT workflow_id, network, state, revision, owner_kind, owner_id,
                benchmark_id, unverified_from_block, unverified_to_block,
                confirmed_track_id, confirmed_settings,
                precommit_confirmed_block, terminal_reason
         FROM pool.workflow
         WHERE network = $1 AND workflow_id = $2
         FOR UPDATE",
    )
    .bind(network.as_str())
    .bind(workflow_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(unavailable)?;

    let Some(row) = row else {
        return Err(WorkflowError::NotFound {
            network,
            workflow_id: workflow_id.to_string(),
        });
    };
    let current = row_to_workflow(&row)?;

    // The compare-and-set. Checked before the guard so a caller working from a
    // stale read is told *that*, rather than being told its transition was
    // disallowed by a state it never saw.
    if current.revision != at_revision {
        return Err(WorkflowError::StaleRevision {
            network,
            workflow_id: workflow_id.to_string(),
            expected: at_revision,
            found: current.revision,
        });
    }
    // The fields are decided from the row read under the lock, so a value
    // that depends on where the workflow *was* — such as whether this
    // confirmation supersedes a local expiry — cannot be computed from a
    // stale read.
    let fields = decide(&current)?;

    let fields_benchmark = fields.benchmark_id.clone();
    let (track, settings, confirmed_block) = match fields.confirmed {
        Some((track, settings, block)) => (Some(track), Some(settings), Some(block)),
        None => (None, None, None),
    };

    let updated = sqlx::query(
        "UPDATE pool.workflow SET
             state = $3,
             revision = revision + 1,
             benchmark_id = COALESCE($4, benchmark_id),
             confirmed_track_id = COALESCE($5, confirmed_track_id),
             confirmed_settings = COALESCE($6, confirmed_settings),
             precommit_confirmed_block = COALESCE($7, precommit_confirmed_block),
             -- §6.1 closes the interval at the *earlier* of TIG verification
             -- or a terminal state, and nothing reopens it: COALESCE keeps the
             -- first close, so a later transition cannot move it. §7.6's
             -- per-block aggregates may already have consumed that close, and
             -- changing it afterwards would change a count already taken.
             unverified_to_block = COALESCE(unverified_to_block, $8),
             terminal_reason = COALESCE($9, terminal_reason)
         WHERE network = $1 AND workflow_id = $2 AND revision = $10
         RETURNING workflow_id, network, state, revision, owner_kind, owner_id,
                   benchmark_id, unverified_from_block, unverified_to_block,
                   confirmed_track_id, confirmed_settings,
                   precommit_confirmed_block, terminal_reason",
    )
    .bind(network.as_str())
    .bind(workflow_id)
    .bind(to.as_str())
    .bind(fields.benchmark_id)
    .bind(track)
    .bind(settings)
    .bind(confirmed_block)
    .bind(fields.unverified_to_block)
    .bind(fields.terminal_reason)
    .bind(at_revision)
    .fetch_one(&mut *tx)
    .await
    // A permanent conflict, not an outage: another workflow already owns this
    // benchmark. Reported distinctly so a caller does not retry it forever.
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.constraint() == Some("workflow_one_per_benchmark") => {
            WorkflowError::BenchmarkAlreadyClaimed {
                network,
                benchmark_id: fields_benchmark.unwrap_or_default(),
            }
        }
        _ => unavailable(e),
    })?;

    let updated = row_to_workflow(&updated)?;
    tx.commit().await.map_err(unavailable)?;
    Ok(updated)
}
