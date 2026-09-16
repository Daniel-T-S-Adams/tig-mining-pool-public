# Slice 1: where each acceptance criterion is satisfied

`plans/slice-1-gateway.md` §8 asks that every criterion in its §4 have a passing
test, recorded evidence, or an explicit written waiver by the time the slice
closes. This is that record: all 61, one line each.

It is in the repository rather than in the closing PR's body for the reason
`evidence/slice-1-live-run.md` gives — a PR body is not in the repository, and
this is the thing a reader reaches for when asking whether a criterion was ever
actually covered.

Test names are given without their module path where the file makes it
unambiguous. Every test named here runs in `make check`; the ones needing a
database skip without `POOL_TEST_SUPERUSER_URL`, which CI sets.

Paths are relative to `crates/` unless they begin with `scripts/`,
`fixtures/`, `migrations/`, `.github/`, or are a sibling of this file.

## A. Configuration and identity

| # | Where it is satisfied |
|---|---|
| A1 | `pool-config/tests/loading.rs` — `valid_config_loads`, `unknown_field_is_rejected`, `missing_file_is_reported_as_a_read_error`, `every_shipped_dev_config_parses_into_the_typed_shape`; exit behaviour in `pool-admin/tests/cli.rs` — `a_missing_config_file_exits_non_zero`, `an_unreachable_database_exits_non_zero_promptly` |
| A2 | `loading.rs` — `missing_network_is_rejected`, `mainnet_is_rejected_in_this_build`, `unrecognised_network_is_rejected`, `the_endpoint_cannot_be_pointed_at_mainnet_or_anywhere_unpinned` |
| A3 | `loading.rs` — `decision_digest_is_stable_and_excludes_non_decision_fields`, `the_offer_the_pool_decides_for_enters_the_digest`; stored on every decision by `migrations/0005`'s `config_digest NOT NULL`, written in `pool-workflow/src/decision.rs` and wired from the live config at `pool-controller/src/main.rs:176` |
| A4 | `scripts/secret-scan.sh` (with `--selftest`) in `make check`; `loading.rs::config_debug_output_carries_no_password`, `cli.rs::no_log_line_contains_the_configured_password`, `loading.rs::a_rejected_endpoint_never_echoes_its_userinfo` |

## B. TIG write-readiness gate

| # | Where it is satisfied |
|---|---|
| B1 | `tig-gateway/tests/write_ready.rs` — `all_nine_passing_is_the_only_route_to_write_ready`, `every_failure_is_reported_not_just_the_first` |
| B2 | `write_ready.rs` — one or more `check_N_*` tests per check, nine checks, 23 tests, including `check_4_a_mutated_openapi_checksum`; the evidence-gathering half in `tig-gateway/tests/evidence.rs` (11 tests), including `check_4_hashes_what_was_served_and_fails_when_it_cannot_look` |
| B3 | `tig-gateway/tests/write_gate.rs` — `losing_write_ready_blocks_new_writes`, `an_in_flight_write_survives_revocation`, `recovery_requires_passing_all_nine_checks_again`; the alert in `write_gate_alerts.rs::losing_write_ready_alerts_once_and_recovery_is_recorded` |

## C. Block snapshot

| # | Where it is satisfied |
|---|---|
| C1 | `pool-snapshot/tests/assembly.rs` (15 tests) — `a_block_advancing_mid_assembly_discards_the_whole_snapshot`, `every_read_is_anchored_to_the_block_that_opened_the_snapshot` |
| C2 | `pool-snapshot/tests/store.rs` — `an_accepted_snapshot_is_persisted_with_its_completeness_status`, `an_incomplete_snapshot_is_recorded_but_yields_no_decision`, `a_different_snapshot_for_the_same_block_is_refused` |
| C3 | `assembly.rs` — `the_cache_keys_on_the_complete_request_not_the_endpoint`, `the_cache_is_dropped_whole_when_the_block_changes`; `against_fake_tig.rs::a_second_assembly_at_the_same_block_is_served_from_the_cache` |
| C4 | `scripts/no-observed-constants.sh` (with `--selftest`) in `make check` — 33 values and 47 field names checked across 65 files |
| C5 | `store.rs::complete_reads_serve_reconciliation_before_the_active_cache_is_ready`; `pool-controller/tests/decide.rs::a_snapshot_that_is_not_decision_usable_decides_nothing`; `service.rs::a_blind_pass_decides_nothing_even_with_an_offer_configured` |

## D. Write intents and idempotency

| # | Where it is satisfied |
|---|---|
| D1 | `pool-workflow/tests/intents.rs` — `concurrent_duplicate_intents_leave_exactly_one_row`, `the_same_key_with_a_different_payload_is_refused` |
| D1a | `intents.rs` — `a_generation_cannot_be_reused_across_a_different_benchmark`, `the_benchmark_binding_is_required_and_refused_by_kind`, `the_schema_refuses_the_binding_even_without_the_repository` |
| D2 | `pool-workflow/tests/admission.rs` — `a_decision_and_its_intent_commit_together`, `concurrent_admissions_cannot_both_take_the_last_slot`, `admission_blocks_on_the_serialized_lease` |
| D2a | `admission.rs::the_next_precommit_at_the_limit_is_refused`, `decide.rs::the_unverified_limit_refuses_the_pass_that_would_exceed_it`, `loading.rs::{a_controller_without_an_unverified_limit_does_not_load, a_zero_unverified_limit_is_rejected}` |
| D2b | Deferred half, by construction: the criterion fixes the transaction's *shape* so the member half slots in without reshaping it. The shape is what D2, D2a and D2c's tests hold, and F6's owner and interval rows are what the deferred half will read. Nothing is waived — there is no member to test against until checklist §10 steps 6–8 |
| D2c | `decide.rs::an_unreadable_penalty_or_charge_stops_the_pass_rather_than_reserving_less`; `loading.rs::the_reserve_policy_values_are_checked_rather_than_carried`; `admission.rs::a_non_canonical_reserve_never_reaches_the_numeric_cast` |
| D2d | `pool-decision/tests/challenge_selection.rs` — `two_way_tie_resolved_by_supplied_draw_ranks`, `projected_tie_resolved_by_supplied_draw_ranks`, `all_zero_counts_tie_at_factor_zero`, `a_tie_without_a_supplied_rank_is_refused`; the derivation itself in `pool-domain/tests/challenge_tie_vector.rs::section_6_3_worked_example_vector` and `the_seed_enters_the_rank_as_bytes_not_as_text`; persisted by `admission.rs::a_tie_records_its_candidates_and_winner` |
| D2e | `admission.rs` — `an_incomplete_snapshot_is_not_an_anchor`, `a_decision_cannot_name_a_snapshot_that_was_never_persisted`, `a_pool_with_no_usable_snapshot_cannot_decide` |
| D3 | `intents.rs::a_new_generation_is_how_a_payload_changes`; `admission.rs::a_workflow_whose_precommit_reached_tig_cannot_take_another_generation`; `tig-gateway/tests/claim.rs::a_generation_whose_sibling_was_sent_stops_for_an_operator` |
| D4 | `intents.rs::the_gateway_records_outcomes_but_never_invents_a_write`; `workflow_state.rs::the_gateway_can_read_a_workflow_and_cannot_change_one`; `admission.rs::the_gateway_can_read_a_decision_and_cannot_write_one` — all run under the real gateway role |
| D5 | `scripts/credential-boundary.sh` in `make check`: the key-loading path is crate-private to `tig-gateway`, and no crate or script outside it names the key — **outside the spike**. `crates/spike` is excluded deliberately (it predates the boundary and reads the testnet key), as are `crates/fake-tig`, which checks an inbound key rather than holding one, and the key-scanners themselves. The spike's read is a known gap tracked by `plans/slice-1-gateway.md` §7, which slice 1 does not clear |

## E. Attempts, the lane, and reconciliation

| # | Where it is satisfied |
|---|---|
| E1 | `pool-workflow/tests/attempts.rs::an_attempt_is_recorded_before_the_response_and_separately_from_it`; `tig-gateway/src/drive.rs::a_prepared_intent_is_sent_once_and_its_attempt_recorded_around_the_send` |
| E2 | `attempts.rs` — `only_one_precommit_may_be_unresolved_in_the_lane`, `an_ambiguous_precommit_keeps_the_lane_closed_until_reconciliation`, `an_ambiguous_attempt_reports_its_lane_as_occupied` |
| E3 | `attempts.rs::an_ambiguous_outcome_moves_the_intent_in_the_same_transaction`; `claim.rs::a_lost_response_whose_write_landed_is_settled_and_not_resent` |
| E4 | `tig-gateway/tests/reconcile.rs` (14 tests) — `every_field_of_the_tuple_is_matched`, `two_candidates_stop_for_operator_resolution`, `a_single_unconfirmed_candidate_is_not_absence`, `an_unreadable_record_is_fatal_rather_than_skipped` |
| E5 | `tig-client/tests/read_policy.rs` (44 tests) — limits, `Retry-After`, and the no-retry classes |
| E6 | `attempts.rs::two_writes_for_one_benchmark_cannot_be_in_flight` |

## F. Confirmation-driven state machine

| # | Where it is satisfied |
|---|---|
| F1 | `pool-workflow/tests/workflow_state.rs::a_workflow_advances_only_through_confirmed_evidence`, `a_submission_is_recorded_and_is_not_a_confirmation`; `claim.rs::being_in_the_read_is_not_being_confirmed` |
| F2 | `workflow_state.rs::confirmed_settings_replace_the_proposed_ones` |
| F3 | `workflow_state.rs` — `a_transition_from_a_stale_revision_is_refused`, `the_revision_can_never_go_backwards` |
| F4 | `pool-controller/tests/lifecycle.rs`, driving **six workflow ladders across five** of `fixtures/queue-lifecycle/v1/lifecycle.json`'s nine cases — both counts pinned in the test, so a case that stopped being driven fails it. The other four are waived below. The ladders run from the fixture against a constructed `ConfirmedWindow`, not against fake-tig; that the window is what a real server's reads produce is `pool-controller/tests/window_against_fake_tig.rs`. Also `workflow_state.rs::a_stopped_benchmark_never_reaches_a_proof`, `fraud_is_terminal_from_wherever_it_is_found` |
| F4a | `pool-workflow/tests/acceptance.rs` (9 tests) — `a_benchmark_write_cannot_exist_before_durable_acceptance`, `a_proof_write_cannot_exist_before_a_canonical_payload_for_the_sample` |
| F4b | `workflow_state.rs` — `no_slice_1_workflow_is_attributable_to_a_member`, `a_terminal_reason_may_not_attribute_member_fault` |
| F4c | `scripts/feature-gate.sh` in `make check`: the stub acceptance record is absent from a default `pool-controller` build |
| F4d | `pool-controller/tests/stub_gate.rs` (5 tests) — `the_stub_refuses_a_live_endpoint_and_writes_nothing`, `a_stub_row_says_it_is_one` |
| F5 | `workflow_state.rs` — `a_workflow_expires_at_the_guardrail_and_records_why`, `the_expiry_reason_is_a_bounded_code`, and the six sweep tests around them; `tick.rs::an_unsent_decision_expires_when_the_guardrail_passes_and_not_before` |
| F6 | `admission.rs` — `admission_creates_the_workflow_with_its_owner_and_open_interval`, `an_intent_cannot_exist_without_its_owner_mapping`, `a_workflow_cannot_exist_without_an_open_interval`; `workflow_state.rs::a_mainnet_workflow_cannot_be_pool_owned` |
| F6a | `workflow_state.rs` — `one_workflow_owns_one_benchmark`, `a_workflow_cannot_be_re_pointed_at_another_benchmark` |

## G. Restart and crash recovery

| # | Where it is satisfied |
|---|---|
| G1 | `pool-workflow/tests/restart.rs` (18 tests); `pool-controller/tests/tick.rs::the_restart_pass_keeps_its_own_say`, `a_failed_expiry_sweep_blocks_claiming` |
| G2 | The four `architecture.md` §12 rows in `tig-gateway/src/drive.rs`'s test module, each with the fake's server-side write count; the fourth row's controller half in `tick.rs::a_crash_after_tig_changed_state_is_recovered_by_the_controller_monotonically`. Repeated live in K4 |
| G3 | `pool-workflow/tests/lease.rs` — `a_reclaim_takes_a_higher_fence_and_the_old_owner_cannot_commit`, `the_fence_can_never_go_backwards`, `a_missing_lease_row_is_a_lost_fence_not_a_free_pass` |
| G4 | `restart.rs::every_missing_height_is_recorded_as_its_own_gap`, `a_gap_record_can_only_ever_be_resolved`; `tick.rs::missed_blocks_are_recorded_as_a_gap_once`, `the_gap_is_recorded_before_the_snapshot_that_revealed_it` |
| G5 | `restart.rs::an_accepted_attempt_with_no_confirmation_advances_nothing`; `claim.rs::a_sent_write_the_search_cannot_account_for_stops_rather_than_resending` |

## H. Credential boundary

| # | Where it is satisfied |
|---|---|
| H1 | `write_ready.rs::check_8_the_api_key_missing_or_readable_by_members`; `evidence.rs::check_8_reads_who_can_open_the_key_file_from_its_mode` |
| H2 | `scripts/credential-boundary.sh` in `make check`, which also scans `scripts/` |
| H3 | `scripts/secret-scan.sh` (A4's scan), plus `pool-config/tests/loading.rs::config_debug_output_carries_no_password` and `pool-admin/tests/cli.rs::no_log_line_contains_the_configured_password` |

## I. Observability

| # | Where it is satisfied |
|---|---|
| I1 | `pool-telemetry/tests/span_fields.rs::root_span_fields_survive_nesting`; `pool-admin/tests/cli.rs::every_log_line_carries_service_deployment_and_network` |
| I2 | **Waived** — deferred to checklist §10 step 5 (below) |
| I3 | `intents.rs::every_column_of_the_table_reaches_the_typed_intent` (the `trace_id` column reaches every statement that returns an intent); `drive.rs::the_admitting_trace_reaches_the_gateway_through_the_row`, `an_intent_admitted_without_a_trace_is_still_driven` |
| I4 | **Waived** — deferred to checklist §10 step 5 with I2 (below) |

## J. Schema and migrations

| # | Where it is satisfied |
|---|---|
| J1 | `pool-admin/tests/migrate.rs` — `migrations_apply_to_an_empty_database`, `migrations_are_ordered_and_unique`, `an_edited_applied_migration_is_refused`, `migrate_serialises_on_its_operation_lock` |
| J2 | `migrate.rs::each_migration_applies_on_top_of_its_predecessors` |
| J3 | `migrate.rs::service_roles_cannot_create_objects_in_the_pool_schema`, `the_monitoring_role_gets_no_blanket_visibility_and_never_writes`; D4's grant tests run under the real roles |
| J4 | **Waived** — the property holds by construction; the measurement is deferred (below) |

## K. Evidence required to call the slice done

| # | Where it is satisfied |
|---|---|
| K1 | `make check` — fmt, clippy `-D warnings`, 75 test suites, and the feature-gate, credential-boundary, secret-scan, image-pin, no-observed-constants and live-run-evidence selftests. Run on every PR by `.github/workflows/pr-checks.yml` |
| K2 | `pool-controller/tests/lifecycle.rs`, six ladders across five of the nine fixture cases, deterministic and run in CI (four waived, below). **One divergence from the criterion as written:** it says "against fake-tig", and the ladders are driven from a constructed `ConfirmedWindow` instead. What fake-tig covers is the step before — `window_against_fake_tig.rs::each_confirmed_state_a_real_server_serves_reaches_the_window` and `a_fraud_ruling_a_real_server_serves_reaches_the_window` assert that a real server's responses produce exactly that window, and `tick.rs` drives the controller against fake-tig end to end |
| K3 | [`slice-1-live-run.md`](slice-1-live-run.md) — the run's block heights, intent and attempt rows |
| K4 | [`slice-1-live-run.md`](slice-1-live-run.md), produced by `scripts/live-crash-test.sh` |
| K5 | The spike-test disposition table in `plans/slice-1-gateway.md` §7 |

## Waivers

`plans/slice-1-gateway.md` §8 allows a criterion to close on "an explicit
written waiver in the PR that closes the slice". Four did. `CLAUDE.md` makes
weakening a test a human-only decision, so each names where the decision is
recorded.

| # | What is waived | Where it lands | Decision |
|---|---|---|---|
| I2 | The metrics exporter | Checklist §10 step 5 | Plan §4, merged in #43 — nothing consumes metrics today, so an exporter built here would be written against nothing |
| I4 | The alert tests | Checklist §10 step 5, with I2 | Plan §4, merged in #43 — with no alerting system the test can only assert that a log line was written |
| K2 | Three of `lifecycle.json`'s nine cases: `duplicate_capacity_offer_request`, `duplicate_durable_acceptance_receipt`, `member_package_timeout_failed` | Checklist §10 step 2 | Plan §4, merged in #43 — all three exercise the member API and artifact upload, which §2 defers to that step |
| K2 | A fourth case, `fraud_confirmed_after_proof` | Checklist §10 step 2 | Found while writing this record; see below |
| J4 | The transaction-duration **test**, not §7.2's invariant | Checklist §10 step 5 | Issue #50, where the repository owner's decision is recorded verbatim |

`fraud_confirmed_after_proof` was not in the three the plan already recorded,
and it is listed here because the record is the place a gap has to appear
rather than be absorbed. It is deferred for the same reason as the other three
and by the same rule, not by a new judgement: its ladder begins in `VERIFYING`,
which slice 1 cannot reach without artifacts to build a proof from, so
`lifecycle.rs`'s computed scope filter drops the case whole. Its rule — §7's
"fraud confirmed" and §4.5's terminal branch — is covered from the other side
by `workflow_state.rs::fraud_is_terminal_from_wherever_it_is_found` and
`window_against_fake_tig.rs::a_fraud_ruling_a_real_server_serves_reaches_the_window`;
what waits for step 2 is the fixture case's own ladder, and the fault
attribution and charge that F4b forbids this slice from evaluating at all.

J4 is the only one of the five that waives a test which could be written today,
which is why it carries its own record. The property it tests holds by
construction — `PostgresAttemptLedger::begin_fenced` commits before it returns
and `drive::handle` calls the transmitter only afterwards, so no transaction is
open across the send — and what is deferred is the *measurement*, which needs
response-delay injection in `fake-tig` that does not exist. The risk carried is
that a later change could open a transaction across a send without the structure
making it obvious; until step 5, §7.2's invariant and this note are what a
reviewer has.
