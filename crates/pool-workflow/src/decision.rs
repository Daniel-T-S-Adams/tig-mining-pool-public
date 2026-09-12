//! The precommit admission transaction (`architecture.md` §5.1 step 4, §7.6).
//!
//! One transaction does all of it: take the serialized precommit-admission
//! lease, recount authoritative unverified workflows, check
//! `pool_unverified < internal_pool_unverified_limit`, record the decision
//! with its anchor snapshot and draw, and create the `PRECOMMIT` intent.
//!
//! The shape is the point. `architecture.md` §12 says a controller that dies
//! after the decision commit leaves a claimable intent; a crash *between* a
//! decision and its intent would leave a decision nothing will ever send, and
//! an intent with no decision would be a write nobody can justify. Neither is
//! reachable, because there is no moment when one exists without the other.
//!
//! Two halves of §7.6's gate are deliberately absent, and both must slot in
//! **without reshaping this transaction** — which is why it is built now
//! rather than retrofitted around a path that ran unguarded:
//!
//! - `member_unverified < tier_number` has no meaning before members and
//!   tiers exist (criterion D2b); and
//! - `eligible_collateral - reserved_exposure >= precommit_reserve` is
//!   member-scoped against a matured balance slice 1 has no deposits to
//!   supply (criterion D2c). The reservation *inputs* are recorded here so
//!   the check has something to read when it lands, and no accounting batch
//!   is posted.

use pool_domain::Network;
use sqlx::{PgPool, Row};

use crate::intent::{IntentState, WriteIntent, WriteKind};

/// The lease key for `pg_advisory_xact_lock`.
///
/// §7.6: "Global admission and queue promotion use the serialized
/// precommit-admission lease." Serialized, because the recount and the insert
/// must be one decision: two controllers counting `limit - 1` at the same
/// instant would each admit, and the count is of rows the other is about to
/// write. A transaction-scoped lock releases on commit or rollback, so a
/// panicking admission cannot wedge the next one.
///
/// The value is arbitrary but fixed; it is an application lock namespace, not
/// a protocol constant.
///
/// **It carries no fence token.** `architecture.md` §7.5 describes singleton
/// activities as using a named database lease with a fencing rule; this is a
/// plain advisory lock, which serializes this short transaction correctly but
/// would not stop a standby controller that had lost its controller lease from
/// admitting. That is sound while slice 1 runs exactly one controller
/// (`docs/plans/slice-1-gateway.md` §2 puts multi-instance failover out of
/// scope) and is recorded here rather than left to be inferred: the slice that
/// introduces standbys must check the controller lease fence as well.
pub const PRECOMMIT_ADMISSION_LOCK: i64 = 0x7069_6f6f_6c5f_7061;

/// The anchor snapshot a decision was derived from (D2e).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorSnapshot {
    pub block_id: String,
    /// The `pool.block_snapshot` content digest. A block may carry several
    /// assemblies, so naming the block alone would not say which was read.
    pub content_digest: [u8; 32],
    pub height: i64,
}

/// The §6.3 draw, recorded whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedDraw {
    /// The versioned domain string the seed was derived from.
    pub domain: String,
    /// Every compute-compatible eligible challenge's rank, as lowercase hex.
    /// The full map is the audit evidence: which challenges were eligible at
    /// that instant is not recoverable afterwards.
    pub draw_ranks: serde_json::Map<String, serde_json::Value>,
    /// The tied set and winner, or `None` when nothing tied.
    pub tie: Option<RecordedTie>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedTie {
    pub candidates: Vec<String>,
    pub winner: String,
}

/// What the engine decided, plus the §11.4 reservation inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDecision {
    pub network: Network,
    pub workflow_id: String,
    pub generation: i32,
    pub anchor: AnchorSnapshot,
    pub draw: RecordedDraw,
    pub selected_challenge: String,
    pub selected_algorithm: String,
    /// The TIG `compute_type` this decision was made for (`tig_integration.md`
    /// §3, §6.1). A decision input, and the one the record used not to keep:
    /// the gateway rebuilds the §6.1 body from this record and refuses to
    /// send bytes that do not digest to the intent, so every input to that
    /// body has to be here.
    pub compute_type: String,
    /// The pool's own settings, at the types it chose them.
    pub track_settings: serde_json::Value,
    /// `P[s]`, `B[t]`, `F[s,t]` and the `X` policy version (§11.4). Inputs
    /// only: slice 1 posts no accounting batch.
    pub reserve_inputs: serde_json::Value,
    /// The maximum `assignment_reserve` across proposed tracks, in attoTIG.
    ///
    /// A canonical unsigned base-10 atom string, which `accounting.md` §3 is
    /// what amounts cross this boundary as. It stays a string rather than
    /// becoming an integer type because §3's range is 256-bit and this
    /// workspace has no U256; [`canonical_atoms`] checks it instead, so a
    /// value PostgreSQL would silently reshape — `1.5` rounding to `2`, `1e3`
    /// expanding, `NaN` sorting above every number and satisfying the column's
    /// `>= 0` check — is refused before it reaches the cast.
    pub precommit_reserve: String,
    /// §9's digest of decision-affecting configuration.
    pub config_digest: [u8; 32],
    /// The §7.3 payload digest of the precommit body this decision produces.
    pub payload_digest: [u8; 32],
}

/// A committed decision and the intent created with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admitted {
    pub decision_id: String,
    pub intent: WriteIntent,
    /// The authoritative recount this admission passed, and the limit it was
    /// measured against.
    pub pool_unverified: i64,
    pub unverified_limit: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum AdmissionError {
    /// A precommit for this workflow already left the gateway.
    ///
    /// The workflow is still `DECIDED` because §7 advances it only from a
    /// confirmed *read*, and the read has not happened — or the response that
    /// carried the assigned `benchmark_id` was lost. Either way TIG may hold a
    /// benchmark for this decision already, so a second generation would pay a
    /// second fee and create a second benchmark for one decision, breaking
    /// §10's permanent one-benchmark-per-workflow mapping.
    ///
    /// The way out is §10's tuple search over the exact submitted settings
    /// (criterion E4), which `restart::reconcile_after_restart` reports as
    /// `NeedsAttention::PrecommitSearchOwed`. Until that lands, this workflow
    /// needs an operator.
    #[error("{network} workflow {workflow_id} already has a precommit at TIG")]
    PrecommitAlreadyTransmitted {
        network: Network,
        workflow_id: String,
    },
    #[error("decision store unavailable: {0}")]
    Unavailable(String),
    /// D2a: the pool never creates a precommit at or above the limit.
    #[error(
        "refused: {pool_unverified} unverified workflows is at or above \
         internal_pool_unverified_limit {unverified_limit}"
    )]
    AtUnverifiedLimit {
        pool_unverified: i64,
        unverified_limit: i64,
    },
    /// This generation already decided. A changed decision needs a new one.
    #[error("{network} workflow {workflow_id} generation {generation} is already decided")]
    AlreadyDecided {
        network: Network,
        workflow_id: String,
        generation: i32,
    },
    #[error("internal_pool_unverified_limit must be at least 1, found {0}")]
    LimitNotPositive(i64),
    /// The workflow already exists and has moved past `DECIDED`.
    ///
    /// §7.3 forbids a new generation once an earlier attempt may have reached
    /// TIG unless reconciliation proves it safe, and that reconciliation is
    /// E4's and D3's rather than this transaction's.
    #[error("{network} workflow {workflow_id} is {state}; it cannot take a new write")]
    WorkflowSettled {
        network: Network,
        workflow_id: String,
        state: String,
    },
    /// The anchor is not the newest usable persisted snapshot (D2e).
    ///
    /// §6.3's draw is fixed by the anchor block, so free choice of anchor
    /// restores exactly the re-roll the seed design removes. The engine cannot
    /// prevent it — it is handed the ranks — so the transaction is where it is
    /// closed.
    #[error(
        "anchor {offered_block_id} is not the newest usable snapshot          ({newest_block_id}) for {network}"
    )]
    StaleAnchor {
        network: Network,
        offered_block_id: String,
        newest_block_id: String,
    },
    /// No snapshot is usable for a decision yet.
    #[error("{network} has no complete persisted snapshot to decide from")]
    NoUsableSnapshot { network: Network },
    /// A precommit intent already exists for this generation.
    ///
    /// A permanent conflict (`architecture.md` §7.3), not an outage: reported
    /// distinctly so a caller does not retry it forever.
    #[error("{network} workflow {workflow_id} generation {generation} already has an intent")]
    IntentExists {
        network: Network,
        workflow_id: String,
        generation: i32,
    },
    /// The reserve is not a canonical unsigned atom string (`accounting.md` §3).
    #[error("precommit_reserve {value:?} is not a canonical unsigned atom string: {reason}")]
    ReserveNotCanonical { value: String, reason: &'static str },
    /// The decision names no compute type, so its §6.1 body cannot be built.
    ///
    /// Permanent, and reported as such rather than as `Unavailable`. That
    /// variant is what this module reserves for a retry that can succeed; a
    /// caller with the usual retry-on-outage policy would loop on this for
    /// ever, and an operator reading "decision store unavailable" would look
    /// at the database instead of at the record. Two routes here: a decision
    /// being admitted with a blank compute type (migration 0013's trigger
    /// refuses it), and a decision written before the column existed
    /// (`payload_inputs` finds it null).
    #[error(
        "{network} workflow {workflow_id} generation {generation}: the decision names no compute \
         type, so its precommit body cannot be rebuilt"
    )]
    ComputeTypeMissing {
        network: Network,
        workflow_id: String,
        generation: i32,
    },
}

/// Whether `value` is the canonical unsigned base-10 form `accounting.md` §3
/// requires of an amount crossing into a `NUMERIC(78,0)` column.
///
/// Rejects a sign, a decimal point, an exponent, whitespace, emptiness,
/// redundant leading zeros, and anything wider than the column's precision.
/// `0` itself is canonical.
fn canonical_atoms(value: &str) -> Result<(), AdmissionError> {
    let reject = |reason: &'static str| {
        Err(AdmissionError::ReserveNotCanonical {
            value: value.to_string(),
            reason,
        })
    };
    if value.is_empty() {
        return reject("it is empty");
    }
    if !value.bytes().all(|b| b.is_ascii_digit()) {
        return reject("it holds something other than base-10 digits");
    }
    if value.len() > 78 {
        return reject("it is wider than the column's 78 digits");
    }
    if value.len() > 1 && value.starts_with('0') {
        return reject("it has a redundant leading zero");
    }
    Ok(())
}

fn unavailable(e: sqlx::Error) -> AdmissionError {
    AdmissionError::Unavailable(e.to_string())
}

/// Admit one precommit, or refuse.
///
/// The recount, the gate, the decision and the intent are one transaction
/// under the admission lease. Nothing here reads a cached metric: §7.6 says a
/// cached metric "can drive alerts but can never authorize work", so the count
/// is taken inside the lease from the rows themselves.
pub async fn admit_precommit(
    pool: &PgPool,
    decision: &NewDecision,
    unverified_limit: i64,
) -> Result<Admitted, AdmissionError> {
    if unverified_limit < 1 {
        return Err(AdmissionError::LimitNotPositive(unverified_limit));
    }

    let mut tx = pool.begin().await.map_err(unavailable)?;

    // The lease. Transaction-scoped, so it is released by commit or rollback
    // and never by a caller remembering to.
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(PRECOMMIT_ADMISSION_LOCK)
        .execute(&mut *tx)
        .await
        .map_err(unavailable)?;

    canonical_atoms(&decision.precommit_reserve)?;

    // Already decided is checked FIRST, before the capacity gate. A retry of a
    // generation that already committed must say so, and it would otherwise be
    // reported as AtUnverifiedLimit whenever that workflow's own intent is
    // what fills the last slot — telling a caller it was refused for capacity
    // when in fact its write already exists.
    let already = sqlx::query(
        "SELECT 1 AS present FROM pool.precommit_decision
         WHERE network = $1 AND workflow_id = $2 AND generation = $3",
    )
    .bind(decision.network.as_str())
    .bind(&decision.workflow_id)
    .bind(decision.generation)
    .fetch_optional(&mut *tx)
    .await
    .map_err(unavailable)?;
    if already.is_some() {
        return Err(AdmissionError::AlreadyDecided {
            network: decision.network,
            workflow_id: decision.workflow_id.clone(),
            generation: decision.generation,
        });
    }

    // D2e, inside the lease: the decision uses the newest complete persisted
    // snapshot available when the transaction begins. §6.3's draw is a pure
    // function of the anchor block, so a caller free to choose the anchor
    // keeps the re-roll the seed design exists to remove. Refusing here is
    // what closes it; the foreign key in migration 0005 additionally makes a
    // snapshot that was never persisted impossible to name.
    require_newest_anchor(&mut tx, decision).await?;

    let pool_unverified = count_unverified(&mut tx, decision.network).await?;
    if pool_unverified >= unverified_limit {
        // Rolls back on drop, releasing the lease.
        return Err(AdmissionError::AtUnverifiedLimit {
            pool_unverified,
            unverified_limit,
        });
    }

    let tie_candidates = decision
        .draw
        .tie
        .as_ref()
        .map(|tie| serde_json::Value::from(tie.candidates.clone()));
    let tie_winner = decision.draw.tie.as_ref().map(|tie| tie.winner.clone());

    // F6: the workflow row, with its permanent owner and §6.1's unverified
    // interval already open, is created **here** — inside the same transaction
    // as the decision and the intent (`architecture.md` §7.2).
    //
    // §6.1 counts a benchmark as unverified "from creation of its pool
    // precommit intent", so the interval opens at the decision's anchor
    // height. Creating the workflow afterwards, or in a later transaction,
    // would leave an intent whose owner mapping did not yet exist — a write
    // the pool could make and then be unable to attribute. The foreign key
    // from `tig_write_intent` makes that ordering impossible to skip.
    // FOR UPDATE, and it matters. Without the lock this read races a
    // concurrent `workflow::transition` on the same row: under READ COMMITTED
    // this transaction can see DECIDED while another is committing the move to
    // PRECOMMIT_CONFIRMED, and then admit a second generation for a workflow
    // whose first generation has already confirmed at TIG — two live
    // generations for one benchmark, which is precisely what the check below
    // exists to prevent.
    let existing_state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM pool.workflow
         WHERE network = $1 AND workflow_id = $2
         FOR UPDATE",
    )
    .bind(decision.network.as_str())
    .bind(&decision.workflow_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(unavailable)?;

    match existing_state.as_deref() {
        None => {
            sqlx::query(
                "INSERT INTO pool.workflow
                     (workflow_id, network, owner_kind, owner_id, unverified_from_block)
                 VALUES ($1, $2, 'POOL_BOOTSTRAP', $3, $4)",
            )
            .bind(&decision.workflow_id)
            .bind(decision.network.as_str())
            .bind(crate::workflow::POOL_BOOTSTRAP_OWNER)
            .bind(decision.anchor.height)
            .execute(&mut *tx)
            .await
            .map_err(unavailable)?;
        }
        // §7.3 allows a new generation for a changed payload, but only while
        // the workflow can still receive the write. A workflow that has
        // reached a terminal state has a closed §6.1 interval and no
        // lifecycle path left: `confirm_precommit` would refuse the
        // confirmation, so the pool would have made a TIG write it could
        // never record.
        // §7.3 forbids a new generation once an earlier attempt may have
        // reached TIG, unless reconciliation proves it safe. Past DECIDED the
        // earlier precommit was at least sent, and from PRECOMMIT_CONFIRMED it
        // demonstrably landed — `confirm_precommit` would then refuse the new
        // generation's confirmation, leaving a TIG write the pool could never
        // record. That reconciliation is E4's and D3's; until it lands, only a
        // workflow that has sent nothing may take another generation.
        // DECIDED is necessary but not sufficient. It is where a workflow
        // sits when a precommit was sent and the response was lost: §6.1
        // returns the assigned benchmark_id in that response, nothing durable
        // holds it, and the state advances only from a confirmed *read*. So a
        // DECIDED workflow may have a precommit already at TIG, indexed under
        // a benchmark id the pool cannot name, and admitting a second
        // generation for it would pay a second fee and create a second
        // benchmark for one decision — §10's permanent one-benchmark mapping
        // broken by the pool itself.
        //
        // The attempt ledger is the evidence, because `begin` records the
        // attempt before the request leaves. Nothing sent, nothing to fear;
        // anything sent, and this workflow is owed E4's tuple search, which
        // `restart::reconcile_after_restart` already reports as
        // `PrecommitSearchOwed`.
        Some(state)
            if crate::workflow::WorkflowState::parse_state(state)
                == Some(crate::workflow::WorkflowState::Decided) => {}
        Some(state) => {
            return Err(AdmissionError::WorkflowSettled {
                network: decision.network,
                workflow_id: decision.workflow_id.clone(),
                state: state.to_string(),
            });
        }
    }

    let decision_row = sqlx::query(
        "INSERT INTO pool.precommit_decision
             (network, workflow_id, generation,
              anchor_block_id, anchor_digest, anchor_height,
              tie_domain, tie_draw_ranks, tie_candidates, tie_winner,
              selected_challenge, selected_algorithm, track_settings,
              pool_unverified, unverified_limit,
              reserve_inputs, precommit_reserve, config_digest, compute_type)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                 $16, $17::numeric, $18, $19)
         ON CONFLICT (network, workflow_id, generation) DO NOTHING
         RETURNING decision_id::text AS decision_id",
    )
    .bind(decision.network.as_str())
    .bind(&decision.workflow_id)
    .bind(decision.generation)
    .bind(&decision.anchor.block_id)
    .bind(decision.anchor.content_digest.as_slice())
    .bind(decision.anchor.height)
    .bind(&decision.draw.domain)
    .bind(serde_json::Value::Object(decision.draw.draw_ranks.clone()))
    .bind(tie_candidates)
    .bind(tie_winner)
    .bind(&decision.selected_challenge)
    .bind(&decision.selected_algorithm)
    .bind(&decision.track_settings)
    .bind(pool_unverified)
    .bind(unverified_limit)
    .bind(&decision.reserve_inputs)
    .bind(&decision.precommit_reserve)
    .bind(decision.config_digest.as_slice())
    .bind(&decision.compute_type)
    .fetch_optional(&mut *tx)
    .await
    // Migration 0013's trigger raises on a blank compute type. A record
    // defect, not an outage, and mapped to say so for the same reason the
    // intent conflict below is: a caller must not retry it forever.
    .map_err(|e| match &e {
        sqlx::Error::Database(db) if db.message().contains("names the compute type") => {
            AdmissionError::ComputeTypeMissing {
                network: decision.network,
                workflow_id: decision.workflow_id.clone(),
                generation: decision.generation,
            }
        }
        _ => unavailable(e),
    })?;

    // Unreachable given the check above, but the ON CONFLICT stays: it is what
    // makes a concurrent duplicate the database's decision rather than a race
    // between the SELECT and the INSERT.
    let Some(decision_row) = decision_row else {
        return Err(AdmissionError::AlreadyDecided {
            network: decision.network,
            workflow_id: decision.workflow_id.clone(),
            generation: decision.generation,
        });
    };
    let decision_id: String = decision_row.try_get("decision_id").map_err(unavailable)?;

    let intent_row = sqlx::query(concat!(
        "INSERT INTO pool.tig_write_intent
                 (network, workflow_id, write_kind, generation, benchmark_id,
                  payload_digest, payload_artifact_id)
             SELECT $1, $2, 'precommit', $3, NULL, $4, NULL
              WHERE NOT ",
        // §10: a precommit that reached TIG makes a second generation a
        // second benchmark for one decision. Asked inside the INSERT
        // because the gateway's `begin` writes to a table this
        // transaction holds no lock on, so an attempt starting between a
        // read and this write would not be seen by it.
        crate::attempt::transmitted_precommit_exists!(),
        " RETURNING intent_id::text AS intent_id, network, workflow_id, write_kind,
                       generation, benchmark_id, payload_digest, payload_artifact_id, state",
    ))
    .bind(decision.network.as_str())
    .bind(&decision.workflow_id)
    .bind(decision.generation)
    .bind(decision.payload_digest.as_slice())
    .fetch_optional(&mut *tx)
    .await
    // §7.3 treats this key as a conflict, not an availability problem.
    // Reporting it as Unavailable would invite a caller to retry a permanent
    // failure forever.
    .map_err(|e| match &e {
        sqlx::Error::Database(db)
            if db.constraint() == Some("tig_write_intent_unique_generation") =>
        {
            AdmissionError::IntentExists {
                network: decision.network,
                workflow_id: decision.workflow_id.clone(),
                generation: decision.generation,
            }
        }
        _ => unavailable(e),
    })?;

    // The guard in the INSERT matched nothing to insert. The workflow is
    // DECIDED — checked under its row lock a few statements above, which this
    // transaction still holds — so the only condition that can have refused
    // the row is the transmitted-precommit test.
    let Some(intent_row) = intent_row else {
        return Err(AdmissionError::PrecommitAlreadyTransmitted {
            network: decision.network,
            workflow_id: decision.workflow_id.clone(),
        });
    };

    let intent = WriteIntent {
        intent_id: intent_row.try_get("intent_id").map_err(unavailable)?,
        network: decision.network,
        workflow_id: decision.workflow_id.clone(),
        write_kind: WriteKind::Precommit,
        generation: decision.generation,
        benchmark_id: None,
        payload_digest: decision.payload_digest,
        payload_artifact_id: None,
        state: IntentState::Prepared,
    };

    tx.commit().await.map_err(unavailable)?;

    Ok(Admitted {
        decision_id,
        intent,
        pool_unverified,
        unverified_limit,
    })
}

/// D2e: refuse any anchor that is not the newest usable persisted snapshot.
///
/// "Usable" is `pool.block_snapshot`'s own definition — reads complete and the
/// active-benchmark cache ready — and its partial unique index already makes
/// at most one assembly per block usable, so the ordering below is total.
async fn require_newest_anchor(
    tx: &mut sqlx::PgConnection,
    decision: &NewDecision,
) -> Result<(), AdmissionError> {
    let newest = sqlx::query(
        "SELECT block_id, content_digest
         FROM pool.block_snapshot
         WHERE network = $1 AND reads_complete AND active_cache_ready
         ORDER BY height DESC, block_id DESC
         LIMIT 1",
    )
    .bind(decision.network.as_str())
    .fetch_optional(&mut *tx)
    .await
    .map_err(unavailable)?;

    let Some(newest) = newest else {
        return Err(AdmissionError::NoUsableSnapshot {
            network: decision.network,
        });
    };
    let block_id: String = newest.try_get("block_id").map_err(unavailable)?;
    let digest: Vec<u8> = newest.try_get("content_digest").map_err(unavailable)?;

    if block_id != decision.anchor.block_id || digest != decision.anchor.content_digest {
        return Err(AdmissionError::StaleAnchor {
            network: decision.network,
            offered_block_id: decision.anchor.block_id.clone(),
            newest_block_id: block_id,
        });
    }
    Ok(())
}

/// The authoritative recount of unverified workflows.
///
/// Counted from `pool.workflow`'s open unverified intervals, inside the lease,
/// because §7.6 forbids a cached metric authorizing work.
///
/// `mining_system.md` §6.1 defines the interval and both its ends: a benchmark
/// is unverified "from creation of its pool precommit intent until TIG records
/// it as verified or it reaches a terminal stopped, expired, or failed state".
/// An open interval — `unverified_to_block IS NULL` — is exactly that
/// condition, which is why the count reads the interval rather than inferring
/// one from intent states. Inferring it was the earlier implementation and it
/// was wrong in the safe direction: a workflow that had reached a terminal
/// state went on consuming capacity forever, because a `CONFIRMED` intent
/// never stops being confirmed.
async fn count_unverified(
    tx: &mut sqlx::PgConnection,
    network: Network,
) -> Result<i64, AdmissionError> {
    let row = sqlx::query(
        "SELECT count(*) AS unverified
         FROM pool.workflow
         WHERE network = $1
           AND unverified_to_block IS NULL",
    )
    .bind(network.as_str())
    .fetch_one(&mut *tx)
    .await
    .map_err(unavailable)?;
    row.try_get("unverified").map_err(unavailable)
}

/// Read back what a recorded decision contributes to its §6.1 body.
///
/// The gateway's half of the reconstruction. It holds `SELECT` on
/// `pool.precommit_decision` for exactly this, and nothing here needs the
/// controller's role: the record is immutable once written, so a read at
/// claim time sees what admission wrote.
///
/// `None` when no decision exists for the generation, which for an intent that
/// does exist is a discrepancy — `admit_precommit` writes both in one
/// transaction — and is left to the caller to name.
pub async fn payload_inputs(
    pool: &PgPool,
    network: Network,
    workflow_id: &str,
    generation: i32,
) -> Result<Option<crate::payload::DecisionPayloadInputs>, AdmissionError> {
    let row = sqlx::query(
        "SELECT anchor_block_id, selected_challenge, selected_algorithm,
                compute_type, track_settings
           FROM pool.precommit_decision
          WHERE network = $1 AND workflow_id = $2 AND generation = $3",
    )
    .bind(network.as_str())
    .bind(workflow_id)
    .bind(generation)
    .fetch_optional(pool)
    .await
    .map_err(unavailable)?;

    let Some(row) = row else {
        return Ok(None);
    };
    // A decision written before `compute_type` existed (migration 0013) reads
    // as null. It cannot be rebuilt, and saying so beats rendering a body the
    // intent never digested.
    let compute_type: Option<String> = row.try_get("compute_type").map_err(unavailable)?;
    let Some(compute_type) = compute_type else {
        return Err(AdmissionError::ComputeTypeMissing {
            network,
            workflow_id: workflow_id.to_string(),
            generation,
        });
    };
    Ok(Some(crate::payload::DecisionPayloadInputs {
        anchor_block_id: row.try_get("anchor_block_id").map_err(unavailable)?,
        selected_challenge: row.try_get("selected_challenge").map_err(unavailable)?,
        selected_algorithm: row.try_get("selected_algorithm").map_err(unavailable)?,
        compute_type,
        track_settings: row.try_get("track_settings").map_err(unavailable)?,
    }))
}
