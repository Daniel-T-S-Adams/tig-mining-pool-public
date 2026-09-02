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
use tig_client::{ReadError, ReadLimits, TigReadClient, TigReader};

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
        ..tig_client::testing::pool_ceiling_for_test()
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
        .map(ReadLimits::for_reader)
        .collect();
    assert!(
        ReadLimits::shares_within_ceiling(&shares),
        "the per-reader shares exceed the pinned per-IP budget"
    );
}

#[test]
fn the_block_poll_cannot_stall_past_its_own_interval() {
    // §11 polls get-block every 15s and §10 makes a missed height
    // unrecoverable, so this path must fail and retry rather than stall.
    let poll = ReadLimits::for_block_poll(TigReader::Controller);
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
    let client = TigReadClient::new_unrestricted_for_test(base, fast_limits()).unwrap();

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
    let client = TigReadClient::new_unrestricted_for_test(base, limits).unwrap();

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
    let client = TigReadClient::new_unrestricted_for_test(base, fast_limits()).unwrap();

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
    let client = TigReadClient::new_unrestricted_for_test(base, fast_limits()).unwrap();

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
    let client = TigReadClient::new_unrestricted_for_test(base, roomy).unwrap();

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
    let client = TigReadClient::new_unrestricted_for_test(base, fast_limits()).unwrap();

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
    let client = TigReadClient::new_unrestricted_for_test(base, fast_limits()).unwrap();

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
    let client = TigReadClient::new_unrestricted_for_test(base, fast_limits()).unwrap();
    client.get_json("probe").await.expect("408 is retryable");
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // Nothing is listening on this port, so every attempt is a transport
    // failure — which must be retried and then reported as transient.
    let limits = ReadLimits {
        max_attempts: 3,
        ..fast_limits()
    };
    let dead = TigReadClient::new_unrestricted_for_test("http://127.0.0.1:1", limits).unwrap();
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
    let a = TigReadClient::new_unrestricted_for_test(base.clone(), fast_limits()).unwrap();
    let b = TigReadClient::new_unrestricted_for_test(base, fast_limits()).unwrap();

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
    let client = TigReadClient::new_unrestricted_for_test(base, roomy).unwrap();

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
    let client = TigReadClient::new_unrestricted_for_test(base, limits).unwrap();

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

    let client =
        TigReadClient::new_unrestricted_for_test(format!("http://{addr}"), fast_limits()).unwrap();
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
    let _first = TigReadClient::new_unrestricted_for_test(base.clone(), fast_limits()).unwrap();

    let stricter = ReadLimits {
        requests_per_second: 1,
        ..fast_limits()
    };
    // Through the test door: `new` would reject these limits on entitlement
    // before ever reaching the divergence check, and divergence is what this
    // case is about. (Two entitled shares cannot diverge — they are equal —
    // so the conflict is only reachable this way.)
    let err = TigReadClient::new_unrestricted_for_test(base, stricter)
        .expect_err("a divergent rate policy must not be silently ignored");
    assert!(
        err.contains("different rate limits"),
        "the error should explain the conflict, got: {err}"
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

    let client =
        TigReadClient::new_unrestricted_for_test(format!("http://{addr}"), fast_limits()).unwrap();
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
        ..tig_client::testing::pool_ceiling_for_test()
    };
    let client =
        TigReadClient::new_unrestricted_for_test(format!("http://{addr}"), limits).unwrap();

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

    let a = TigReadClient::new_unrestricted_for_test(base, fast_limits()).unwrap();
    let b = TigReadClient::new_unrestricted_for_test(upper, fast_limits()).unwrap();

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
        ..tig_client::testing::pool_ceiling_for_test()
    };
    let client =
        TigReadClient::new_unrestricted_for_test(format!("http://{addr}"), limits).unwrap();

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
        ..tig_client::testing::pool_ceiling_for_test()
    };
    let hog = TigReadClient::new_unrestricted_for_test(base.clone(), slow).unwrap();

    // Drain the burst so the next caller has to wait for a token.
    for _ in 0..3 {
        let _ = hog.get_json("probe").await;
    }

    let impatient = ReadLimits {
        call_deadline: Duration::from_millis(200),
        ..slow
    };
    let client = TigReadClient::new_unrestricted_for_test(base, impatient).unwrap();

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
        ..tig_client::testing::pool_ceiling_for_test()
    };
    let hog = TigReadClient::new_unrestricted_for_test(base.clone(), one_slot).unwrap();
    tokio::spawn(async move {
        let _ = hog.get_json("probe").await;
    });
    // Let the hog take the only slot.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let impatient = ReadLimits {
        call_deadline: Duration::from_millis(300),
        ..one_slot
    };
    let client = TigReadClient::new_unrestricted_for_test(base, impatient).unwrap();

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
    let throttled = TigReadClient::new_unrestricted_for_test(base.clone(), roomy).unwrap();
    let other = TigReadClient::new_unrestricted_for_test(base, roomy).unwrap();

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
    TigReadClient::new(base.clone(), ReadLimits::for_reader(TigReader::Controller))
        .expect("a reader's share must be accepted");
    TigReadClient::new(base.clone(), ReadLimits::for_block_poll(TigReader::Gateway))
        .expect("a block-poll share must be accepted");

    // The whole ceiling, spelled out field by field, is not.
    let hand_built = ReadLimits {
        requests_per_second: 2,
        burst: 2,
        max_concurrent: 2,
        ..ReadLimits::for_reader(TigReader::Controller)
    };
    let err = TigReadClient::new(base, hand_built)
        .expect_err("a hand-built full ceiling must be refused");
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
    let client = TigReadClient::new_unrestricted_for_test(base, fast_limits()).unwrap();

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
    let client = TigReadClient::new_unrestricted_for_test(base, limits).unwrap();

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
    let client = TigReadClient::new_unrestricted_for_test(base.clone(), fast_limits()).unwrap();

    // The 404 returns immediately and must leave no pause behind it.
    let _ = client.get_json("probe").await.expect_err("404 is rejected");

    let after = TigReadClient::new_unrestricted_for_test(base, fast_limits()).unwrap();
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
    let bound = tig_client::testing::host_pause_bound_for_test();
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
    let receiver = TigReadClient::new_unrestricted_for_test(base.clone(), limits).unwrap();
    let _ = receiver
        .get_json("probe")
        .await
        .expect_err("cannot fit a day");

    let other = TigReadClient::new_unrestricted_for_test(base, limits).unwrap();
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
    let effective = tig_client::testing::effective_max_backoff_for_test(&base, greedy).unwrap();
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
    let client = TigReadClient::new_unrestricted_for_test(&base, all_greedy).unwrap();
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
        tig_client::testing::effective_max_backoff_for_test(&base, strict).unwrap(),
        Duration::from_millis(10),
        "a client must be allowed to be stricter than the pinned ceiling"
    );
}
