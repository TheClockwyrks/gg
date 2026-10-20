//! Tests for the [shell seam](super): that the real runner reports what a process did without
//! interpreting it, and that a substituted one really does replace the process for every path that
//! reaches a command line.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use test_cabinet_contracts::gg_session_record::GgShellOrigin;

use super::{RealShellRunner, ShellRequest, ShellRunner, ShellStatus, StubShellRunner, real_shell};
use crate::tools::shell::ShellTool;
use crate::tools::{OffloadPolicy, Tool, ToolContext, ToolFailure, ToolOutcome, run_command};

/// A request for `command`, run in `cwd` on behalf of `agent`, with a generous ceiling.
fn request(command: &str, cwd: &std::path::Path, agent: &str) -> ShellRequest {
    ShellRequest {
        command: command.to_string(),
        cwd: cwd.to_path_buf(),
        timeout: Duration::from_secs(30),
        agent_id: agent.to_string(),
        origin: GgShellOrigin::Tool,
    }
}

// ---------------------------------------------------------------------------
// The real runner
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_real_runner_reports_the_exit_code_and_both_streams() {
    let dir = tempfile::tempdir().expect("tempdir");
    let execution = RealShellRunner
        .run(request(
            "printf out; printf err >&2; exit 3",
            dir.path(),
            "root",
        ))
        .await;

    assert_eq!(execution.status, ShellStatus::Exited { code: Some(3) });
    // Reported *unmerged* and untruncated: merging and truncation are the caller's policy, and a
    // runner that had already applied them could not be swapped for one answering from a record.
    assert_eq!(execution.stdout, "out");
    assert_eq!(execution.stderr, "err");
}

#[tokio::test]
async fn the_real_runner_runs_in_the_requested_directory() {
    let dir = tempfile::tempdir().expect("tempdir");
    let nested = dir.path().join("nested");
    std::fs::create_dir(&nested).expect("create nested");

    let execution = RealShellRunner
        .run(request("basename \"$PWD\"", &nested, "root"))
        .await;

    assert_eq!(execution.status, ShellStatus::Exited { code: Some(0) });
    assert_eq!(execution.stdout.trim(), "nested");
}

#[tokio::test]
async fn the_real_runner_reports_a_timeout_as_its_own_status() {
    let dir = tempfile::tempdir().expect("tempdir");
    let execution = RealShellRunner
        .run(ShellRequest {
            command: "printf partial; sleep 30".to_string(),
            cwd: dir.path().to_path_buf(),
            timeout: Duration::from_millis(200),
            agent_id: "root".to_string(),
            origin: GgShellOrigin::Tool,
        })
        .await;

    // A gg-side ceiling, not the command failing — which is why it is its own status rather than an
    // absent exit code, indistinguishable from the signal-terminated case.
    //
    // Nothing is asserted about the streams: after the kill the runner waits only a short grace for
    // the reader tasks (a forked grandchild can hold the write end open indefinitely), so whatever
    // the process had printed is recovered on a best-effort basis and a test that demanded it would
    // be asserting a race. That is a property of the runner rather than of the seam.
    assert_eq!(execution.status, ShellStatus::TimedOut);
}

// ---------------------------------------------------------------------------
// The real runner's process tree: what a timeout kills and what an exit leaves running
// ---------------------------------------------------------------------------

/// Whether `pid` names a process that is still running: it exists and is not a zombie waiting to be
/// reaped (a killed process whose parent has not collected it yet is dead for every purpose here).
#[cfg(unix)]
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

/// Poll until `pid` is no longer running, for at most `bound`. Answers whether it stopped. The kill
/// was already sent; what is waited for is only the kernel and the reaper finishing with it, so the
/// poll sleeps between looks rather than spinning.
#[cfg(unix)]
async fn stops_within(pid: libc::pid_t, bound: Duration) -> bool {
    let deadline = std::time::Instant::now() + bound;
    loop {
        if !is_running(pid) {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Read the pid a command wrote to `file`.
#[cfg(unix)]
fn pid_in(file: &std::path::Path) -> libc::pid_t {
    std::fs::read_to_string(file)
        .unwrap_or_else(|err| panic!("the command should have written {}: {err}", file.display()))
        .trim()
        .parse()
        .expect("a pid")
}

/// Kill `pid` if it is still there, so a failed assertion leaves nothing running behind the test.
#[cfg(unix)]
struct KillOnDrop(libc::pid_t);

#[cfg(unix)]
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        // SAFETY: as in `is_running`; a pid that is already gone answers `ESRCH`, which is ignored.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
        }
    }
}

/// A request for `command` in `cwd` under a `timeout` ceiling.
#[cfg(unix)]
fn timed(command: &str, cwd: &std::path::Path, timeout: Duration) -> ShellRequest {
    ShellRequest {
        timeout,
        ..request(command, cwd, "root")
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_timeout_kills_the_grandchild_sh_left_behind_too() {
    let dir = tempfile::tempdir().expect("tempdir");
    // `sh` forks a `sleep` that inherits both pipes, records its pid, and waits on it — the shape
    // in which killing `sh` alone would orphan the `sleep`, still holding the pipe.
    let execution = RealShellRunner
        .run(timed(
            "sleep 300 & echo $! > grandchild.pid; wait",
            dir.path(),
            Duration::from_secs(1),
        ))
        .await;
    let grandchild = pid_in(&dir.path().join("grandchild.pid"));
    let _cleanup = KillOnDrop(grandchild);

    assert_eq!(execution.status, ShellStatus::TimedOut);
    assert!(
        stops_within(grandchild, Duration::from_secs(10)).await,
        "the timeout kill must reach the process group, not `sh` alone: {grandchild} is still running"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_timeout_kills_a_process_a_child_spawned_with_its_own_pipes() {
    let dir = tempfile::tempdir().expect("tempdir");
    // The shape that wedged a run: a program that spawns a long-lived child on pipes of its own and
    // keeps running because of it. `node` where there is one (the case as it was met); a subshell
    // holding a backgrounded `sleep` otherwise, which is the same tree.
    let has_node = std::process::Command::new("node")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    let command = if has_node {
        "node -e \"const c = require('child_process').spawn('sleep', ['3000'], { stdio: 'pipe' }); \
         require('fs').writeFileSync('spawned.pid', String(c.pid))\""
    } else {
        "sh -c 'sleep 3000 </dev/null >/dev/null 2>&1 & echo $! > spawned.pid; wait'"
    };
    let execution = RealShellRunner
        .run(timed(command, dir.path(), Duration::from_secs(2)))
        .await;
    let spawned = pid_in(&dir.path().join("spawned.pid"));
    let _cleanup = KillOnDrop(spawned);

    assert_eq!(execution.status, ShellStatus::TimedOut);
    assert!(
        stops_within(spawned, Duration::from_secs(10)).await,
        "the process the command's child spawned must die with the group: {spawned} is still running"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn an_exit_returns_while_a_backgrounded_process_holds_the_pipe_and_leaves_it_running() {
    let dir = tempfile::tempdir().expect("tempdir");
    let started = std::time::Instant::now();
    // `sh` exits at once; the `sleep` it sent to the background inherited stdout and stderr and holds
    // both open for as long as it lives — a dev server started with `&`, in miniature.
    let execution = RealShellRunner
        .run(timed(
            "sleep 300 & echo $! > background.pid; exit 0",
            dir.path(),
            Duration::from_secs(60),
        ))
        .await;
    let elapsed = started.elapsed();
    let background = pid_in(&dir.path().join("background.pid"));
    let _cleanup = KillOnDrop(background);

    assert_eq!(execution.status, ShellStatus::Exited { code: Some(0) });
    // Bounded by the output grace, not by the background process. The bound is generous against a
    // slow machine; what it rules out is waiting on the `sleep`'s 300 seconds.
    assert!(
        elapsed < Duration::from_secs(10),
        "the call should return within the output grace of the exit, took {elapsed:?}"
    );
    // An ordinary exit kills nothing, and the drain left behind keeps the pipe open, so the process
    // the command meant to leave running is still running.
    assert!(
        is_running(background),
        "a process deliberately sent to the background must survive the call: {background}"
    );
}

#[tokio::test]
async fn the_real_runner_reports_a_launch_failure_with_empty_streams() {
    let missing = std::path::Path::new("/nonexistent-directory-for-the-seam-test");
    let execution = RealShellRunner.run(request("true", missing, "root")).await;

    match &execution.status {
        ShellStatus::LaunchFailed { failure, message } => {
            assert_eq!(*failure, crate::tools::ToolFailure::NotFound);
            assert!(
                message.starts_with("failed to launch shell command:"),
                "unexpected launch-failure message: {message}"
            );
        }
        other => panic!("expected a launch failure, got {other:?}"),
    }
    // Nothing ran, so there is nothing it printed — the distinction from a command that ran and
    // printed nothing.
    assert!(execution.stdout.is_empty());
    assert!(execution.stderr.is_empty());
}

// ---------------------------------------------------------------------------
// The seam itself
// ---------------------------------------------------------------------------

#[test]
fn a_bare_tool_context_runs_commands_for_real() {
    let ctx = ToolContext::new("/workspace");
    // The default has to fall this way: a context that was never told otherwise runs real
    // commands, so no caller can end up with a stubbed shell by forgetting to ask for a real one.
    assert_eq!(format!("{:?}", ctx.shell), format!("{:?}", real_shell()));
    assert!(ctx.agent_id.is_empty());
}

#[tokio::test]
async fn a_substituted_runner_replaces_the_process_entirely() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stub = Arc::new(StubShellRunner::exiting(0, "answered from the stub"));
    let ctx = ToolContext::new(dir.path())
        .with_agent("agent-7")
        .with_shell(stub.clone());

    let outcome = run_command(
        "touch the-command-really-ran",
        Duration::from_secs(5),
        &OffloadPolicy::Inline,
        &ctx,
        GgShellOrigin::Tool,
    )
    .await;

    assert!(outcome.ok);
    assert!(
        outcome.output.contains("answered from the stub"),
        "the stub's output should be what the model reads: {}",
        outcome.output
    );
    // The whole point: the command did not run. A substituted runner that left this file behind
    // would be building a tree out of side effects it claims to have stubbed.
    assert!(
        !dir.path().join("the-command-really-ran").exists(),
        "a substituted runner must not start the process"
    );
}

#[tokio::test]
async fn a_substituted_runner_is_told_the_command_the_directory_and_the_agent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stub = Arc::new(StubShellRunner::exiting(0, ""));
    let ctx = ToolContext::new(dir.path())
        .with_agent("agent-7")
        .with_shell(stub.clone());

    run_command(
        "npm run build",
        Duration::from_secs(11),
        &OffloadPolicy::Inline,
        &ctx,
        GgShellOrigin::Tool,
    )
    .await;

    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].command, "npm run build");
    assert_eq!(requests[0].cwd, dir.path());
    assert_eq!(requests[0].timeout, Duration::from_secs(11));
    // The named property: the seam carries the *calling agent*. A runner given only a workspace
    // path could not know whose recorded commands to draw from, and every shell divergence has to
    // name an agent.
    assert_eq!(requests[0].agent_id, "agent-7");
}

#[tokio::test]
async fn the_callers_output_policy_still_applies_to_a_substituted_runner() {
    let dir = tempfile::tempdir().expect("tempdir");
    let stub = Arc::new(StubShellRunner::exiting(0, &"x".repeat(64 * 1024)));
    let ctx = ToolContext::new(dir.path()).with_shell(stub.clone());

    let outcome = run_command(
        "cat huge.log",
        Duration::from_secs(5),
        &OffloadPolicy::Inline,
        &ctx,
        GgShellOrigin::Tool,
    )
    .await;

    // The line the seam is drawn on: the runner owns what the process *did*, and gg's presentation
    // of it — the merge, the cap, the truncation note — stays this build of gg's, so a session
    // played back through a newer gg shows the newer gg's footer over the recorded bytes.
    assert!(
        outcome.output.contains("[output truncated to"),
        "gg's own truncation must still apply: {}",
        &outcome.output[..200.min(outcome.output.len())]
    );
}

#[test]
fn re_rooting_a_context_carries_the_agent_and_the_shell_across() {
    let stub = Arc::new(StubShellRunner::exiting(0, ""));
    let base = ToolContext::new("/workspace")
        .with_agent("agent-7")
        .with_shell(stub.clone());

    let derived = base.rooted_at("/workspace/web");

    assert_eq!(
        derived.workspace_dir,
        std::path::Path::new("/workspace/web")
    );
    // A validation command declaring a `cwd` is still that agent's command, and still runs through
    // that agent's shell. Building a fresh context for it would silently restore the real one.
    assert_eq!(derived.agent_id, "agent-7");
    assert_eq!(format!("{:?}", derived.shell), format!("{:?}", stub));
}

// ---------------------------------------------------------------------------
// What a substituted runner's failures become
// ---------------------------------------------------------------------------

/// Invoke the shell tool over a fresh temp workspace whose runner is `stub`, going through the JSON
/// adapter so both it and `run_command` are on the path.
async fn invoke_with(stub: StubShellRunner, args: serde_json::Value) -> ToolOutcome {
    let dir = tempfile::tempdir().expect("tempdir");
    let ctx = ToolContext::new(dir.path()).with_shell(Arc::new(stub));
    ShellTool::new(OffloadPolicy::Inline)
        .invoke(args, &ctx)
        .await
}

#[tokio::test]
async fn a_launch_failure_is_a_failure_of_the_call_and_carries_no_sidecar() {
    let outcome = invoke_with(
        StubShellRunner::launch_failing(
            ToolFailure::IoError,
            "failed to launch shell command: eek",
        ),
        json!({ "command": "npm run build" }),
    )
    .await;

    assert!(!outcome.ok);
    assert_eq!(outcome.failure, Some(ToolFailure::IoError));
    // Nothing ran, so the runner's message is the whole story: no exit code line, and no
    // `ShellData` claiming a process reported something.
    assert_eq!(outcome.output, "failed to launch shell command: eek");
    assert!(
        outcome.data.is_none(),
        "a launch failure has no process facts to attach: {:?}",
        outcome.data
    );
}

#[tokio::test]
async fn a_launch_failure_keeps_the_runners_classification() {
    let outcome = invoke_with(
        StubShellRunner::launch_failing(ToolFailure::NotFound, "no such directory"),
        json!({ "command": "npm run build" }),
    )
    .await;

    // The runner classified it; `run_command` passes that judgement through rather than
    // re-deriving one from the message it was handed.
    assert_eq!(outcome.failure, Some(ToolFailure::NotFound));
}

#[tokio::test]
async fn a_wait_failure_is_a_failure_of_the_call_and_carries_no_sidecar() {
    let outcome = invoke_with(
        StubShellRunner::wait_failing(
            ToolFailure::IoError,
            "failed to wait for shell command: eek",
        ),
        json!({ "command": "npm run build" }),
    )
    .await;

    assert!(!outcome.ok);
    assert_eq!(outcome.failure, Some(ToolFailure::IoError));
    assert_eq!(outcome.output, "failed to wait for shell command: eek");
    assert!(
        outcome.data.is_none(),
        "a wait failure never learned what the process did: {:?}",
        outcome.data
    );
}

#[tokio::test]
async fn a_signal_terminated_command_reports_no_exit_code() {
    let outcome = invoke_with(
        StubShellRunner::signalled(),
        json!({ "command": "sleep 30" }),
    )
    .await;

    assert!(!outcome.ok);
    // The process *ran*, so nothing about the call went wrong: `failure` stays empty and the
    // absent code is reported as the facts of a process that was killed.
    assert_eq!(outcome.failure, None);
    assert_eq!(outcome.summary.as_deref(), Some("terminated by signal"));
    match outcome.data.as_ref() {
        Some(crate::tools::ApiData::Shell(data)) => assert_eq!(data.exit_code, None),
        other => panic!("expected shell data, got {other:?}"),
    }
}
