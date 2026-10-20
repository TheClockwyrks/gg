//! The **shell seam**: the one thing in gg that actually starts a process, behind a trait.
//!
//! A gg session has exactly two inputs that are not a function of its own state — the model call
//! and the shell — and between them they account for ~all of a run's wall clock and ~all of its
//! cost. The model call has had a seam since the beginning (the
//! [client factory](crate::client::ClientFactory) every agent resolves through); this module gives
//! the shell the matching one, so gg's own suite can drive the loop against a substituted runner
//! instead of starting real processes.
//!
//! # Why the seam is here and not at the tool
//!
//! Three call paths reach a command line — the [`shell`](super::SHELL_TOOL) tool, a
//! [responses-as-code](crate::sandbox) program's `system.shell(…)`, and a
//! [hook's](crate::hooks) commands — and the only thing all three share
//! is the [`ToolContext`](super::ToolContext). So the runner travels on the context, and the one
//! function all three go through — [`run_command`](super::run_command), which each reaches with its
//! own [origin](GgShellOrigin) — takes it from there. Splitting the seam
//! any lower (at the `shell` tool) would leave the other two paths starting real processes behind
//! a substituted seam; splitting it any higher would need three seams that could disagree.
//!
//! # What is on each side of it
//!
//! The runner owns **only the process**: spawn `sh -c`, drain both pipes, enforce the per-command
//! timeout, and report what happened. Everything a model actually reads — merging the two streams,
//! the [output policy](super::OffloadPolicy), gg's truncation notes, the [`ToolOutcome`] shape — is
//! on the *caller's* side, in [`shell`](super). That line is deliberate: a substituted runner
//! replaces what a command *did*, and inherits gg's presentation of it verbatim — so a test that
//! substitutes one is testing gg's presentation rather than re-implementing it.
//!
//! [`ToolOutcome`]: super::ToolOutcome

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use test_cabinet_contracts::gg_session_record::GgShellOrigin;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use super::ToolFailure;

/// Once the command has exited or been killed, how long to wait for both pipes to close before
/// reporting what was captured. A process the command left running in the background (or one that
/// left its process group) can hold a pipe open indefinitely; the call does not wait on it.
const OUTPUT_GRACE: Duration = Duration::from_millis(250);

/// One command line, as handed to whatever is going to run it.
///
/// Owned rather than borrowed because a substituted runner keeps what it was asked for — the pair
/// (command, directory) is what a test asserts against — and because the cost of two allocations is
/// nothing beside starting a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRequest {
    /// The command line, run as `sh -c <command>`.
    pub command: String,
    /// The directory to run it in. Already resolved: the calling agent's workspace root, or the
    /// directory a [hook command](crate::hooks) declared relative to it.
    pub cwd: PathBuf,
    /// gg's per-command ceiling, after any budget clamp the caller applied.
    pub timeout: Duration,
    /// The agent whose turn issued the command, empty for a dispatch with no agent behind it.
    ///
    /// Carried because a recorded shell is keyed **per agent**: a runner given only a workspace
    /// path cannot say whose command it just ran, every shell divergence names an
    /// agent, and a [hook's](crate::hooks) commands in particular have to be attributed to the
    /// agent it fired for — an [agent-stop](test_cabinet_contracts::gg::GgHookEvent::AgentStop) gate to
    /// the agent whose ending it holds — or they land on an unattributed queue.
    /// [`RealShellRunner`] has no use for it — a real process is a real process — so the only
    /// reader is the [recording](crate::capture::RecordingShellRunner) runner, which stamps it on
    /// the entry it appends.
    pub agent_id: String,
    /// **Which of gg's three command paths** this line came from: the [`shell`](super::SHELL_TOOL)
    /// tool, a [responses-as-code](crate::sandbox) program's `system.shell(…)`, or a
    /// [hook's](crate::hooks) commands.
    ///
    /// Carried on the request rather than derived at the recorder, because the seam is the *only*
    /// place all three meet and by the time a command reaches it the caller is gone. It is what
    /// keeps a hook's command off the agent's ordinary queue — a hook command and
    /// a `shell` tool call are different inputs even when the text is identical — and it is the one
    /// field [`RealShellRunner`] ignores and a
    /// [recording](crate::capture::RecordingShellRunner) one exists for.
    pub origin: GgShellOrigin,
}

/// How a command's process ended, as the [runner](ShellRunner) saw it.
///
/// Four cases rather than an exit code, because the three that are not an ordinary exit are
/// presented to the model differently and one of them ([`LaunchFailed`](Self::LaunchFailed)) never
/// reaches the [output policy](super::OffloadPolicy) at all. Collapsing them into
/// `Option<i32>` — which is what the caller-facing
/// [`ShellData::exit_code`](super::ShellData::exit_code) does, once the presentation above the seam
/// has already said which case it was — would make a timeout and a signal-terminated process
/// indistinguishable here, where the distinction is the whole point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellStatus {
    /// The process ran to completion. `code` is `None` for one a signal ended.
    Exited {
        /// Its exit status, or `None` when a signal ended it.
        code: Option<i32>,
    },
    /// The [request's](ShellRequest::timeout) ceiling expired and gg killed the process group.
    /// Whatever it had already printed is still reported.
    TimedOut,
    /// The process never started — `sh` could not be spawned. The one shell failure that is a
    /// failure *of the call* rather than a result of it, so it carries the classified failure and
    /// the message the model is told, and its streams are empty.
    LaunchFailed {
        /// How the failure is classified to a structured caller.
        failure: ToolFailure,
        /// The model-facing message.
        message: String,
    },
    /// The process started but `wait()` itself failed — rare, and reported rather than papered
    /// over as a success.
    WaitFailed {
        /// How the failure is classified to a structured caller.
        failure: ToolFailure,
        /// The model-facing message.
        message: String,
    },
}

/// What one command's process did, **before** any output policy touched it — the whole of what a
/// [`ShellRunner`] answers with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellExecution {
    /// How it ended.
    pub status: ShellStatus,
    /// Everything it wrote to standard output.
    pub stdout: String,
    /// Everything it wrote to standard error.
    pub stderr: String,
}

impl ShellExecution {
    /// The execution of a command that never ran at all. Distinct from one that ran and printed
    /// nothing, which is why [`LaunchFailed`](ShellStatus::LaunchFailed) exists rather than an
    /// absent exit code.
    fn never_ran(failure: ToolFailure, message: String) -> Self {
        Self {
            status: ShellStatus::LaunchFailed { failure, message },
            stdout: String::new(),
            stderr: String::new(),
        }
    }
}

/// Runs a command line and reports what the process did.
///
/// The **only** implementation in a live run is [`RealShellRunner`]; the reason the trait exists is
/// that gg's own suite substitutes one that answers without starting a process. `Debug` is required so the
/// [`ToolContext`](super::ToolContext) carrying it stays printable, which several tool errors rely
/// on.
#[async_trait]
pub trait ShellRunner: Send + Sync + std::fmt::Debug {
    /// Run `request` to completion and report what happened. Never fails: a process that could not
    /// even be started is a [`LaunchFailed`](ShellStatus::LaunchFailed) execution, so every caller
    /// has exactly one shape to handle.
    async fn run(&self, request: ShellRequest) -> ShellExecution;
}

/// The production [`ShellRunner`]: `sh -c` in the requested directory, its own process group, both
/// pipes drained concurrently with the wait, the whole group killed on the request's ceiling.
///
/// The call returns at most [`OUTPUT_GRACE`] after `sh` exits or is killed, whatever is still
/// holding the pipes. On an ordinary exit nothing is killed: a process the command deliberately
/// sent to the background (a dev server) keeps running, and its output keeps being drained — and
/// discarded — so it never meets a closed pipe.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealShellRunner;

/// The one [`RealShellRunner`] every [`ToolContext`](super::ToolContext) that was not handed a
/// substitute points at.
///
/// A shared singleton rather than a fresh `Arc` per context because the runner is a zero-sized
/// unit: gg builds a tool context per agent *per turn*, and there is nothing to own.
static REAL_SHELL: LazyLock<Arc<dyn ShellRunner>> = LazyLock::new(|| Arc::new(RealShellRunner));

/// The shared [`RealShellRunner`] handle — what an unsubstituted
/// [`ToolContext`](super::ToolContext) runs commands through.
pub fn real_shell() -> Arc<dyn ShellRunner> {
    Arc::clone(&REAL_SHELL)
}

#[async_trait]
impl ShellRunner for RealShellRunner {
    async fn run(&self, request: ShellRequest) -> ShellExecution {
        let ShellRequest {
            command,
            cwd,
            timeout,
            ..
        } = request;

        let mut command_builder = Command::new("sh");
        command_builder
            .arg("-c")
            .arg(&command)
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Reap the child if we drop it (e.g. on an early return).
            .kill_on_drop(true);
        // Put the command in its own process group so a timeout kill reaches the whole
        // tree — `sh` may fork the command into a grandchild that would otherwise survive
        // (and hold the output pipe open) when only `sh` is killed.
        #[cfg(unix)]
        command_builder.process_group(0);

        let mut child = match command_builder.spawn() {
            Ok(child) => child,
            Err(err) => {
                return ShellExecution::never_ran(
                    ToolFailure::from_io(&err),
                    format!("failed to launch shell command: {err}"),
                );
            }
        };

        // Drain both pipes concurrently with the wait so a command that fills a pipe
        // buffer cannot deadlock, and partial output survives a timeout kill. Each reader
        // appends to a capture this side can snapshot without waiting for the pipe's EOF.
        let out_capture = Capture::default();
        let err_capture = Capture::default();
        let out_task = tokio::spawn(drain(child.stdout.take(), out_capture.clone()));
        let err_task = tokio::spawn(drain(child.stderr.take(), err_capture.clone()));

        let waited = tokio::time::timeout(timeout, child.wait()).await;

        if waited.is_err() {
            // The command overran its ceiling: kill its whole process group, so a grandchild
            // `sh` forked cannot outlive the call holding the pipe. The kill is not awaited.
            kill_process_group(&mut child);
        }

        // Collect the captured output, bounded either way. On an ordinary exit the readers
        // usually see EOF at once; but a process the command sent to the background (`npm run
        // dev &`) inherited the pipe and holds it open for as long as it lives, and on a timeout
        // a process that left the group may do the same. So the call waits at most a short grace
        // for the EOF, then reports what was captured by then.
        //
        // The readers are deliberately NOT aborted when the grace runs out: they are left
        // draining, detached, until the pipe closes. Closing the read end instead would hand the
        // backgrounded process `SIGPIPE` (or `EPIPE`) the next time it writes — a server the
        // model started on purpose would die of the call having returned. Nor is the group
        // killed on an ordinary exit, for the same reason.
        let _ = tokio::time::timeout(OUTPUT_GRACE, async {
            let _ = out_task.await;
            let _ = err_task.await;
        })
        .await;
        let stdout = out_capture.take();
        let stderr = err_capture.take();

        let status = match waited {
            Ok(Ok(status)) => ShellStatus::Exited {
                code: status.code(),
            },
            // `child.wait()` itself failed (rare): report it rather than pretend success.
            Ok(Err(err)) => ShellStatus::WaitFailed {
                failure: ToolFailure::from_io(&err),
                message: format!("waiting on shell command: {err}"),
            },
            Err(_) => ShellStatus::TimedOut,
        };

        ShellExecution {
            status,
            stdout,
            stderr,
        }
    }
}

/// Kill a timed-out command's whole process group — `sh` and everything it started that is still
/// in the group — with `SIGKILL`.
///
/// The command was spawned with [`process_group(0)`](std::os::unix::process::CommandExt::process_group),
/// so `sh`'s pid is the group's id. The child has not been reaped (its wait timed out), so that id
/// cannot have been reused by an unrelated group. Should the group signal fail for any reason, `sh`
/// itself is still killed, which is what the runner did before it signalled the group.
#[cfg(unix)]
fn kill_process_group(child: &mut tokio::process::Child) {
    let signalled = child
        .id()
        .and_then(|pid| libc::pid_t::try_from(pid).ok())
        .is_some_and(|pgid| {
            // SAFETY: `kill` takes two integers and touches no memory of this process; a negative
            // pid addresses the process group whose id is its absolute value.
            unsafe { libc::kill(-pgid, libc::SIGKILL) == 0 }
        });
    if !signalled {
        let _ = child.start_kill();
    }
}

/// Where there are no process groups, kill the one process there is.
#[cfg(not(unix))]
fn kill_process_group(child: &mut tokio::process::Child) {
    let _ = child.start_kill();
}

/// One pipe's captured bytes, shared between the reader draining the pipe and the runner reporting
/// on it.
///
/// Shared rather than returned from the reader, because the runner reports after at most
/// [`OUTPUT_GRACE`] whether or not the pipe has closed: what a reader still running has already read
/// is part of the command's output. Once [taken](Self::take), the reader keeps reading — so the
/// writer never meets a closed pipe — but discards what it reads, so a server left running in the
/// background cannot grow a buffer nobody will read.
#[derive(Debug, Clone, Default)]
struct Capture(Arc<Mutex<CaptureState>>);

#[derive(Debug, Default)]
struct CaptureState {
    bytes: Vec<u8>,
    taken: bool,
}

impl Capture {
    /// Append `chunk`, unless the capture has already been reported.
    fn push(&self, chunk: &[u8]) {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.taken {
            state.bytes.extend_from_slice(chunk);
        }
    }

    /// Everything captured so far, as lossy UTF-8 (build output is not guaranteed valid UTF-8);
    /// from here on the reader discards what it reads.
    fn take(&self) -> String {
        let mut state = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        state.taken = true;
        let bytes = std::mem::take(&mut state.bytes);
        String::from_utf8_lossy(&bytes).into_owned()
    }
}

/// Read a captured pipe to EOF into `capture`. A `None` handle (unexpected) reads as empty, and a
/// read error ends the drain keeping whatever was captured before it.
async fn drain<R>(reader: Option<R>, capture: Capture)
where
    R: AsyncReadExt + Unpin,
{
    let Some(mut reader) = reader else {
        return;
    };
    let mut chunk = vec![0u8; 8 * 1024];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => capture.push(&chunk[..n]),
        }
    }
}

/// A [`ShellRunner`] that starts nothing: it records every [request](ShellRequest) it is handed and
/// answers each with the same canned [execution](ShellExecution).
///
/// The test double for the seam itself, kept beside the trait rather than in one test file because
/// three of gg's modules need it — the seam is only meaningful if the `shell` tool, a
/// [responses-as-code](crate::sandbox) program and a [hook](crate::hooks) all
/// reach it, and proving that means substituting it from each of their tests.
#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct StubShellRunner {
    /// Every request handed to [`run`](ShellRunner::run), in order.
    requests: Arc<std::sync::Mutex<Vec<ShellRequest>>>,
    /// What every one of them is answered with.
    answer: ShellExecution,
}

#[cfg(test)]
impl StubShellRunner {
    /// A stub answering every command with exit `code` and `stdout`, and nothing on stderr.
    pub(crate) fn exiting(code: i32, stdout: &str) -> Self {
        Self {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            answer: ShellExecution {
                status: ShellStatus::Exited { code: Some(code) },
                stdout: stdout.to_string(),
                stderr: String::new(),
            },
        }
    }

    /// A stub answering every command with `status` and empty streams.
    fn answering(status: ShellStatus) -> Self {
        Self {
            requests: Arc::new(std::sync::Mutex::new(Vec::new())),
            answer: ShellExecution {
                status,
                stdout: String::new(),
                stderr: String::new(),
            },
        }
    }

    /// A stub answering every command with a launch failure classified `failure` and reported as
    /// `message` — the process never started, so both streams are empty.
    pub(crate) fn launch_failing(failure: crate::tools::ToolFailure, message: &str) -> Self {
        Self::answering(ShellStatus::LaunchFailed {
            failure,
            message: message.to_string(),
        })
    }

    /// A stub answering every command with a wait failure classified `failure` and reported as
    /// `message`: the process started but gg never learned what it did.
    pub(crate) fn wait_failing(failure: crate::tools::ToolFailure, message: &str) -> Self {
        Self::answering(ShellStatus::WaitFailed {
            failure,
            message: message.to_string(),
        })
    }

    /// A stub answering every command with a process that exited without a code — the shape a
    /// signal-terminated command has.
    pub(crate) fn signalled() -> Self {
        Self::answering(ShellStatus::Exited { code: None })
    }

    /// Every request it has been handed, in order.
    pub(crate) fn requests(&self) -> Vec<ShellRequest> {
        self.requests.lock().expect("stub shell lock").clone()
    }
}

#[cfg(test)]
#[async_trait]
impl ShellRunner for StubShellRunner {
    async fn run(&self, request: ShellRequest) -> ShellExecution {
        self.requests.lock().expect("stub shell lock").push(request);
        self.answer.clone()
    }
}

#[cfg(test)]
#[path = "shell.runner.test.rs"]
mod tests;
