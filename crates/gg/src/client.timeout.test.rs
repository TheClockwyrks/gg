//! The two clocks that bound one model call — the [per-attempt
//! ceiling](DEFAULT_MODEL_CALL_TIMEOUT) and the [stream-idle
//! bound](DEFAULT_MODEL_STREAM_IDLE) — against a **stalled gateway**: one that accepts the request
//! and never answers it again, which is exactly the failure that once held a run twenty minutes
//! inside one call with nothing able to interrupt it — and against its opposite, a **slow first
//! delta**: a model that thinks without streaming, or a provider that sends its reply in one piece,
//! which the idle bound must never cut, because it is unarmed until the reply's first delta.
//!
//! Each test states the figure its client was built with, so what is asserted is the bound the
//! run configured rather than one gg picked — except the ones that build their client with no
//! figure at all, which assert gg's own defaults.
//!
//! Every test here runs under `start_paused` time, so a bound elapses the moment the runtime has
//! nothing left to do rather than after the bound's figure in real seconds, and the paused clock
//! reads exactly the duration that cut the call — which is what shows it was the configured one.
//!
//! The gateway is a stand-in the client is [answered by](OpenRouterClient::answered_by) rather than
//! a socket: a response whose body is exactly the chunks the test chooses, ready at once, and then
//! a stream that never yields again. A real loopback socket cannot be combined with a paused clock
//! deterministically — whether the client has read what the gateway wrote before the runtime next
//! idles is up to the scheduler, and when it has not, the clock jumps. What the stand-in replaces is
//! reqwest's socket; everything from the response onward is gg's own path, the one a real reply
//! takes.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use crate::model::Message;
use crate::telemetry::{CollectingSink, Emitter};

/// The [per-attempt ceiling](DEFAULT_MODEL_CALL_TIMEOUT) the tests configure: unlike the default,
/// so a call cut by the default instead reads differently on the paused clock.
const CONFIGURED: Duration = Duration::from_secs(37);

/// The [stream-idle bound](DEFAULT_MODEL_STREAM_IDLE) the tests configure: unlike the default, for
/// the same reason — and short enough that a stall is spent quickly on the paused clock.
const CONFIGURED_IDLE: Duration = Duration::from_secs(11);

/// The one SSE event a stalling stream sends before it goes quiet, naming the provider serving it.
const FIRST_EVENT: &str = "data: {\"provider\":\"slowco\",\"choices\":[{\"index\":0,\
                           \"delta\":{\"content\":\"working\"}}]}\n\n";

/// A schedule of two retries against a one-second base and the usual sixty-second ceiling: small
/// enough that the waits a test asserts on are the schedule's own arithmetic rather than a figure
/// the default policy had to spend first.
fn two_retries() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 3,
        base_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(60),
    }
}

/// The SSE keep-alive comment OpenRouter holds a connection open with while a reply is pending.
const KEEP_ALIVE: &str = ": OPENROUTER PROCESSING\n\n";

/// A whole reply in one chunk: a single `write_file` tool call, its arguments complete, then the
/// finish reason, the usage trailer and the terminal sentinel. The shape a provider that streams no
/// reasoning and sends its reply in one piece produces — the first delta is the whole answer.
const ONE_CHUNK_TOOL_CALL: &str = concat!(
    "data: {\"provider\":\"Google\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
    "\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":",
    "{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"index.html\\\"}\"}}]}}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
    "data: [DONE]\n\n",
);

/// A body that sends a [keep-alive](KEEP_ALIVE) every two seconds for `quiet`, then `reply` in one
/// chunk at `quiet` — nothing from the model until the whole reply is ready.
fn quiet_then(
    quiet: Duration,
    reply: &'static str,
) -> impl futures_util::Stream<Item = Result<&'static str, std::io::Error>> + Send + 'static {
    let start = tokio::time::Instant::now();
    futures_util::stream::unfold(false, move |sent| async move {
        if sent {
            return None;
        }
        let at = start + quiet;
        let next_keep_alive = tokio::time::Instant::now() + Duration::from_secs(2);
        if next_keep_alive < at {
            tokio::time::sleep_until(next_keep_alive).await;
            Some((Ok(KEEP_ALIVE), false))
        } else {
            tokio::time::sleep_until(at).await;
            Some((Ok(reply), true))
        }
    })
}

/// A client answered by `body` on every attempt, built with **no** bounds of its own — so it runs
/// under [`DEFAULT_MODEL_CALL_TIMEOUT`] and [`DEFAULT_MODEL_STREAM_IDLE`] — and on `retry`, with
/// each response head held back `head_delay`. Returns it and the count of requests it was sent.
fn default_client<S>(
    retry: RetryPolicy,
    head_delay: Duration,
    body: impl Fn() -> S + Send + Sync + 'static,
) -> (OpenRouterClient, Arc<AtomicUsize>)
where
    S: futures_util::Stream<Item = Result<&'static str, std::io::Error>> + Send + 'static,
{
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&requests);
    let client = OpenRouterClient::new(
        "http://gateway.invalid/api/v1",
        reqwest::Client::new(),
        "google/gemini-4-argon",
        "sk-test",
        retry,
        None,
    )
    .with_head_delay(head_delay)
    .answered_by(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::wrap_stream(body()))
            .expect("a well-formed response")
            .into()
    });
    (client, requests)
}

/// Every `warn` line on the sink.
fn warnings(sink: &CollectingSink) -> Vec<String> {
    sink.events()
        .into_iter()
        .filter_map(|event| match event.kind {
            GgTelemetryKind::Log { level, message } if level == "warn" => Some(message),
            _ => None,
        })
        .collect()
}

/// The one `write_file` call a [one-chunk reply](ONE_CHUNK_TOOL_CALL) assembles into.
fn assert_the_one_chunk_reply(outcome: Result<ModelResponse, ModelError>) {
    let reply = outcome.expect("a reply that arrived inside the ceiling is a reply");
    assert_eq!(reply.tool_calls.len(), 1, "{reply:?}");
    assert_eq!(reply.tool_calls[0].name, "write_file");
    assert_eq!(
        reply.tool_calls[0].arguments,
        serde_json::json!({ "path": "index.html" })
    );
}

/// A gateway that answers `200` with `first` as the whole of what its body ever delivers, and then
/// never delivers another byte nor ends. Returns the client answered by it — on the retry schedule
/// and the two bounds the test names — and the count of the requests it was sent, the direct
/// evidence of how many attempts the client spent.
fn stalled_client(
    first: &'static str,
    retry: RetryPolicy,
    ceiling: Duration,
    idle: Duration,
) -> (OpenRouterClient, Arc<AtomicUsize>) {
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&requests);
    let client = OpenRouterClient::new(
        "http://gateway.invalid/api/v1",
        reqwest::Client::new(),
        "openai/gpt-5.6",
        "sk-test",
        retry,
        None,
    )
    .with_model_call_timeout(ceiling)
    .with_stream_idle(idle)
    .answered_by(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        let chunks = futures_util::stream::iter(
            (!first.is_empty()).then_some(Ok::<_, std::io::Error>(first)),
        )
        .chain(futures_util::stream::pending());
        http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::wrap_stream(chunks))
            .expect("a well-formed response")
            .into()
    });
    (client, requests)
}

/// A stream that goes quiet without ever sending a delta is **not** a stall: the idle bound is
/// unarmed until the reply's first delta, so the silence is bounded by the configured ceiling
/// alone, however much narrower the idle bound is — and a ceiling surfaces as
/// [`ModelError::Timeout`] without spending the retry schedule or recording a stall.
#[tokio::test(start_paused = true)]
async fn a_gateway_that_never_sends_a_delta_is_cut_at_the_ceiling_not_the_idle_bound() {
    let (client, requests) = stalled_client("", two_retries(), CONFIGURED, CONFIGURED_IDLE);
    let sink = CollectingSink::new();
    client.announce_retries_on(&Emitter::with_sink(None, Box::new(sink.clone())));

    let started = tokio::time::Instant::now();
    let outcome = client.complete(&[Message::user("build it")], &[]).await;

    assert_eq!(
        started.elapsed(),
        CONFIGURED,
        "the wait for the first delta is bounded by the ceiling alone"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(
        matches!(outcome, Err(ModelError::Timeout { after, provider: None }) if after == CONFIGURED),
        "{outcome:?}"
    );
    assert!(
        warnings(&sink).is_empty(),
        "nothing was retried, so nothing was announced"
    );
    assert!(
        !sink.events().iter().any(|event| matches!(
            event.kind,
            GgTelemetryKind::ProviderFault {
                fault: GgProviderFault::Stall,
                ..
            }
        )),
        "a silence before the first delta is not a stall"
    );
}

/// A first delta that arrives **later than the idle bound but inside the ceiling** is a reply: the
/// keep-alives before it neither arm the idle clock nor count as progress, and the whole reply
/// arriving in one chunk at thirty seconds — nearly three idle bounds in — completes on the first
/// attempt.
#[tokio::test(start_paused = true)]
async fn a_first_delta_later_than_the_idle_bound_but_inside_the_ceiling_is_a_reply() {
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&requests);
    let client = OpenRouterClient::new(
        "http://gateway.invalid/api/v1",
        reqwest::Client::new(),
        "openai/gpt-5.6",
        "sk-test",
        two_retries(),
        None,
    )
    .with_model_call_timeout(CONFIGURED)
    .with_stream_idle(CONFIGURED_IDLE)
    .answered_by(move |_| {
        seen.fetch_add(1, Ordering::SeqCst);
        http::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .body(reqwest::Body::wrap_stream(quiet_then(
                Duration::from_secs(30),
                ONE_CHUNK_TOOL_CALL,
            )))
            .expect("a well-formed response")
            .into()
    });

    let started = tokio::time::Instant::now();
    let outcome = client.complete(&[Message::user("build it")], &[]).await;

    assert_eq!(started.elapsed(), Duration::from_secs(30));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_the_one_chunk_reply(outcome);
}

/// **Under gg's defaults**, a model that streams no reasoning and sends its whole reply — one
/// tool call — in a single chunk ninety seconds after the request, keep-alives aside, gets that
/// reply on the first attempt. Ninety seconds is past the sixty-second
/// [idle bound](DEFAULT_MODEL_STREAM_IDLE), which is unarmed until the first delta, and well inside
/// the [ceiling](DEFAULT_MODEL_CALL_TIMEOUT).
#[tokio::test(start_paused = true)]
async fn under_the_defaults_a_ninety_second_one_chunk_reply_completes() {
    assert_eq!(DEFAULT_MODEL_CALL_TIMEOUT, Duration::from_secs(1800));
    assert_eq!(DEFAULT_MODEL_STREAM_IDLE, Duration::from_secs(60));
    let (client, requests) = default_client(RetryPolicy::default(), Duration::ZERO, || {
        quiet_then(Duration::from_secs(90), ONE_CHUNK_TOOL_CALL)
    });

    let started = tokio::time::Instant::now();
    let outcome = client.complete(&[Message::user("build it")], &[]).await;

    assert_eq!(started.elapsed(), Duration::from_secs(90));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_the_one_chunk_reply(outcome);
}

/// The same reply under the defaults when the provider holds back the **response head** itself
/// for ninety seconds and then sends the whole reply at once: the head wait is bounded by the
/// [ceiling](DEFAULT_MODEL_CALL_TIMEOUT) alone, so it completes on the first attempt too.
#[tokio::test(start_paused = true)]
async fn under_the_defaults_a_response_head_ninety_seconds_late_completes() {
    let (client, requests) =
        default_client(RetryPolicy::default(), Duration::from_secs(90), || {
            futures_util::stream::iter([Ok(ONE_CHUNK_TOOL_CALL)])
        });

    let started = tokio::time::Instant::now();
    let outcome = client.complete(&[Message::user("build it")], &[]).await;

    assert_eq!(started.elapsed(), Duration::from_secs(90));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert_the_one_chunk_reply(outcome);
}

/// **Under gg's defaults**, a reply that starts and then stops mid-reply is still cancelled sixty
/// seconds after its last delta, however long the first delta took to come: the clock armed by the
/// first delta is the [idle bound](DEFAULT_MODEL_STREAM_IDLE), and the keep-alives after it do not
/// restart it. The retry that follows answers.
#[tokio::test(start_paused = true)]
async fn under_the_defaults_a_reply_that_stops_mid_stream_is_cancelled_sixty_seconds_after_its_last_delta()
 {
    let attempts = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&attempts);
    let (client, requests) = default_client(RetryPolicy::default(), Duration::ZERO, move || {
        let first = seen.fetch_add(1, Ordering::SeqCst) == 0;
        let body: std::pin::Pin<
            Box<dyn futures_util::Stream<Item = Result<&'static str, std::io::Error>> + Send>,
        > = if first {
            // Forty seconds of keep-alives, one delta, then keep-alives forever and never a
            // delta again.
            Box::pin(quiet_then(Duration::from_secs(40), FIRST_EVENT).chain(
                futures_util::stream::unfold((), |()| async move {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    Some((Ok(KEEP_ALIVE), ()))
                }),
            ))
        } else {
            Box::pin(futures_util::stream::iter([Ok(ONE_CHUNK_TOOL_CALL)]))
        };
        body
    });
    let sink = CollectingSink::new();
    client.announce_retries_on(&Emitter::with_sink(None, Box::new(sink.clone())));

    let started = tokio::time::Instant::now();
    let outcome = client.complete(&[Message::user("build it")], &[]).await;

    assert_eq!(
        started.elapsed(),
        Duration::from_secs(40) + DEFAULT_MODEL_STREAM_IDLE + RetryPolicy::default().base_delay,
        "the stall was cut sixty seconds after the delta at forty, then one schedule wait"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_the_one_chunk_reply(outcome);
    let warned = warnings(&sink);
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert!(
        warned[0]
            .contains("the stream stalled: no delta from the model for 60s (provider: slowco)"),
        "{warned:?}"
    );
}

/// The [call ceiling](DEFAULT_MODEL_CALL_TIMEOUT) bounds **each attempt's whole duration**, not the
/// backoff between them: a schedule whose waits add up to far more than the ceiling runs to its
/// answer, and an attempt that runs past the ceiling after earlier ones failed is cut at the
/// ceiling measured from its own start.
///
/// The stalled third attempt never sends a delta, so the idle bound is unarmed and it is the
/// ceiling that cuts it — the same reading a provider that streams nothing until its reply is
/// complete produces when the reply never comes.
#[tokio::test(start_paused = true)]
async fn the_ceiling_bounds_each_attempt_and_leaves_the_backoff_outside() {
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&requests);
    let client = OpenRouterClient::new(
        "http://gateway.invalid/api/v1",
        reqwest::Client::new(),
        "openai/gpt-5.6",
        "sk-test",
        two_retries(),
        None,
    )
    .with_model_call_timeout(CONFIGURED)
    .with_stream_idle(CONFIGURED_IDLE)
    .answered_by(move |_| {
        let attempt = seen.fetch_add(1, Ordering::SeqCst);
        let builder = http::Response::builder();
        match attempt {
            // Two `503`s asking for a minute each: two minutes of waiting, past the ceiling.
            0 | 1 => builder
                .status(503)
                .header("retry-after", "60")
                .body(reqwest::Body::from("overloaded")),
            // Then an attempt that never answers at all — a silence only the ceiling bounds.
            _ => builder.status(200).body(reqwest::Body::wrap_stream(
                futures_util::stream::pending::<Result<&'static str, std::io::Error>>(),
            )),
        }
        .expect("a well-formed response")
        .into()
    });

    let started = tokio::time::Instant::now();
    let outcome = client.complete(&[Message::user("build it")], &[]).await;

    assert!(
        matches!(outcome, Err(ModelError::Timeout { after, .. }) if after == CONFIGURED),
        "the stalled third attempt is cut by the ceiling: {outcome:?}"
    );
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert_eq!(
        started.elapsed(),
        Duration::from_secs(120) + CONFIGURED,
        "both waits ran in full, and the ceiling was measured from the third attempt"
    );
}

/// A stream that stalls **mid-reply**, after a delta arrived, is cut on idleness at the configured
/// bound and retried, and the retry's `warn` names the provider the chunk that did arrive named —
/// a stalled provider is blacklistable by name exactly as a failing one is.
#[tokio::test(start_paused = true)]
async fn a_mid_stream_stall_is_cancelled_at_the_idle_bound_naming_its_provider() {
    let (client, requests) =
        stalled_client(FIRST_EVENT, two_retries(), CONFIGURED, CONFIGURED_IDLE);
    let sink = CollectingSink::new();
    client.announce_retries_on(&Emitter::with_sink(None, Box::new(sink.clone())));

    let started = tokio::time::Instant::now();
    let outcome = client.complete(&[Message::user("build it")], &[]).await;

    assert_eq!(
        started.elapsed(),
        3 * CONFIGURED_IDLE + Duration::from_secs(1) + Duration::from_secs(2)
    );
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert!(
        matches!(outcome, Err(ModelError::RetryExhausted { .. })),
        "{outcome:?}"
    );
    let warned = sink
        .events()
        .into_iter()
        .filter_map(|event| match event.kind {
            GgTelemetryKind::Log { level, message } if level == "warn" => Some(message),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(warned.len(), 2, "one warn per retry: {warned:?}");
    assert!(
        warned[0]
            .contains("the stream stalled: no delta from the model for 11s (provider: slowco)"),
        "the retry is announced as a stall naming the provider: {warned:?}"
    );
}

/// A call cut by the [ceiling](DEFAULT_MODEL_CALL_TIMEOUT) surfaces as [`ModelError::Timeout`]
/// **immediately**, without spending the retry budget: each internal retry of a ceiling-long
/// silence would cost the full ceiling again, and the turn-level retry is the bounded one the
/// error ceilings govern.
#[tokio::test(start_paused = true)]
async fn a_ceiling_long_silence_times_out_without_spending_retries() {
    let (client, requests) = stalled_client(
        // A delta at once and then silence, under an idle bound wider than the ceiling: once the
        // idle clock is armed, the ceiling is the one that fires when it has the less time left.
        FIRST_EVENT,
        two_retries(),
        CONFIGURED,
        CONFIGURED + CONFIGURED,
    );

    let started = tokio::time::Instant::now();
    let outcome = client.complete(&[Message::user("build it")], &[]).await;
    assert_eq!(
        started.elapsed(),
        CONFIGURED,
        "the call was cut by a bound other than the configured ceiling"
    );

    match outcome {
        Err(ModelError::Timeout { after, provider }) => {
            assert_eq!(after, CONFIGURED);
            assert_eq!(
                provider.as_deref(),
                Some("slowco"),
                "the chunk that did arrive named who was serving the attempt"
            );
        }
        other => panic!("a ceiling-long silence must time out, got {other:?}"),
    }
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the ceiling is not retried internally"
    );
}

/// A stream that stalls after its first chunk is cut on **idleness**, measured from the delta that
/// chunk carried, and the stall names the provider the chunk did.
#[tokio::test(start_paused = true)]
async fn a_stream_that_goes_quiet_after_a_chunk_is_a_stall_naming_its_provider() {
    let stream =
        futures_util::stream::iter([Ok::<_, std::convert::Infallible>(FIRST_EVENT.as_bytes())])
            .chain(futures_util::stream::pending());
    let config = Some(resolved_config());

    let idle = Duration::from_secs(5);
    let started = tokio::time::Instant::now();
    let outcome = read_stream(stream, config, started + DEFAULT_MODEL_CALL_TIMEOUT, idle).await;
    assert_eq!(started.elapsed(), idle);
    match outcome {
        StreamOutcome::Stalled { idle, provider } => {
            assert_eq!(idle, Duration::from_secs(5));
            assert_eq!(
                provider.as_deref(),
                Some("slowco"),
                "the chunk that did arrive named who was serving the stalled stream"
            );
        }
        _ => panic!("a stream that stopped producing must end as a stall"),
    }
}

/// Keep-alive comments and blank lines do not restart the idle clock: a provider that has stopped
/// answering can send them forever, and a reader that mistook one for progress would wait the call
/// ceiling out on each. The stall is measured from the **delta**, and the paused clock reads the
/// delta's gap plus the bound, not the keep-alive's arrival.
#[tokio::test(start_paused = true)]
async fn keep_alives_do_not_restart_the_idle_clock() {
    let stream =
        futures_util::stream::iter([Ok::<_, std::convert::Infallible>(FIRST_EVENT.as_bytes())])
            .chain(futures_util::stream::unfold((), |()| async move {
                // A keep-alive comment every two seconds, forever, and never a delta again.
                tokio::time::sleep(Duration::from_secs(2)).await;
                Some((Ok(": OPENROUTER PROCESSING\n\n".as_bytes()), ()))
            }));
    let idle = Duration::from_secs(5);
    let started = tokio::time::Instant::now();
    let outcome = read_stream(
        Box::pin(stream),
        None,
        started + DEFAULT_MODEL_CALL_TIMEOUT,
        idle,
    )
    .await;
    // The delta arrived at 0 s; keep-alives at 2 s and 4 s; the bound expires at 5 s — a clock
    // restarted by either keep-alive would have read 6 s or 7 s instead.
    assert_eq!(started.elapsed(), idle);
    assert!(
        matches!(outcome, StreamOutcome::Stalled { idle, .. } if idle == Duration::from_secs(5)),
        "keep-alives are not progress: {outcome:?}"
    );
}

/// A reply whose deltas keep arriving is never cut on idleness, however much longer than the idle
/// bound it runs: each delta restarts the clock.
#[tokio::test(start_paused = true)]
async fn a_reply_still_delivering_deltas_outlives_the_idle_bound() {
    let deltas = futures_util::stream::unfold(0, |tick| async move {
        match tick {
            // A delta every four seconds, five times over: twenty seconds against a five-second
            // bound.
            0..5 => {
                tokio::time::sleep(Duration::from_secs(4)).await;
                Some((
                    Ok::<_, std::convert::Infallible>(FIRST_EVENT.as_bytes()),
                    tick + 1,
                ))
            }
            5 => Some((
                Ok(
                    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                    data: [DONE]\n\n"
                        .as_bytes(),
                ),
                tick + 1,
            )),
            _ => None,
        }
    });
    let started = tokio::time::Instant::now();
    let outcome = read_stream(
        Box::pin(deltas),
        None,
        started + DEFAULT_MODEL_CALL_TIMEOUT,
        Duration::from_secs(5),
    )
    .await;
    assert_eq!(started.elapsed(), Duration::from_secs(20));
    match outcome {
        StreamOutcome::Reply(reply) => {
            assert_eq!(
                reply.text.as_deref(),
                Some("workingworkingworkingworkingworking")
            );
        }
        other => panic!("a reply still delivering deltas must complete, got {other:?}"),
    }
}

/// A provider that accepts the connection and never sends a response head is bounded by the
/// [ceiling](DEFAULT_MODEL_CALL_TIMEOUT) alone: the idle clock is unarmed until a delta arrives,
/// so the attempt is held for the whole ceiling and then surfaces as [`ModelError::Timeout`]
/// without spending the retry schedule.
///
/// The gateway is a real socket that is listened on and never answered.
#[tokio::test(start_paused = true)]
async fn a_response_head_that_never_arrives_times_out_at_the_ceiling() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a local port");
    let address = listener.local_addr().expect("the bound address");
    let client = OpenRouterClient::new(
        format!("http://{address}/api/v1"),
        reqwest::Client::new(),
        "openai/gpt-5.6",
        "sk-test",
        two_retries(),
        None,
    )
    .with_model_call_timeout(CONFIGURED)
    .with_stream_idle(CONFIGURED_IDLE);
    let sink = CollectingSink::new();
    client.announce_retries_on(&Emitter::with_sink(None, Box::new(sink.clone())));

    let started = tokio::time::Instant::now();
    let outcome = client.complete(&[Message::user("build it")], &[]).await;

    assert_eq!(
        started.elapsed(),
        CONFIGURED,
        "the head wait cost the call ceiling, not the idle bound"
    );
    assert!(
        matches!(outcome, Err(ModelError::Timeout { after, provider: None }) if after == CONFIGURED),
        "{outcome:?}"
    );
    assert!(
        warnings(&sink).is_empty(),
        "a ceiling is not retried internally"
    );
    drop(listener);
}

/// The same head wait through the [held-back gateway](OpenRouterClient::with_head_delay): a head
/// that arrives only after the ceiling is cut at the ceiling, once.
#[tokio::test(start_paused = true)]
async fn a_response_head_later_than_the_ceiling_times_out_once() {
    let (client, requests) = default_client(
        two_retries(),
        DEFAULT_MODEL_CALL_TIMEOUT + Duration::from_secs(1),
        || futures_util::stream::iter([Ok(ONE_CHUNK_TOOL_CALL)]),
    );

    let started = tokio::time::Instant::now();
    let outcome = client.complete(&[Message::user("build it")], &[]).await;

    assert_eq!(started.elapsed(), DEFAULT_MODEL_CALL_TIMEOUT);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        0,
        "the head was never answered"
    );
    assert!(
        matches!(outcome, Err(ModelError::Timeout { after, provider: None })
            if after == DEFAULT_MODEL_CALL_TIMEOUT),
        "{outcome:?}"
    );
}

/// Keep-alive comments and blank lines **before any delta** do not arm the idle clock: a read that
/// receives nothing else runs to the [ceiling](DEFAULT_MODEL_CALL_TIMEOUT), however many idle
/// bounds pass, and ends as a [`Ceiling`](StreamOutcome::Ceiling) rather than a stall.
#[tokio::test(start_paused = true)]
async fn keep_alives_before_any_delta_reach_the_ceiling_not_a_stall() {
    let stream = futures_util::stream::unfold((), |()| async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        Some((
            Ok::<_, std::convert::Infallible>(": OPENROUTER PROCESSING\n\n\n".as_bytes()),
            (),
        ))
    });
    let idle = Duration::from_secs(5);
    let started = tokio::time::Instant::now();
    let outcome = read_stream(Box::pin(stream), None, started + CONFIGURED, idle).await;
    assert_eq!(started.elapsed(), CONFIGURED);
    assert!(
        matches!(outcome, StreamOutcome::Ceiling { provider: None }),
        "keep-alives before the first delta neither arm nor restart the idle clock: {outcome:?}"
    );
}

/// The detector configuration the streaming tests arm, resolved the way the client resolves it.
fn resolved_config() -> LoopGuardConfig {
    LoopGuardConfig {
        window_words: 256,
        repeat_threshold: 32,
        min_offenders: 2,
        min_saturated_run: 3000,
        max_response_chars: 0,
    }
}
