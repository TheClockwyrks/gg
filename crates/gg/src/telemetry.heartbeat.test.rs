//! **Heartbeats**: what [`Emitter::while_waiting`] says while an agent waits, and when it stays
//! quiet.
//!
//! Every test runs on tokio's paused clock, so a wait of two and a half minutes takes no real time
//! and the beats land on exactly the instants the interval puts them at: what is asserted is the
//! count and the figures, never a tolerance.

use std::time::Duration;

use test_cabinet_contracts::gg::{GgTelemetryEvent, GgTelemetryKind, GgWaitKind};
use tokio::time::sleep;

use super::*;

/// A wait of `secs` seconds on the paused clock.
async fn wait_for(secs: u64) {
    sleep(Duration::from_secs(secs)).await;
}

/// Every heartbeat in `events`, as `(agentId, waitingOn, waitedMs, detail)`.
fn heartbeats(
    events: &[GgTelemetryEvent],
) -> Vec<(Option<String>, GgWaitKind, u64, Option<String>)> {
    events
        .iter()
        .filter_map(|event| match &event.kind {
            GgTelemetryKind::Heartbeat {
                waiting_on,
                waited_ms,
                detail,
            } => Some((
                event.agent_id.clone(),
                *waiting_on,
                *waited_ms,
                detail.clone(),
            )),
            _ => None,
        })
        .collect()
}

/// A base emitter writing into a fresh capturing sink, and the sink to read it back from.
fn capturing() -> (Emitter, CapturingSink) {
    let sink = CapturingSink::new();
    (
        Emitter::with_sink(Some("run-1".to_string()), Box::new(sink.clone())),
        sink,
    )
}

/// The documented cadence: one beat a minute.
#[test]
fn the_interval_is_a_minute() {
    assert_eq!(HEARTBEAT_INTERVAL, Duration::from_secs(60));
}

/// A wait of two and a half minutes beats twice — one interval in and two intervals in — each beat
/// naming what is waited on and how long the wait has lasted, on the waiting agent's stream; and
/// the wait's own output comes back unchanged.
#[tokio::test(start_paused = true)]
async fn a_wait_of_150_seconds_beats_twice() {
    let (base, sink) = capturing();
    let emitter = base.for_agent("root", None);

    let output = emitter
        .while_waiting(GgWaitKind::ToolCall, Some("shell".to_string()), async {
            wait_for(150).await;
            42
        })
        .await;

    assert_eq!(output, 42);
    let root = Some("root".to_string());
    let shell = Some("shell".to_string());
    assert_eq!(
        heartbeats(&sink.events()),
        vec![
            (root.clone(), GgWaitKind::ToolCall, 60_000, shell.clone()),
            (root, GgWaitKind::ToolCall, 120_000, shell),
        ]
    );
}

/// The first beat comes one interval in, never at the start: a wait shorter than the interval says
/// nothing at all.
#[tokio::test(start_paused = true)]
async fn a_wait_shorter_than_the_interval_never_beats() {
    let (base, sink) = capturing();
    let emitter = base.for_agent("root", None);

    emitter
        .while_waiting(GgWaitKind::ModelRequest, None, wait_for(59))
        .await;

    assert!(sink.events().is_empty(), "{:?}", sink.lines());
}

/// A wait that ends exactly on a beat returns rather than reporting it is still waiting.
#[tokio::test(start_paused = true)]
async fn a_wait_ending_on_a_beat_does_not_report_it() {
    let (base, sink) = capturing();
    let emitter = base.for_agent("root", None);

    emitter
        .while_waiting(GgWaitKind::Program, None, wait_for(120))
        .await;

    let beats = heartbeats(&sink.events());
    assert_eq!(beats.len(), 1, "{beats:?}");
    assert_eq!(beats[0].2, 60_000);
}

/// Each wait kind reaches the wire as its own `waitingOn`, and a beat without a detail leaves the
/// field off.
#[tokio::test(start_paused = true)]
async fn every_wait_kind_beats_under_its_own_name() {
    let kinds = [
        (GgWaitKind::ModelRequest, "model_request"),
        (GgWaitKind::ToolCall, "tool_call"),
        (GgWaitKind::Program, "program"),
        (GgWaitKind::Subagent, "subagent"),
    ];
    for (kind, wire) in kinds {
        let (base, sink) = capturing();
        base.for_agent("root", None)
            .while_waiting(kind, None, wait_for(61))
            .await;
        let lines = sink.lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        let line: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(line["type"], "heartbeat");
        assert_eq!(line["waitingOn"], wire);
        assert_eq!(line["waitedMs"], 60_000);
        assert_eq!(line["agentId"], "root");
        assert!(line.get("detail").is_none(), "{line}");
    }
}

/// Nested waits on one agent — a tool call that is itself a subagent wait — run two tickers, and
/// still beat once per interval, labelled by the outer wait, which began first.
#[tokio::test(start_paused = true)]
async fn nested_waits_do_not_beat_twice() {
    let (base, sink) = capturing();
    let emitter = base.for_agent("root", None);

    emitter
        .while_waiting(
            GgWaitKind::ToolCall,
            Some("wait_for_subagents".to_string()),
            emitter.while_waiting(
                GgWaitKind::Subagent,
                Some("subagent `a`".to_string()),
                wait_for(150),
            ),
        )
        .await;

    let beats = heartbeats(&sink.events());
    assert_eq!(beats.len(), 2, "{beats:?}");
    assert!(
        beats.iter().all(|beat| beat.0.as_deref() == Some("root")),
        "{beats:?}"
    );
}

/// An inner wait that begins part-way into the outer one ticks out of step with it, and its ticks
/// fall inside an interval the agent has already beaten in, so they are dropped: still one beat per
/// interval, all of them the outer wait's.
#[tokio::test(start_paused = true)]
async fn an_inner_wait_out_of_step_with_the_outer_does_not_beat_between() {
    let (base, sink) = capturing();
    let emitter = base.for_agent("root", None);

    emitter
        .while_waiting(GgWaitKind::Program, None, async {
            wait_for(10).await;
            emitter
                .while_waiting(
                    GgWaitKind::ToolCall,
                    Some("shell".to_string()),
                    wait_for(140),
                )
                .await;
        })
        .await;

    assert_eq!(
        heartbeats(&sink.events()),
        vec![
            (Some("root".to_string()), GgWaitKind::Program, 60_000, None),
            (Some("root".to_string()), GgWaitKind::Program, 120_000, None),
        ]
    );
}

/// Parallel waits on one agent, through two emitters of its (the clone a client keeps and the loop's
/// own), share the agent's guard too: two waits of 150 s side by side beat twice, not four times.
#[tokio::test(start_paused = true)]
async fn parallel_waits_on_one_agent_beat_once_per_interval() {
    let (base, sink) = capturing();
    let emitter = base.for_agent("root", None);
    let other = emitter.clone();

    tokio::join!(
        emitter.while_waiting(GgWaitKind::ToolCall, None, wait_for(150)),
        other.while_waiting(GgWaitKind::ModelRequest, None, wait_for(150)),
    );

    assert_eq!(heartbeats(&sink.events()).len(), 2);
}

/// When the wait that has been beating ends, a wait still running on the same agent picks the beat
/// up at its own next tick — the guard limits the rate, it never silences the agent.
#[tokio::test(start_paused = true)]
async fn a_surviving_parallel_wait_keeps_beating() {
    let (base, sink) = capturing();
    let emitter = base.for_agent("root", None);

    tokio::join!(
        emitter.while_waiting(GgWaitKind::ToolCall, None, wait_for(100)),
        async {
            wait_for(5).await;
            emitter
                .while_waiting(GgWaitKind::Subagent, None, wait_for(195))
                .await;
        },
    );

    // The tool call beats at 60 s; the subagent wait's 65 s tick falls in that interval and is
    // dropped, and its 125 s and 185 s ticks beat, 120 s and 180 s into it.
    let beats: Vec<(GgWaitKind, u64)> = heartbeats(&sink.events())
        .into_iter()
        .map(|beat| (beat.1, beat.2))
        .collect();
    assert_eq!(
        beats,
        vec![
            (GgWaitKind::ToolCall, 60_000),
            (GgWaitKind::Subagent, 120_000),
            (GgWaitKind::Subagent, 180_000),
        ]
    );
}

/// Two agents waiting at once each beat on their own stream, under their own id: one agent's beat
/// never stands in for another's.
#[tokio::test(start_paused = true)]
async fn each_agent_beats_under_its_own_id() {
    let (base, sink) = capturing();
    let root = base.for_agent("root", None);
    let child = base.for_agent("child-1", Some("root".to_string()));

    tokio::join!(
        root.while_waiting(GgWaitKind::Subagent, None, wait_for(150)),
        child.while_waiting(GgWaitKind::ModelRequest, None, wait_for(150)),
    );

    let events = sink.events();
    let of = |agent: &str| {
        events
            .iter()
            .filter(|event| {
                matches!(event.kind, GgTelemetryKind::Heartbeat { .. })
                    && event.agent_id.as_deref() == Some(agent)
            })
            .collect::<Vec<_>>()
    };
    let root_beats = of("root");
    let child_beats = of("child-1");
    assert_eq!(root_beats.len(), 2);
    assert_eq!(child_beats.len(), 2);
    assert!(
        child_beats
            .iter()
            .all(|event| event.parent_agent_id.as_deref() == Some("root"))
    );
    assert!(root_beats.iter().all(|event| matches!(
        event.kind,
        GgTelemetryKind::Heartbeat {
            waiting_on: GgWaitKind::Subagent,
            ..
        }
    )));
}

/// A wait that is dropped part-way — its caller cancelled — stops beating with it: the ticker is
/// polled beside the wait, never spawned, so nothing outlives it.
#[tokio::test(start_paused = true)]
async fn a_cancelled_wait_stops_beating() {
    let (base, sink) = capturing();
    let emitter = base.for_agent("root", None);

    let cut = tokio::time::timeout(
        Duration::from_secs(70),
        emitter.while_waiting(GgWaitKind::ToolCall, None, wait_for(1_000)),
    )
    .await;
    assert!(cut.is_err());
    wait_for(500).await;

    assert_eq!(heartbeats(&sink.events()).len(), 1);
}

/// The interval is the base emitter's, so a test can shorten it once for every agent derived from
/// it.
#[tokio::test(start_paused = true)]
async fn the_interval_can_be_shortened_on_the_base_emitter() {
    let (base, sink) = capturing();
    let emitter = base
        .with_heartbeat_interval(Duration::from_secs(10))
        .for_agent("root", None);

    emitter
        .while_waiting(GgWaitKind::ToolCall, None, wait_for(25))
        .await;

    let waited: Vec<u64> = heartbeats(&sink.events())
        .into_iter()
        .map(|beat| beat.2)
        .collect();
    assert_eq!(waited, vec![10_000, 20_000]);
}

/// The session summary folds nothing from a heartbeat: a run whose agents beat through their waits
/// summarizes exactly as the same run without them.
#[tokio::test(start_paused = true)]
async fn heartbeats_leave_the_summary_unchanged() {
    /// The same small stream of events, on `emitter`, with or without a beating wait between them.
    async fn run(emitter: &Emitter, beat: bool) {
        emitter.emit(GgTelemetryKind::ToolResult {
            name: "shell".to_string(),
            ok: true,
            summary: None,
            failure: None,
        });
        if beat {
            emitter
                .while_waiting(
                    GgWaitKind::ToolCall,
                    Some("shell".to_string()),
                    wait_for(300),
                )
                .await;
        }
        emitter.emit(GgTelemetryKind::ToolResult {
            name: "read_file".to_string(),
            ok: false,
            summary: None,
            failure: Some(test_cabinet_contracts::gg::GgCallFailure::Other),
        });
    }

    let (quiet, quiet_sink) = capturing();
    run(&quiet.for_agent("root", None), false).await;
    let (beating, beating_sink) = capturing();
    run(&beating.for_agent("root", None), true).await;

    assert!(heartbeats(&quiet_sink.events()).is_empty());
    assert_eq!(heartbeats(&beating_sink.events()).len(), 4);
    assert_eq!(
        serde_json::to_value(beating.finalize_summary("completed")).unwrap(),
        serde_json::to_value(quiet.finalize_summary("completed")).unwrap(),
    );
}
