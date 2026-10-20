//! **Heartbeats from the loop**: the agent's own waits — a model call, a tool call — beat on its
//! stream while they last.
//!
//! The beat itself (its cadence, the per-agent guard) is
//! [`Emitter::while_waiting`]'s and is tested beside it; what is tested here is that the loop runs
//! its waits through it. Driven on tokio's paused clock with a model and a shell that take their
//! time on that clock, so a two-and-a-half-minute wait costs nothing and beats on exact instants.

use super::*;

/// A [`MockClient`] that keeps its caller waiting `delay` on the runtime's clock before every
/// reply — a slow provider.
struct SlowClient {
    inner: MockClient,
    delay: std::time::Duration,
}

#[async_trait::async_trait]
impl ModelClient for SlowClient {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolDefinition],
    ) -> Result<ModelResponse, ModelError> {
        tokio::time::sleep(self.delay).await;
        self.inner.complete(messages, tools).await
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }
}

/// A shell that keeps its caller waiting `delay` on the runtime's clock, then reports a clean exit
/// — a long build.
#[derive(Debug)]
struct SlowShell {
    delay: std::time::Duration,
}

#[async_trait::async_trait]
impl crate::tools::ShellRunner for SlowShell {
    async fn run(&self, _request: crate::tools::ShellRequest) -> crate::tools::ShellExecution {
        tokio::time::sleep(self.delay).await;
        crate::tools::ShellExecution {
            status: crate::tools::ShellStatus::Exited { code: Some(0) },
            stdout: "built".to_string(),
            stderr: String::new(),
        }
    }
}

/// [`drive_hooked`] with a caller-built [`ToolContext`], so a test can put a shell of its own under
/// the loop.
async fn drive_with(
    client: &dyn ModelClient,
    ctx: &ToolContext,
    registry: &ToolRegistry,
    emitter: &Emitter,
) -> LoopEnd {
    Agent::root(ROOT_PROFILE_ID)
        .drive(
            client,
            "go",
            registry,
            ctx,
            emitter,
            &mut test_modules(test_context_setup(), false)
                .with(ModuleHandle::Skills(SkillsRuntime::disabled()))
                .with(ModuleHandle::Memories(MemoriesRuntime::disabled()))
                .with(ModuleHandle::Tasks(TasksRuntime::disabled()))
                .with(ModuleHandle::Board(BoardRuntime::disabled())),
            DriveSetup {
                limits: no_limits(5),
                compaction: no_compaction(),
                amc: no_amc(),
                autoload: no_autoload(),
                persistence: no_persistence(),
                read_policy: Some(ReadPolicy::Unlimited),
                shell_offload: OffloadPolicy::ample(),
                code: no_code(),
                discovery: DiscoveryWarning::default(),
                hooks: no_hooks(),
                ending_role: EndingRole::Standard,
                opening: Opening::Fresh,
                carried_programs: None,
                turn_base: 0,
                replay: None,
            },
            &[],
            &GgAgentConfig::root(),
            &GgCapabilitySet::default(),
            &mut None,
            None,
        )
        .await
}

/// Every heartbeat on `sink`, as `(agentId, waitingOn, waitedMs, detail)`.
fn beats(sink: &CollectingSink) -> Vec<(Option<String>, GgWaitKind, u64, Option<String>)> {
    sink.events()
        .into_iter()
        .filter_map(|event| match event.kind {
            GgTelemetryKind::Heartbeat {
                waiting_on,
                waited_ms,
                detail,
            } => Some((event.agent_id, waiting_on, waited_ms, detail)),
            _ => None,
        })
        .collect()
}

/// A model that takes two and a half minutes to answer keeps its agent beating through the call:
/// two beats, on the agent's stream, naming the model.
#[tokio::test(start_paused = true)]
async fn a_slow_model_call_beats_while_the_agent_waits() {
    let dir = TempDir::new().unwrap();
    let registry = ToolRegistry::from_capabilities(GgCapabilitySet::minimal("mock/x").root());
    let sink = CollectingSink::new();
    let emitter = Emitter::with_sink(None, Box::new(sink.clone())).for_agent("root", None);
    let client = SlowClient {
        inner: MockClient::new("mock/x", vec![finish_call("f1", "done")]),
        delay: std::time::Duration::from_secs(150),
    };

    let end = drive_with(&client, &ToolContext::new(dir.path()), &registry, &emitter).await;

    assert_eq!(end.status, "completed");
    let root = Some("root".to_string());
    let model = Some("mock/x".to_string());
    assert_eq!(
        beats(&sink),
        vec![
            (
                root.clone(),
                GgWaitKind::ModelRequest,
                60_000,
                model.clone()
            ),
            (root, GgWaitKind::ModelRequest, 120_000, model),
        ]
    );
}

/// A command that runs two and a half minutes keeps its agent beating through the tool call, named
/// by the tool; the quick model calls either side of it say nothing.
#[tokio::test(start_paused = true)]
async fn a_slow_tool_call_beats_with_the_tools_name() {
    let dir = TempDir::new().unwrap();
    let registry = ToolRegistry::from_capabilities(GgCapabilitySet::minimal("mock/x").root());
    let sink = CollectingSink::new();
    let emitter = Emitter::with_sink(None, Box::new(sink.clone())).for_agent("root", None);
    let client = MockClient::new(
        "mock/x",
        vec![
            ending_call("s1", "shell", json!({ "command": "make" })),
            finish_call("f1", "built"),
        ],
    );
    let ctx = ToolContext::new(dir.path()).with_shell(Arc::new(SlowShell {
        delay: std::time::Duration::from_secs(150),
    }));

    let end = drive_with(&client, &ctx, &registry, &emitter).await;

    assert_eq!(end.status, "completed");
    let root = Some("root".to_string());
    let shell = Some("shell".to_string());
    assert_eq!(
        beats(&sink),
        vec![
            (root.clone(), GgWaitKind::ToolCall, 60_000, shell.clone()),
            (root, GgWaitKind::ToolCall, 120_000, shell),
        ]
    );
}

/// A run whose waits are all short beats not at all: the stream of an ordinary quick session is
/// exactly what it was before heartbeats existed.
#[tokio::test(start_paused = true)]
async fn a_quick_session_never_beats() {
    let dir = TempDir::new().unwrap();
    let registry = ToolRegistry::from_capabilities(GgCapabilitySet::minimal("mock/x").root());
    let sink = CollectingSink::new();
    let emitter = Emitter::with_sink(None, Box::new(sink.clone())).for_agent("root", None);
    let client = SlowClient {
        inner: MockClient::new(
            "mock/x",
            vec![
                ending_call("s1", "shell", json!({ "command": "make" })),
                finish_call("f1", "built"),
            ],
        ),
        delay: std::time::Duration::from_secs(59),
    };
    let ctx = ToolContext::new(dir.path()).with_shell(Arc::new(SlowShell {
        delay: std::time::Duration::from_secs(59),
    }));

    let end = drive_with(&client, &ctx, &registry, &emitter).await;

    assert_eq!(end.status, "completed");
    assert!(beats(&sink).is_empty());
}
