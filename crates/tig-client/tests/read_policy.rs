//! `tig_integration.md` §11: limits, timeouts and retry policy.
//!
//! Driven against a local server whose behaviour each test controls, because
//! the rules being tested are about *how* the client reacts to statuses and
//! headers — something a well-behaved upstream never exercises.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use tig_client::{ReadError, ReadLimits, ReadPolicy, TigReadClient, TigReader};

/// The §11 policy exactly as shipped in `config/tig_integration.json`.
///
/// Loaded rather than hand-written, so these tests cover the pinned values
/// as configured. A fixture here would let the shipped config drift to
/// numbers no test ever loads, which is what criterion E5 exists to close.
fn policy() -> ReadPolicy {
    tig_client::testing::shipped_policy_for_test().expect("the shipped config must parse")
}

fn client(base: impl Into<String>, limits: ReadLimits) -> Result<TigReadClient, String> {
    TigReadClient::new(base, &policy(), limits)
}

fn unrestricted(base: impl Into<String>, limits: ReadLimits) -> Result<TigReadClient, String> {
    TigReadClient::new_unrestricted_for_test(base, &policy(), limits)
}

fn effective_max_backoff(base: &str, limits: ReadLimits) -> Result<Duration, String> {
    tig_client::testing::effective_max_backoff_for_test(base, &policy(), limits)
}

/// A server that fails a controllable number of times before succeeding.
#[derive(Clone, Default)]
struct Flaky {
    calls: Arc<AtomicU32>,
    fail_first: u32,
    status: u16,
    retry_after_raw: Option<String>,
}

async fn serve(flaky: Flaky) -> String {
    let app = Router::new()
        .route(
            "/probe",
            get(|State(f): State<Flaky>| async move {
                let n = f.calls.fetch_add(1, Ordering::SeqCst);
                if n < f.fail_first {
                    let mut headers = HeaderMap::new();
                    if let Some(raw) = f.retry_after_raw.clone() {
                        headers.insert("retry-after", raw.parse().unwrap());
                    }
                    (
                        StatusCode::from_u16(f.status).unwrap(),
                        headers,
                        "upstream unhappy".to_string(),
                    )
                } else {
                    // The success branch carries the header too when one is
                    // configured. It used to hardcode an empty map, which
                    // made any test aiming a Retry-After at a 2xx vacuous:
                    // the header it claimed to send was never sent.
                    let mut headers = HeaderMap::new();
                    if let Some(raw) = f.retry_after_raw.clone() {
                        headers.insert("retry-after", raw.parse().unwrap());
                    }
                    (StatusCode::OK, headers, r#"{"ok":true}"#.to_string())
                }
            }),
        )
        .route("/notjson", get(|| async { "this is not json" }))
        .with_state(flaky);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

fn fast_limits() -> ReadLimits {
    // Timings shrunk so the policy is observable without slow tests; the
    // rules under test are the shape of the behaviour, not the constants.
    //
    // Note the earlier version of this helper set `max_backoff` to 50ms,
    // which was the one range in which a broken jitter calculation happened
    // to be correct — so the suite could not see that every backoff was
    // sub-second under the pinned 60s cap. `backoff_escalates_and_stays_
    // under_the_cap` now covers that range directly.
    ReadLimits {
        max_backoff: Duration::from_millis(50),
        // The pool ceiling rather than a reader share, so the pacing test
        // observes the pinned 2/s rather than one process's half of it.
        // Reached through `testing` because production code must not be
        // able to claim the whole allowance (ADR-0006).
        ..tig_client::testing::pool_ceiling_for_test(&policy())
    }
}

#[test]
fn the_reader_shares_stay_within_the_pinned_ceiling() {
    // §11's budget is per IP and the pool runs more than one reader behind
    // one address, so the shares must sum to no more than the ceiling —
    // otherwise the pinned rate is exceeded by construction.
    // Mapped over TigReader::ALL rather than a hand-written list, so a new
    // reader added without a share fails here instead of silently taking a
    // further full allowance.
    let shares: Vec<ReadLimits> = TigReader::ALL
        .iter()
        .copied()
        .map(|r| policy().for_reader(r))
        .collect();
    assert!(
        policy().shares_within_ceiling(&shares),
        "the per-reader shares exceed the pinned per-IP budget"
    );
}

#[test]
fn the_block_poll_cannot_stall_past_its_own_interval() {
    // §11 polls get-block every 15s and §10 makes a missed height
    // unrecoverable, so this path must fail and retry rather than stall.
    let poll = policy().for_block_poll(TigReader::Controller);
    assert!(
        poll.call_deadline <= Duration::from_secs(15),
        "the poll deadline must not exceed the poll interval"
    );
    // The whole-call deadline is only consulted between attempts, so a
    // per-attempt budget larger than it would let one slow attempt run
    // past the interval regardless. Asserting only the config value above
    // would not have caught that.
    assert!(
        poll.attempt_timeout <= poll.call_deadline,
        "a single attempt could outlast the whole-call deadline"
    );
}

#[tokio::test]
async fn retries_a_server_error_then_succeeds() {
    let flaky = Flaky {
        fail_first: 2,
        status: 503,
        ..Default::default()
    };
    let calls = flaky.calls.clone();
    let base = serve(flaky).await;
    let client = unrestricted(base, fast_limits()).unwrap();

    let value = client.get_json("probe").await.expect("should recover");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(calls.load(Ordering::SeqCst), 3, "two failures then success");
}

#[tokio::test]
async fn gives_up_after_max_attempts_and_reports_it_as_transient() {
    let flaky = Flaky {
        fail_first: u32::MAX,
        status: 500,
        ..Default::default()
    };
    let calls = flaky.calls.clone();
    let base = serve(flaky).await;
    let limits = ReadLimits {
        max_attempts: 3,
        ..fast_limits()
    };
    let client = unrestricted(base, limits).unwrap();

    let err = client.get_json("probe").await.expect_err("must give up");
    assert!(
        err.is_transient(),
        "a 5xx exhaustion is retryable, got {err}"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "exactly max_attempts tries"
    );
}

#[tokio::test]
async fn does_not_retry_a_client_error() {
    // §11: "do not retry schema failures, authentication failures, or other
    // 4xx errors automatically". Repeating a request the server rejected on
    // its contents cannot succeed and only spends quota.
    let flaky = Flaky {
        fail_first: u32::MAX,
        status: 400,
        ..Default::default()
    };
    let calls = flaky.calls.clone();
    let base = serve(flaky).await;
    let client = unrestricted(base, fast_limits()).unwrap();

    let err = client.get_json("probe").await.expect_err("must not retry");
    match err {
        ReadError::Rejected { status, .. } => assert_eq!(status, 400),
        other => panic!("expected Rejected, got {other}"),
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1, "exactly one attempt");
}

#[tokio::test]
async fn retries_429_because_it_is_a_rate_limit_not_a_rejection() {
    let flaky = Flaky {
        fail_first: 1,
        status: 429,
        ..Default::default()
    };
    let calls = flaky.calls.clone();
    let base = serve(flaky).await;
    let client = unrestricted(base, fast_limits()).unwrap();

    client.get_json("probe").await.expect("429 is retryable");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn honours_retry_after_over_its_own_backoff() {
    // §11: "honor `Retry-After` when present". The server's instruction wins
    // over the client's jittered backoff, which would otherwise be shorter.
    let flaky = Flaky {
        fail_first: 1,
        status: 429,
        retry_after_raw: Some("1".to_string()),
        ..Default::default()
    };
    let base = serve(flaky).await;
    // max_backoff above the instructed wait: the cap exists to stop an
    // upstream stalling us indefinitely, not to clip a reasonable one, and
    // this case is about honouring the instruction.
    let roomy = ReadLimits {
        max_backoff: Duration::from_secs(5),
        ..fast_limits()
    };
    let client = unrestricted(base, roomy).unwrap();

    let started = Instant::now();
    client.get_json("probe").await.expect("recovers");
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_secs(1),
        "must have waited the full Retry-After, waited {waited:?}"
    );
}

#[tokio::test]
async fn a_non_json_body_is_a_schema_error_not_a_retry() {
    // §1's fail-closed rule: an unexpected shape stops the caller rather
    // than being retried into the same answer.
    let base = serve(Flaky::default()).await;
    let client = unrestricted(base, fast_limits()).unwrap();

    let err = client.get_json("notjson").await.expect_err("not JSON");
    match err {
        ReadError::Schema { .. } => {}
        other => panic!("expected Schema, got {other}"),
    }
    assert!(
        !err.is_transient(),
        "a schema failure must not look retryable"
    );
}

#[tokio::test]
async fn the_rate_limiter_paces_requests_across_the_process() {
    // §11 pins 2 requests/second with a burst of 2. Six requests therefore
    // cannot complete in under ~2s: the burst covers two, the rest arrive at
    // the sustained rate.
    let base = serve(Flaky::default()).await;
    let client = unrestricted(base, fast_limits()).unwrap();

    let started = Instant::now();
    for _ in 0..6 {
        client.get_json("probe").await.unwrap();
    }
    let elapsed = started.elapsed();

    assert!(
        elapsed >= Duration::from_millis(1900),
        "six requests at 2/s with burst 2 should take about 2s, took {elapsed:?}"
    );
}

#[tokio::test]
async fn backoff_escalates_and_stays_under_a_realistic_cap() {
    // The range the rest of the suite cannot see. With §11's pinned 60s cap
    // an earlier jitter calculation produced only sub-second waits and never
    // escalated, which turns "retry with escalating backoff" into
    // re-hammering an endpoint that is already failing.
    //
    // Asserted statistically because the backoff is jittered by design: over
    // many draws the later attempts must reach well beyond one second, which
    // a sub-second-bounded implementation can never do.
    let cap = Duration::from_secs(60);
    let mut max_seen = Duration::ZERO;
    for attempt in 1..=8u32 {
        for _ in 0..200 {
            let d = tig_client::testing::jitter_below_for_test(cap, attempt);
            assert!(d <= cap, "jitter {d:?} exceeded the cap {cap:?}");
            max_seen = max_seen.max(d);
        }
    }
    assert!(
        max_seen > Duration::from_secs(2),
        "backoff never exceeded {max_seen:?}; it is not escalating across the cap"
    );
}

#[tokio::test]
async fn retries_a_408_and_a_connection_failure() {
    // §11 lists network failure and 408 alongside 429 and 5xx.
    let flaky = Flaky {
        fail_first: 1,
        status: 408,
        ..Default::default()
    };
    let calls = flaky.calls.clone();
    let base = serve(flaky).await;
    let client = unrestricted(base, fast_limits()).unwrap();
    client.get_json("probe").await.expect("408 is retryable");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // Nothing is listening on this port, so every attempt is a transport
    // failure — which must be retried and then reported as transient.
    let limits = ReadLimits {
        max_attempts: 3,
        ..fast_limits()
    };
    let dead = unrestricted("http://127.0.0.1:1", limits).unwrap();
    let err = dead
        .get_json("probe")
        .await
        .expect_err("nothing is listening");
    assert!(
        err.is_transient(),
        "a connection failure is retryable: {err}"
    );
    match err {
        ReadError::Unavailable { attempts, .. } => assert_eq!(attempts, 3),
        other => panic!("expected Unavailable, got {other}"),
    }
}

#[tokio::test]
async fn two_clients_on_one_host_share_the_rate_limit() {
    // §11's limiter is global. Two independently constructed clients — the
    // snapshot ingestor and the active-benchmark cache filler, say — must
    // not each get a full allowance and together double the pinned rate.
    let base = serve(Flaky::default()).await;
    let a = unrestricted(base.clone(), fast_limits()).unwrap();
    let b = unrestricted(base, fast_limits()).unwrap();

    let started = Instant::now();
    for _ in 0..3 {
        a.get_json("probe").await.unwrap();
        b.get_json("probe").await.unwrap();
    }
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1900),
        "six requests across two clients should still take about 2s, took {elapsed:?}"
    );
}

#[tokio::test]
async fn honours_an_http_date_retry_after() {
    // The date form is legal and common from a fronting CDN; ignoring it
    // would silently substitute the client's own shorter backoff.
    // HTTP-date has one-second resolution, so `now + 1s` can format to as
    // little as 0.1s in the future. Use a larger offset and assert against
    // the guaranteed lower bound (offset - 1s) rather than the nominal one.
    let when = httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(3));
    let flaky = Flaky {
        fail_first: 1,
        status: 429,
        retry_after_raw: Some(when),
        ..Default::default()
    };
    let base = serve(flaky).await;
    let roomy = ReadLimits {
        max_backoff: Duration::from_secs(10),
        ..fast_limits()
    };
    let client = unrestricted(base, roomy).unwrap();

    let started = Instant::now();
    client.get_json("probe").await.expect("recovers");
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "must have waited out the HTTP-date Retry-After, waited {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_call_cannot_outlive_its_deadline() {
    // Without a whole-call bound a read can run max_attempts x attempt
    // timeout plus every backoff, outliving the block its snapshot anchors
    // to (§9).
    let flaky = Flaky {
        fail_first: u32::MAX,
        status: 503,
        retry_after_raw: Some("30".to_string()),
        ..Default::default()
    };
    let base = serve(flaky).await;
    let limits = ReadLimits {
        call_deadline: Duration::from_secs(2),
        max_attempts: 10,
        ..fast_limits()
    };
    let client = unrestricted(base, limits).unwrap();

    let started = Instant::now();
    let err = client.get_json("probe").await.expect_err("must stop");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline did not bound the call: {:?}",
        started.elapsed()
    );
    assert!(format!("{err}").contains("deadline"), "got: {err}");
}

#[tokio::test]
async fn a_truncated_body_is_retried_rather_than_failing_once() {
    // §11 retries network failures, and a body that dies mid-transfer is
    // one. Returning at that point made it a single-attempt failure that
    // still reported itself as the retryable variant — the error type and
    // the behaviour disagreeing.
    //
    // Served by declaring more content than is sent, then closing.
    let calls = Arc::new(AtomicU32::new(0));
    let seen = calls.clone();
    let app = Router::new().route(
        "/probe",
        get(move || {
            let calls = calls.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n < 1 {
                    // Content-Length promises more than the body delivers.
                    let mut headers = HeaderMap::new();
                    headers.insert("content-length", "1000".parse().unwrap());
                    (StatusCode::OK, headers, "short".to_string())
                } else {
                    (
                        StatusCode::OK,
                        HeaderMap::new(),
                        r#"{"ok":true}"#.to_string(),
                    )
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = unrestricted(format!("http://{addr}"), fast_limits()).unwrap();
    let value = client
        .get_json("probe")
        .await
        .expect("should retry and recover");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert!(
        seen.load(Ordering::SeqCst) >= 2,
        "the truncated body should have been retried, saw {} call(s)",
        seen.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn clients_disagreeing_on_rate_limits_are_refused() {
    // The limiter is shared per host, so a second client configured more
    // strictly would otherwise run silently at the first one's rate while
    // still reporting its own limits.
    let base = serve(Flaky::default()).await;
    let _first = unrestricted(base.clone(), fast_limits()).unwrap();

    let stricter = ReadLimits {
        requests_per_second: 1,
        ..fast_limits()
    };
    // Through the test door: `new` would reject these limits on entitlement
    // before ever reaching the divergence check, and divergence is what this
    // case is about. (Two entitled shares cannot diverge — they are equal —
    // so the conflict is only reachable this way.)
    let err = unrestricted(base, stricter)
        .expect_err("a divergent rate policy must not be silently ignored");
    assert!(
        err.contains("different rate limits"),
        "the error should explain the conflict, got: {err}"
    );
}

#[tokio::test]
async fn a_host_whose_clients_are_all_gone_accepts_new_limits() {
    // The refusal above exists to keep live clients on one host agreeing.
    // Once no client is left, the registered limits protect nothing — and
    // in a test process the same host:port is routinely reissued by the
    // kernel to an unrelated server, whose client must not be refused for
    // disagreeing with a limiter nobody holds. The rate must still be
    // decided by the newer client, not inherited from the dead entry.
    let base = serve(Flaky::default()).await;
    let first = unrestricted(base.clone(), fast_limits()).unwrap();

    let stricter = ReadLimits {
        requests_per_second: 1,
        ..fast_limits()
    };
    assert!(
        unrestricted(base.clone(), stricter).is_err(),
        "while the first client lives, its limits are the host's limits"
    );

    drop(first);
    let second = unrestricted(base, stricter)
        .expect("with no live client, the host is free to take new limits");
    assert_eq!(
        tig_client::testing::shared_limits_for_test(&second).requests_per_second,
        1,
        "the new client's rate must be its own, not the dead entry's"
    );
}

#[tokio::test]
async fn large_integers_survive_the_read_boundary() {
    // §4 requires lossless numeric handling and accounting.md §3 forbids
    // floating point; a u64 beyond 2^53 routed through f64 would come back
    // changed. Integers are safe at this layer because serde_json stores
    // them as u64 — non-integral numbers are not, and are handled by the
    // pinned models rather than by a workspace-wide serde_json feature.
    let big = "18446744073709551615"; // u64::MAX
    let body = format!(r#"{{"fuel":{big}}}"#);
    let app = Router::new().route(
        "/probe",
        get(move || {
            let body = body.clone();
            async move { body }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = unrestricted(format!("http://{addr}"), fast_limits()).unwrap();
    let value = client.get_json("probe").await.unwrap();
    assert_eq!(
        value["fuel"].to_string(),
        big,
        "a full-width integer must survive the read boundary unchanged"
    );
}

#[tokio::test]
async fn a_hanging_server_cannot_outlast_the_call_deadline() {
    // The behavioural half. A server that accepts the connection and never
    // answers is the shape that defeats a between-attempts-only deadline:
    // nothing fails, so nothing is retried, and the call simply waits.
    let app = Router::new().route(
        "/probe",
        get(|| async {
            tokio::time::sleep(Duration::from_secs(120)).await;
            "never arrives"
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    // Deliberately the UNCLAMPED shape: a per-attempt budget far larger
    // than the whole-call deadline. With them equal, reqwest's own timeout
    // would end the call and the test would pass without exercising the
    // deadline at all — proving nothing about the bug it exists for.
    let limits = ReadLimits {
        call_deadline: Duration::from_secs(2),
        attempt_timeout: Duration::from_secs(30),
        ..tig_client::testing::pool_ceiling_for_test(&policy())
    };
    let client = unrestricted(format!("http://{addr}"), limits).unwrap();

    let started = Instant::now();
    let err = client.get_json("probe").await.expect_err("must not hang");
    let waited = started.elapsed();
    assert!(
        waited < Duration::from_secs(5),
        "the call ran {waited:?}, past its 2s deadline"
    );
    assert!(err.is_transient(), "a timed-out read is retryable: {err}");
}

#[tokio::test]
async fn one_host_spelled_two_ways_shares_a_single_allowance() {
    // §11's budget is per IP, so the limiter's key has to be the host —
    // not the base-URL string. Two spellings hashing apart would each get a
    // full allowance with no divergent-limits error to reveal it.
    let base = serve(Flaky::default()).await;
    let upper = base.replace("http://", "HTTP://");

    let a = unrestricted(base, fast_limits()).unwrap();
    let b = unrestricted(upper, fast_limits()).unwrap();

    let started = Instant::now();
    for _ in 0..3 {
        a.get_json("probe").await.unwrap();
        b.get_json("probe").await.unwrap();
    }
    assert!(
        started.elapsed() >= Duration::from_millis(1900),
        "the two spellings did not share one budget, took {:?}",
        started.elapsed()
    );
}

#[test]
fn a_redundant_default_port_does_not_split_the_budget() {
    // `example.org` and `example.org:443` are one host to a per-IP limiter.
    // Hashing them apart would hand out two full allowances with no
    // divergent-limits error, since the keys never collide.
    assert_eq!(
        tig_client::testing::limiter_key_for_test("https://example.org"),
        tig_client::testing::limiter_key_for_test("https://EXAMPLE.org:443/v1"),
    );
    assert_eq!(
        tig_client::testing::limiter_key_for_test("http://example.org"),
        tig_client::testing::limiter_key_for_test("http://example.org:80"),
    );
    // A non-default port is a genuinely different endpoint.
    assert_ne!(
        tig_client::testing::limiter_key_for_test("https://example.org"),
        tig_client::testing::limiter_key_for_test("https://example.org:8443"),
    );
}

#[tokio::test]
async fn a_trickling_body_cannot_outlast_the_call_deadline() {
    // Headers arrive immediately, then the body dribbles. Bounding only the
    // send left this path able to add a whole attempt_timeout on top of the
    // deadline — about twice the poll interval for a block-poll client.
    //
    // Served from a raw socket rather than axum: the point is a response
    // whose body never completes, which a normal handler cannot express.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                // Promise a body far larger than what is ever sent.
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n")
                    .await;
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if socket.write_all(b"x").await.is_err() {
                        return;
                    }
                }
            });
        }
    });

    let limits = ReadLimits {
        call_deadline: Duration::from_secs(2),
        attempt_timeout: Duration::from_secs(30),
        ..tig_client::testing::pool_ceiling_for_test(&policy())
    };
    let client = unrestricted(format!("http://{addr}"), limits).unwrap();

    let started = Instant::now();
    let err = client
        .get_json("probe")
        .await
        .expect_err("must not stream forever");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the body read ran {:?}, past its 2s deadline",
        started.elapsed()
    );
    assert!(format!("{err}").contains("deadline"), "got: {err}");
}

#[tokio::test]
async fn a_caller_queued_behind_the_token_bucket_still_meets_its_deadline() {
    // The token wait was unbounded, so a block-poll client queued behind
    // the same process's other reads could block past its deadline before
    // the deadline was ever checked.
    let base = serve(Flaky::default()).await;

    // A deliberately slow bucket, so the queue is real.
    let slow = ReadLimits {
        requests_per_second: 1,
        burst: 1,
        max_concurrent: 1,
        call_deadline: Duration::from_secs(30),
        ..tig_client::testing::pool_ceiling_for_test(&policy())
    };
    let hog = unrestricted(base.clone(), slow).unwrap();

    // Drain the burst so the next caller has to wait for a token.
    for _ in 0..3 {
        let _ = hog.get_json("probe").await;
    }

    let impatient = ReadLimits {
        call_deadline: Duration::from_millis(200),
        ..slow
    };
    let client = unrestricted(base, impatient).unwrap();

    let started = Instant::now();
    let result = client.get_json("probe").await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the token wait ran {:?}, past a 200ms deadline",
        started.elapsed()
    );
    // Either it got a token in time or it gave up; it must not hang.
    if let Err(e) = result {
        assert!(e.is_transient(), "a deadline expiry is retryable: {e}");
    }
}

#[tokio::test]
async fn a_client_queued_behind_the_in_flight_slot_still_meets_its_deadline() {
    // With a share of one concurrent GET, a large body holds the only
    // permit for a whole attempt. An unbounded acquire let a block-poll
    // client queue behind it without consulting its own deadline.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\n\r\n")
                    .await;
                loop {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    if socket.write_all(b"x").await.is_err() {
                        return;
                    }
                }
            });
        }
    });

    let base = format!("http://{addr}");
    let one_slot = ReadLimits {
        max_concurrent: 1,
        call_deadline: Duration::from_secs(20),
        attempt_timeout: Duration::from_secs(20),
        ..tig_client::testing::pool_ceiling_for_test(&policy())
    };
    let hog = unrestricted(base.clone(), one_slot).unwrap();
    tokio::spawn(async move {
        let _ = hog.get_json("probe").await;
    });
    // Let the hog take the only slot.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let impatient = ReadLimits {
        call_deadline: Duration::from_millis(300),
        ..one_slot
    };
    let client = unrestricted(base, impatient).unwrap();

    let started = Instant::now();
    let err = client
        .get_json("probe")
        .await
        .expect_err("must not queue forever");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "queued {:?} behind the in-flight slot, past a 300ms deadline",
        started.elapsed()
    );
    assert!(err.is_transient(), "a deadline expiry is retryable: {err}");
}

#[tokio::test]
async fn a_retry_after_pauses_every_reader_on_the_host() {
    // §11's budget is per IP, so a throttle signal is about the host, not
    // the caller. Applying it to one task leaves the others issuing GETs at
    // the pinned rate straight through the instructed wait.
    let flaky = Flaky {
        fail_first: 1,
        status: 429,
        retry_after_raw: Some("2".to_string()),
        ..Default::default()
    };
    let base = serve(flaky).await;

    // max_backoff above the Retry-After, so the cap does not shorten the
    // pause this case is about.
    let roomy = ReadLimits {
        max_backoff: Duration::from_secs(5),
        ..fast_limits()
    };
    let throttled = unrestricted(base.clone(), roomy).unwrap();
    let other = unrestricted(base, roomy).unwrap();

    // The first call receives the 429 and its Retry-After.
    let started = Instant::now();
    let handle = tokio::spawn(async move { throttled.get_json("probe").await });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // A different client on the same host must wait the pause out too.
    other.get_json("probe").await.expect("eventually succeeds");
    let elapsed = started.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1800),
        "the second reader did not observe the shared pause, finished in {elapsed:?}"
    );
    let _ = handle.await;
}

#[tokio::test]
async fn new_refuses_limits_that_are_not_a_readers_share() {
    // ADR-0006's only runtime enforcement. Gating it on the `testing`
    // feature made it inert in every test — the self dev-dependency turns
    // that feature on for the whole test graph — so the guard had no
    // coverage and could not have been exercised. `new` enforces in every
    // build now, and this is what proves it.
    let base = serve(Flaky::default()).await;

    // A share is accepted.
    client(base.clone(), policy().for_reader(TigReader::Controller))
        .expect("a reader's share must be accepted");
    client(base.clone(), policy().for_block_poll(TigReader::Gateway))
        .expect("a block-poll share must be accepted");

    // The whole ceiling, spelled out field by field, is not.
    let hand_built = ReadLimits {
        requests_per_second: 2,
        burst: 2,
        max_concurrent: 2,
        ..policy().for_reader(TigReader::Controller)
    };
    let err = client(base, hand_built).expect_err("a hand-built full ceiling must be refused");
    assert!(
        err.contains("not a reader's share"),
        "the error should explain the entitlement rule, got: {err}"
    );
}

#[tokio::test]
async fn a_retry_after_on_a_success_does_not_pause_the_host() {
    // A 2xx carrying the header is not a throttle signal. Pausing every
    // reader on the host because of one would be a self-inflicted outage.
    let flaky = Flaky {
        fail_first: 0,
        status: 200,
        retry_after_raw: Some("3".to_string()),
        ..Default::default()
    };
    let base = serve(flaky).await;
    let client = unrestricted(base, fast_limits()).unwrap();

    let started = Instant::now();
    client.get_json("probe").await.expect("succeeds");
    client.get_json("probe").await.expect("succeeds again");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a Retry-After on a 200 paused the host, took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_long_retry_after_fails_the_call_rather_than_stalling_it() {
    // §11 says to honour Retry-After, so the pool does not shorten it. An
    // earlier version clamped it to the client's own backoff cap, which
    // meant retrying sooner than the server asked — against a per-IP limit,
    // that prolongs the throttling the instruction exists to relieve.
    //
    // A very long instruction is bounded per CALLER instead: the read fails
    // fast against its own deadline and surfaces, rather than either
    // stalling for a day or disobeying.
    let flaky = Flaky {
        fail_first: u32::MAX,
        status: 429,
        retry_after_raw: Some("86400".to_string()), // a day
        ..Default::default()
    };
    let base = serve(flaky).await;
    let limits = ReadLimits {
        call_deadline: Duration::from_secs(2),
        ..fast_limits()
    };
    let client = unrestricted(base, limits).unwrap();

    let started = Instant::now();
    let err = client
        .get_json("probe")
        .await
        .expect_err("a day-long pause cannot fit a 2s budget");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the call stalled for {:?} instead of failing fast",
        started.elapsed()
    );
    assert!(
        format!("{err}").contains("deadline"),
        "the failure should name the deadline, got: {err}"
    );
    assert!(err.is_transient(), "a deadline expiry is retryable: {err}");
}

#[tokio::test]
async fn a_never_retryable_status_does_not_pause_the_host() {
    // A 404 or 401 is not a throttle signal. Honouring its header would
    // pause every reader over a request that will never be retried.
    let flaky = Flaky {
        fail_first: u32::MAX,
        status: 404,
        retry_after_raw: Some("3".to_string()),
        ..Default::default()
    };
    let base = serve(flaky).await;
    let client = unrestricted(base.clone(), fast_limits()).unwrap();

    // The 404 returns immediately and must leave no pause behind it.
    let _ = client.get_json("probe").await.expect_err("404 is rejected");

    let after = unrestricted(base, fast_limits()).unwrap();
    let started = Instant::now();
    let _ = after.get_json("probe").await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a 404's Retry-After paused the host, next read took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_huge_retry_after_does_not_halt_other_readers_indefinitely() {
    // Two properties, asserted with different instruments because they need
    // different ones.
    //
    // The bound is a value: the host-wide pause is capped at the pinned
    // ceiling, not at the day the server asked for. Sixty seconds is too
    // long to wait out, and a timing assertion at that scale would measure
    // the test's patience rather than the cap.
    let bound = tig_client::testing::host_pause_bound_for_test(&policy());
    assert!(
        bound <= Duration::from_secs(60),
        "the host pause bound is {bound:?}, past §11's pinned ceiling"
    );
    assert!(
        bound < Duration::from_secs(86_400),
        "a day-long instruction would be adopted wholesale"
    );

    // And behaviourally: another reader fails fast against its own deadline
    // rather than hanging, so a paused host surfaces as errors an operator
    // can see instead of silence.
    let flaky = Flaky {
        fail_first: 1,
        status: 429,
        retry_after_raw: Some("86400".to_string()),
        ..Default::default()
    };
    let base = serve(flaky).await;
    let limits = ReadLimits {
        call_deadline: Duration::from_secs(1),
        ..fast_limits()
    };
    let receiver = unrestricted(base.clone(), limits).unwrap();
    let _ = receiver
        .get_json("probe")
        .await
        .expect_err("cannot fit a day");

    let other = unrestricted(base, limits).unwrap();
    let started = Instant::now();
    let err = other
        .get_json("probe")
        .await
        .expect_err("paused, so it fails against its own deadline");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the second reader hung for {:?} instead of failing fast",
        started.elapsed()
    );
    assert!(
        err.is_transient(),
        "a paused-host failure is retryable: {err}"
    );
}

#[tokio::test]
async fn a_client_cannot_widen_the_backoff_ceiling_beyond_policy() {
    // Asserted on the value, not on elapsed time. The first version of this
    // test wrapped the call in a 3s timeout and then asserted it finished
    // within 4s — which the timeout guaranteed, so it passed whether or not
    // the clamp existed.
    let base = serve(Flaky::default()).await;

    let greedy = ReadLimits {
        max_backoff: Duration::from_secs(86_400),
        ..fast_limits()
    };
    let effective = effective_max_backoff(&base, greedy).unwrap();
    assert!(
        effective <= Duration::from_secs(60),
        "a client widened the backoff ceiling to {effective:?}, past §11's pinned 60s"
    );

    // Every pinned duration is clamped, not just the backoff.
    let all_greedy = ReadLimits {
        connect_timeout: Duration::from_secs(600),
        attempt_timeout: Duration::from_secs(600),
        call_deadline: Duration::from_secs(86_400),
        max_backoff: Duration::from_secs(86_400),
        ..fast_limits()
    };
    let client = unrestricted(&base, all_greedy).unwrap();
    let effective = client.limits_for_test();
    assert!(effective.connect_timeout <= Duration::from_secs(5));
    assert!(effective.attempt_timeout <= Duration::from_secs(30));
    assert!(effective.call_deadline <= Duration::from_secs(300));
    assert!(effective.max_backoff <= Duration::from_secs(60));

    // Stricter than policy is still allowed.
    let strict = ReadLimits {
        max_backoff: Duration::from_millis(10),
        ..fast_limits()
    };
    assert_eq!(
        effective_max_backoff(&base, strict).unwrap(),
        Duration::from_millis(10),
        "a client must be allowed to be stricter than the pinned ceiling"
    );
}

// --- criterion E5: the limits are configuration, not compiled constants ---

/// The shipped config with one path replaced, for the negative cases.
fn shipped_json() -> serde_json::Value {
    serde_json::from_str(include_str!("../../../config/tig_integration.json"))
        .expect("the shipped config must parse")
}

#[test]
fn the_shipped_config_carries_every_pinned_limit() {
    // §11's pinned numbers, read back through the loader rather than
    // asserted against compiled copies of themselves. If a value moves in
    // the config, this is where it has to be re-reviewed.
    let policy = policy();
    let ceiling = tig_client::testing::pool_ceiling_for_test(&policy);
    assert_eq!(ceiling.requests_per_second, 2);
    assert_eq!(ceiling.burst, 2);
    assert_eq!(ceiling.max_concurrent, 2);
    assert_eq!(ceiling.connect_timeout, Duration::from_secs(5));
    assert_eq!(ceiling.attempt_timeout, Duration::from_secs(30));
    assert_eq!(ceiling.call_deadline, Duration::from_secs(300));
    assert_eq!(ceiling.max_backoff, Duration::from_secs(60));
    assert_eq!(policy.block_poll_interval(), Duration::from_secs(15));
}

#[test]
fn a_config_without_read_limits_is_refused() {
    // No compiled fallback: §12 forbids a protocol value living in the
    // binary, and a default ceiling would be reached exactly when
    // configuration was missing — the moment least able to notice.
    let mut json = shipped_json();
    json.as_object_mut().unwrap().remove("read_limits");
    let err = ReadPolicy::from_config_json(&json.to_string())
        .expect_err("a config without read_limits must not load");
    assert!(
        format!("{err}").contains("read_limits"),
        "the error should name what is missing, got: {err}"
    );
}

#[test]
fn a_reader_without_a_configured_share_is_refused() {
    // ADR-0006: a reader that silently received a full allowance would
    // double the pool's rate against a per-IP limit, and §10 makes
    // sustained throttling of the get-block poll permanently unrecoverable
    // for per-block attribution. Each reader is checked on its own, so a
    // loader that only looked at the first one still fails here.
    for reader in TigReader::ALL.iter().copied() {
        let mut json = shipped_json();
        json["read_limits"]["reader_shares"]
            .as_object_mut()
            .unwrap()
            .remove(reader.as_str());
        let err = format!(
            "{}",
            ReadPolicy::from_config_json(&json.to_string())
                .expect_err("a reader without a configured share must not load")
        );
        assert!(
            err.contains(reader.as_str()),
            "the error should name the unconfigured reader, got: {err}"
        );
    }
}

#[test]
fn shares_that_exceed_the_ceiling_are_refused_at_load() {
    // §11: "the shares must sum to no more than the ceiling". Refused when
    // the policy is built, so an over-allocating config cannot produce a
    // usable policy at all rather than being caught at some later call.
    let mut json = shipped_json();
    json["read_limits"]["reader_shares"]["controller"]["requests_per_second"] =
        serde_json::json!(2);
    let err = ReadPolicy::from_config_json(&json.to_string())
        .expect_err("over-allocated shares must not load");
    assert!(
        format!("{err}").contains("exceed the pool-wide ceiling"),
        "got: {err}"
    );
}

#[test]
fn an_absent_pinned_limit_is_refused_rather_than_defaulted() {
    // Every ceiling field is required. A dropped one would otherwise be
    // filled by serde's default and the policy would load carrying a value
    // nobody chose — for `max_backoff_seconds`, a zero backoff.
    let mut json = shipped_json();
    json["read_limits"]["pool_ceiling"]
        .as_object_mut()
        .unwrap()
        .remove("max_backoff_seconds");
    let err = ReadPolicy::from_config_json(&json.to_string())
        .expect_err("an absent pinned limit must not load");
    assert!(
        format!("{err}").contains("pool_ceiling"),
        "the error should name the section, got: {err}"
    );
}

#[test]
fn a_misspelt_limit_is_refused_rather_than_ignored() {
    // `deny_unknown_fields`. Without it a typo would parse as an unknown key
    // and be dropped in silence, leaving the intended value unset — the
    // config would then say one thing and the client do another. Asserted
    // separately from the absent-field case above, which the required-field
    // check catches on its own and would pass with `deny_unknown_fields`
    // removed entirely.
    let mut json = shipped_json();
    json["read_limits"]["pool_ceiling"]["max_backoff_secs"] = serde_json::json!(60);
    let err = ReadPolicy::from_config_json(&json.to_string())
        .expect_err("an unrecognised pinned limit must not load");
    assert!(
        format!("{err}").contains("pool_ceiling"),
        "the error should name the section, got: {err}"
    );
}

#[test]
fn the_client_still_refuses_limits_that_are_not_a_configured_share() {
    // The entitlement guard now reads its allowance from configuration; it
    // must still refuse the whole ceiling handed in as a struct literal.
    let policy = policy();
    let ceiling = tig_client::testing::pool_ceiling_for_test(&policy);
    let err = client("http://127.0.0.1:1", ceiling)
        .expect_err("the full ceiling is not a reader's share");
    assert!(err.contains("ADR-0006"), "got: {err}");
}

// --- §11's Cache-Control response cache ---

/// A server that counts requests to one route and answers with a chosen
/// `Cache-Control`.
async fn counting_server(
    route: &'static str,
    cache_control: Option<&'static str>,
) -> (String, Arc<AtomicU32>) {
    let calls = Arc::new(AtomicU32::new(0));
    let seen = calls.clone();
    let app = Router::new().route(
        route,
        get(move || {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let mut headers = HeaderMap::new();
                if let Some(value) = cache_control {
                    headers.insert("cache-control", value.parse().unwrap());
                }
                (StatusCode::OK, headers, r#"{"ok":true}"#.to_string())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), seen)
}

/// A block-addressed read: §11 permits caching this one.
const ANCHORED: &str = "get-challenges?block_id=b1";

#[tokio::test]
async fn the_latest_block_is_never_served_from_cache() {
    // The one that matters. §9 issues this identical URL for a snapshot's
    // opening and closing read, so a cached body would make step 6's
    // comparison a tautology and let a snapshot spanning a block boundary
    // be accepted as block-consistent — and it would freeze the §11 poll,
    // skipping the heights §10 records as an unrecoverable gap.
    //
    // The server offers a long lifetime; scope refuses it regardless.
    let (base, calls) = counting_server("/get-block", Some("max-age=600")).await;
    let client = client(base, policy().for_reader(TigReader::Controller)).unwrap();

    client
        .get_json("get-block?include_data=true")
        .await
        .unwrap();
    client
        .get_json("get-block?include_data=true")
        .await
        .unwrap();
    client
        .get_json("get-block?include_data=true")
        .await
        .unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "every get-block must reach the server"
    );
}

#[tokio::test]
async fn a_cacheable_response_is_not_requested_again() {
    // §11: "The live Cache-Control header is respected." The point is not
    // the copy, it is the request that never happens — asserted with a
    // server-side counter rather than by timing.
    let (base, calls) = counting_server("/get-challenges", Some("max-age=60")).await;
    let client = client(base, policy().for_reader(TigReader::Controller)).unwrap();

    let first = client.get_json(ANCHORED).await.unwrap();
    let second = client.get_json(ANCHORED).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the second read must be served from cache"
    );
}

#[tokio::test]
async fn a_response_without_cache_control_is_never_cached() {
    // No header means no permission, even for a request §11 would allow.
    // Inventing a lifetime for a live protocol value is how §12's rule
    // against compiled constants gets broken by the back door.
    let (base, calls) = counting_server("/get-challenges", None).await;
    let client = client(base, policy().for_reader(TigReader::Controller)).unwrap();

    client.get_json(ANCHORED).await.unwrap();
    client.get_json(ANCHORED).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn no_store_and_zero_max_age_are_honoured() {
    for header in ["no-store", "no-cache", "max-age=0", "max-age=60, no-store"] {
        let leaked: &'static str = Box::leak(header.to_string().into_boxed_str());
        let (base, calls) = counting_server("/get-challenges", Some(leaked)).await;
        let client = client(base, policy().for_reader(TigReader::Controller)).unwrap();
        client.get_json(ANCHORED).await.unwrap();
        client.get_json(ANCHORED).await.unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "{header} must not be cached"
        );
    }
}

#[tokio::test]
async fn an_error_is_never_cached_as_successful_data() {
    // §11: "errors and incomplete snapshots are not cached as successful
    // data". A 4xx carrying a long max-age must not become a stored answer.
    let calls = Arc::new(AtomicU32::new(0));
    let seen = calls.clone();
    let app = Router::new().route(
        "/get-challenges",
        get(move || {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let mut headers = HeaderMap::new();
                headers.insert("cache-control", "max-age=600".parse().unwrap());
                (StatusCode::BAD_REQUEST, headers, "nope".to_string())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = client(
        format!("http://{addr}"),
        policy().for_reader(TigReader::Controller),
    )
    .unwrap();
    assert!(client.get_json(ANCHORED).await.is_err());
    assert!(client.get_json(ANCHORED).await.is_err());
    assert_eq!(
        seen.load(Ordering::SeqCst),
        2,
        "an error must not be served from cache"
    );
}

#[tokio::test]
async fn a_cache_hit_consumes_no_rate_limit_budget() {
    // A hit makes no request, so it must not take a token or an in-flight
    // slot. Asserted by making the budget so small that a second real
    // request would be paced: the pair completes promptly because only one
    // request happens.
    let (base, calls) = counting_server("/get-challenges", Some("max-age=60")).await;
    let slow = ReadLimits {
        requests_per_second: 1,
        burst: 1,
        ..policy().for_reader(TigReader::Controller)
    };
    let client = client(base, slow).unwrap();

    client.get_json(ANCHORED).await.unwrap();
    let started = Instant::now();
    for _ in 0..5 {
        client.get_json(ANCHORED).await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "five cache hits waited on the limiter: {:?}",
        started.elapsed()
    );
}
