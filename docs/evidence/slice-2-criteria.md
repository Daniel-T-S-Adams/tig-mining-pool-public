# Slice 2: where each acceptance criterion is satisfied

`plans/slice-2-member-agent.md` §8 asks the same of slice 2 that slice 1's §8
asked of it: every criterion in §4 has a passing test, recorded evidence, or an
explicit written waiver by the time the slice closes.

**This file is written as the slice is built, not at the end of it.** Slice 1's
equivalent was assembled when the slice closed, and reconstructing where
ninety-odd criteria were covered — months after each was written — was the
expensive part. A criterion is filled in by the PR that satisfies it, and until
then it says so.

It is in the repository rather than in a PR body for the reason
`evidence/slice-1-live-run.md` gives: a PR body is not in the repository, and
this is the thing a reader reaches for when asking whether a criterion was ever
actually covered.

Test names are given without their module path where the file makes it
unambiguous. Every test named here runs in `make check`; the ones needing a
database skip without `POOL_TEST_SUPERUSER_URL`, which CI sets and `make
db-test` sets locally.

Paths are relative to `crates/` unless they begin with `scripts/`,
`migrations/`, `docs/`, or are a sibling of this file.

## Status

Ninety-four criteria in thirteen groups. This records the ones that are
satisfied now; **an empty cell is an open criterion, not an unrecorded one.**

## A. Pool API surface and request authentication

| # | Where it is satisfied |
|---|---|
| A1 | Configuration half: `pool-config/tests/loading.rs` — `the_member_api_section_is_required_by_pool_api_and_forbidden_elsewhere`, `a_member_api_section_with_an_unusable_value_does_not_load`, `pool_api_must_not_carry_a_tig_endpoint`, `a_login_domain_or_chain_the_pool_would_not_ask_for_is_rejected`, `every_shipped_dev_config_parses_into_the_typed_shape`. Startup half: `pool-api/tests/ticket_key.rs` — a deployment without a usable ticket key does not start. Grant half: every database-backed test in `pool-api` and `pool-admin` connects as the real `pool_api` role, and `migrations/0018`–`0026` grant it no TIG write intent and no workflow change |
| A2 | `pool-api/tests/member_api.rs` — `the_protocol_read_is_public_and_conforms_to_the_pinned_schema`, `the_constants_this_build_serves_are_the_ones_the_schema_pins` |
| A3 | `pool-api/tests/request_verification.rs` — `a_body_that_does_not_match_its_digest_is_refused_before_any_database_work`, with `a_closed_database_is_the_pools_failure_and_not_the_callers` as its control; `nothing_about_the_worker_is_read_until_the_signature_verifies` for the read that verification itself needs. Ordering recorded in `docs/security.md` §4.1 |
| A4 | Freshness: `request_verification.rs` — `a_timestamp_outside_the_window_is_refused_and_the_edges_are_not`, `a_timestamp_at_the_ends_of_its_range_is_refused_rather_than_overflowing`. Replay: `an_identical_retry_is_accepted_and_a_reuse_is_rejected_and_audited`, `one_worker_cannot_spend_another_workers_request_ids`, `a_reuse_that_cannot_be_audited_is_not_refused_permanently`. Schema: `pool-admin/tests/request_replay.rs` (10 tests), `audit_event.rs` (10 tests) |
| A5 | Binding half: `request_verification.rs` — `a_correctly_signed_request_names_the_account_behind_the_worker`, `a_credential_does_not_authorise_another_worker`. **The resource half — that worker B cannot observe whether worker A's assignment exists — waits for the first resource route** |
| A6 | *Open.* Idempotency is implemented per route so far (`enroll::tests::the_same_request_twice_returns_the_same_worker_and_a_changed_one_conflicts`, `two_identical_retries_arriving_together_both_get_the_recorded_result`); the property test over the whole route table waits for the routes |
| A7 | *Open.* |
| A8 | Echo: `request_verification.rs::a_refusal_echoes_the_attempt_it_refused_once_the_header_is_readable`; shape: `member_api.rs::an_error_conforms_with_and_without_the_echoed_request_id`. Non-disclosure: `auth::tests::every_identity_failure_is_the_same_answer`, `request_verification.rs::a_credential_does_not_authorise_another_worker`, `enroll::tests::a_ticket_that_is_not_redeemable_is_refused_the_same_way_as_one_that_never_existed`. **A known limit is recorded in `docs/security.md` §4.1: the answers are identical, the work is not** |

## B. Enrollment, rotation, revocation

| # | Where it is satisfied |
|---|---|
| B1 | Issuance: `pool-api/src/account.rs` tests — `a_first_login_creates_the_account_and_hands_back_one_ticket` (the stored value is the HMAC and not the bearer), `a_ticket_lasts_fifteen_minutes_and_no_longer`, `two_bearers_are_never_the_same`, `a_login_that_fails_partway_spends_nothing`. Redemption in one transaction: `enroll::tests::a_refused_enrollment_leaves_no_worker_and_no_credential`, `one_ticket_buys_exactly_one_worker`, `two_agents_redeeming_one_ticket_produce_one_worker`. Schema: `migrations/0018`'s `enrollment_ticket`, tested in `pool-admin/tests/member_identity.rs` |
| B2 | `enroll::tests` — `a_ticket_buys_a_worker_and_its_first_credential`, `a_key_the_agent_cannot_prove_it_holds_buys_nothing`, `the_same_request_twice_returns_the_same_worker_and_a_changed_one_conflicts`, `two_identical_retries_arriving_together_both_get_the_recorded_result`. Wire shapes: `member_api.rs::an_enrollment_response_conforms_to_the_pinned_schema` |
| B3 | *Open.* |
| B4 | Partly: `pool-identity/tests/wallet.rs::each_signing_string_is_the_text_its_section_specifies` checks the three domains differ, and `enroll::tests::a_recovery_ticket_does_not_enrol_a_worker` checks a recovery ticket does not enrol. **The three cross-domain replays wait for rotation and recovery to exist** |
| B5 | Partly: `request_verification.rs::a_credential_or_worker_that_is_no_longer_active_is_refused` and `a_credential_whose_rotation_grace_has_ended_is_refused` show revocation taking effect on the next request with no cache. **What revocation prevents — offers, heartbeats, events, uploads — waits for those routes** |
| B6 | *Open.* |
| B7 | *Open.* Trust effects have nothing to attach to yet |
| B8 | `pool-identity/tests/wallet.rs` (10 tests) — `the_address_derivation_agrees_with_published_keys_and_addresses`, `a_signature_for_another_pool_or_another_chain_is_refused`, `a_signature_for_another_purpose_or_nonce_is_refused`, `an_expired_signature_is_refused_and_the_edge_is_not`, `a_signature_cannot_be_replayed_with_a_later_expiry`, `a_malformed_signature_is_refused_rather_than_guessed_at`. One-time nonces: `pool-admin/tests/wallet_login_nonce.rs` (7 tests) and `account::tests::the_same_signature_buys_exactly_one_ticket`. Format recorded in `docs/accounting.md` §12.2 |
| B9 | `account::tests` — `two_logins_from_one_wallet_are_one_account` (one wallet, one account; `member_id` stays pool-issued), `a_signature_good_for_longer_than_the_pool_grants_buys_nothing`, `the_nonce_is_remembered_until_the_signature_it_spent_expires`. Key custody: `pool-api/tests/ticket_key.rs`, `ticket_key::tests`, and `scripts/credential-boundary.sh`, which proves from outside the workspace that no other crate can obtain the key. Recorded in `docs/security.md` §4.1 and `docs/architecture.md` §9 |

## C–M

*Open.* Slots and qualification (C), offers and admission (D), assignment
publication (E), events and heartbeats (F), uploads (G), ingestion (H),
publication and durable acceptance (I), the commitment payload (J), the member
agent (K), schema and roles (L), and the closing evidence (M) are filled in by
the PRs that satisfy them.

L1–L3 are substantially in place — `migrations/0018`–`0026` and the `pool_api`
role — but the criteria are written against the routes that use them, so they
are recorded when those land.
