//! **A command that wedges its call, end to end**: the scenario GGH-3 was filed on, driven
//! through a whole session with the real shell under a scripted model.
//!
//! A model ran `node -e "require('child_process').spawn('sleep', ['3000'], { stdio: 'pipe' })"`:
//! `node` stays alive for as long as the child it spawned on its own pipes does, so the call
//! never returned on its own, and the agent said nothing for half an hour until the host's idle
//! watchdog ended the run. What the [shell](https://docs.testcabinet.ai/gg/shell/) and the
//! [stream](https://docs.testcabinet.ai/gg/telemetry/overview/) promise instead, all checked on
//! one run here:
//!
//! - the call ends on gg's own per-call bound, as the timeout failure the model reads, and the
//!   session carries on to its own ending;
//! - the kill reaches the whole process group, so the `sleep` the command's child spawned is gone
//!   afterwards rather than left running in the container;
//! - while the call lasts, the agent [beats](Emitter::while_waiting) on its stream, naming the
//!   tool it waits on, so a host watching the stream sees a live agent.
//!
//! Everything else is the real thing: [`run_with_factory`] runs the
//! [real shell](crate::tools::real_shell), so the processes are real and the clock is the wall
//! clock. The heartbeat interval is shortened to fit inside the short timeout the model asks for.

use super::*;
use crate::model::Role;
use test_cabinet_contracts::gg::{GgCallFailure, GgWaitKind};

/// How long the model lets the command run: long enough for several beats, short enough for a
/// unit test.
const CALL_TIMEOUT_SECS: f64 = 2.0;

/// The beat cadence for this run, in place of the documented minute.
const BEAT: std::time::Duration = std::time::Duration::from_millis(200);

/// Whether `pid` names a live process: present and not a zombie awaiting its reaper.
fn is_running(pid: libc::pid_t) -> bool {
    // SAFETY: signal 0 performs only the existence and permission check; nothing is delivered.
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    // `/proc/<pid>/stat` reads `<pid> (<comm>) <state> …`; state `Z` is a zombie. Where there is no
    // `/proc`, an existing process is taken to be running.
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .is_none_or(|state| state != "Z"),
        Err(_) => true,
    }
}

/// Poll until `pid` is no longer running, for at most `bound`; answers whether it stopped. The
/// kill was already sent, so what is waited on is only the kernel and the reaper finishing with
/// it, and the poll sleeps between looks rather than spinning.
async fn stops_within(pid: libc::pid_t, bound: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + bound;
    loop {
        if !is_running(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Kill `pid` if it is still there, so a failed assertion leaves nothing running behind the test.
struct KillOnDrop(libc::pid_t);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // SAFETY: as in `is_running`; a pid that is already gone answers `ESRCH`, which is ignored.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
        }
    }
}

/// Whether a `node` is on the `PATH` to run the command as it was met.
fn has_node() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// The issue's command, plus the spawned child's pid written to `spawned.pid` so the test can
/// look for it afterwards. A subshell holding a backgrounded `sleep` stands in where there is no
/// `node`: the same process tree.
fn wedging_command() -> &'static str {
    if has_node() {
        "node -e \"const c = require('child_process').spawn('sleep', ['3000'], { stdio: 'pipe' }); \
         require('fs').writeFileSync('spawned.pid', String(c.pid))\""
    } else {
        "sh -c 'sleep 3000 </dev/null >/dev/null 2>&1 & echo $! > spawned.pid; wait'"
    }
}

#[tokio::test]
async fn a_command_whose_child_holds_its_pipes_times_out_beating_and_leaves_nothing_running() {
    let dir = TempDir::new().unwrap();
    let sink = CollectingSink::new();
    let emitter = Emitter::with_sink(Some("run-wedged".to_string()), Box::new(sink.clone()))
        .with_heartbeat_interval(BEAT);
    let client = Arc::new(RecordingClient::new(
        "mock/primary",
        vec![
            ending_call(
                "s1",
                "shell",
                json!({ "command": wedging_command(), "timeout_secs": CALL_TIMEOUT_SECS }),
            ),
            finish_call("f1", "the command timed out"),
        ],
    ));
    let shared = Arc::clone(&client);
    let factory = ScriptedFactory::new().slot(ROOT_PROFILE_ID, move |_| {
        Box::new(SharedRecordingClient(Arc::clone(&shared)))
    });
    let inv = invocation(dir.path(), GgCapabilitySet::minimal("mock/primary"));

    // Bounded: the defect this covers is a call that never returns, which would otherwise be a
    // test that never finishes rather than one that fails.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        run_with_factory(&inv, &emitter, Arc::new(factory)),
    )
    .await
    .expect("the session must end on the call's own timeout, not hang on the wedged command");
    let spawned: libc::pid_t = std::fs::read_to_string(dir.path().join("spawned.pid"))
        .expect("the command should have written the spawned child's pid")
        .trim()
        .parse()
        .expect("a pid");
    let _cleanup = KillOnDrop(spawned);
    let events = sink.events();

    // The session ends normally, on the model's own `finish`.
    assert!(
        matches!(outcome, SessionOutcome::Ran),
        "the session must end normally after the timeout: {outcome:?}"
    );
    let ended = events.iter().rev().find_map(|event| match &event.kind {
        GgTelemetryKind::SessionEnded { status } => Some(status.as_str()),
        _ => None,
    });
    assert_eq!(ended, Some("completed"));

    // The model reads the timeout failure as the call's result.
    let requests = client.requests();
    let answer = requests
        .last()
        .expect("the model was asked again after the call")
        .iter()
        .find(|message| message.role == Role::Tool && message.tool_call_id.as_deref() == Some("s1"))
        .and_then(|message| message.content.clone())
        .expect("the shell call is answered in the next request");
    assert!(
        answer.contains("command timed out after 2.000s and was killed."),
        "the model must read the timeout: {answer}"
    );

    // The stream records it as the limit it is, and the shell event pins the kill as -1.
    let result_at = events
        .iter()
        .position(|event| {
            matches!(
                &event.kind,
                GgTelemetryKind::ToolResult { name, ok: false, failure: Some(GgCallFailure::LimitExceeded), .. }
                    if name == "shell"
            )
        })
        .expect("a failed shell result classed as a limit");
    assert!(
        events
            .iter()
            .any(|event| matches!(&event.kind, GgTelemetryKind::Shell { exit_code: -1, .. }))
    );

    // The agent beat while the call lasted: on its own stream, naming the tool, between the call
    // and its result, and never past the call's own bound.
    let call_at = events
        .iter()
        .position(|event| {
            matches!(&event.kind, GgTelemetryKind::ToolCall { name, .. } if name == "shell")
        })
        .expect("the shell call");
    let beats: Vec<(usize, u64)> = events
        .iter()
        .enumerate()
        .filter_map(|(at, event)| match &event.kind {
            GgTelemetryKind::Heartbeat {
                waiting_on: GgWaitKind::ToolCall,
                waited_ms,
                detail,
            } if detail.as_deref() == Some("shell")
                && event.agent_id.as_deref() == Some(ROOT_AGENT_ID) =>
            {
                Some((at, *waited_ms))
            }
            _ => None,
        })
        .collect();
    assert!(
        beats.len() >= 2,
        "a two-second call on a 200 ms beat must beat more than once: {beats:?}"
    );
    for (at, waited_ms) in &beats {
        assert!(
            call_at < *at && *at < result_at,
            "a beat must fall inside the call: {beats:?}"
        );
        assert!(*waited_ms >= BEAT.as_millis() as u64);
    }
    assert!(
        beats.windows(2).all(|pair| pair[0].1 < pair[1].1),
        "waitedMs must grow beat over beat: {beats:?}"
    );

    // And nothing the command started is left behind.
    assert!(
        stops_within(spawned, std::time::Duration::from_secs(10)).await,
        "the sleep the command's child spawned must die with the group: {spawned} is still running"
    );
}
