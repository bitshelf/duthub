//! A2A executor: turns `DataPart` skill requests into evidence tool calls.
//!
//! Contract (ADR-9):
//! - input is a single `DataPart` `{skill, dutId?, args}`; TextPart-only
//!   messages, unknown skills, non-object args and cross-DUT requests fail
//!   the task with `TASK_STATE_FAILED` + a policy reason (protocol-level
//!   failure, not a transport error);
//! - `messageId` is deduplicated in-process: a replay carries the recorded
//!   artifact result without re-running the tool (new taskId, same payload);
//! - task IDs are registered into [`TaskOwnership`] for the authorizer;
//! - `with_test_delay` is a test-only hook that keeps a task in `Working`
//!   long enough to exercise `CancelTask`.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a::{
    Artifact, Message, Part, PartContent, Role, StreamResponse, TaskArtifactUpdateEvent, TaskState,
    TaskStatus, TaskStatusUpdateEvent, new_artifact_id,
};
use a2a_server::executor::{AgentExecutor, ExecutorContext};
use duthub_core::audit::{AuditOutcome, AuditSink};
use duthub_mcp::state::McpState;
use futures::SinkExt;
use futures::stream::BoxStream;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::evidence;
use crate::task::TaskOwnership;

/// Upper bound for the in-process messageId dedup table (ADR-9).
const DEDUP_CAP: usize = 4096;

/// Poll interval while waiting inside the test-only delay window.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Client skill request carried in a `DataPart` (camelCase keys).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SkillRequest {
    skill: String,
    #[serde(default)]
    dut_id: Option<String>,
    #[serde(default)]
    args: serde_json::Value,
}

type DedupKey = (String, String, [u8; 32]);

/// Recorded results of previously executed `(message_id, skill, args_digest)`
/// tuples (soft, in-process, FIFO-evicted). Keying on skill + args digest
/// prevents a caller from replaying a cached result across mismatched payloads.
struct DedupState {
    results: HashMap<DedupKey, serde_json::Value>,
    order: VecDeque<DedupKey>,
}

impl DedupState {
    fn new() -> Self {
        Self {
            results: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn key(message_id: &str, skill: &str, args: &serde_json::Value) -> DedupKey {
        let serialized = serde_json::to_vec(args).unwrap_or_default();
        let digest: [u8; 32] = Sha256::digest(&serialized).into();
        (message_id.to_owned(), skill.to_owned(), digest)
    }

    fn get(&self, key: &DedupKey) -> Option<serde_json::Value> {
        self.results.get(key).cloned()
    }

    fn record(&mut self, key: DedupKey, result: &serde_json::Value) {
        if self.results.contains_key(&key) {
            return;
        }
        while self.results.len() >= DEDUP_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.results.remove(&oldest);
            } else {
                break;
            }
        }
        self.order.push_back(key.clone());
        self.results.insert(key, result.clone());
    }
}

/// Per-task cancellation flag: `cancel()` sets it, `execute()` checks it so a
/// canceled task never emits further events, and drops it on exit.
struct CancelFlags {
    flags: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicBool>>>,
}

impl CancelFlags {
    fn new() -> Self {
        Self {
            flags: Mutex::new(HashMap::new()),
        }
    }

    fn arm(&self, task_id: &str) -> Arc<std::sync::atomic::AtomicBool> {
        let mut flags = self
            .flags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        flags
            .entry(task_id.to_owned())
            .or_insert_with(|| Arc::new(std::sync::atomic::AtomicBool::new(false)))
            .clone()
    }

    fn set(&self, task_id: &str) {
        let mut flags = self
            .flags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(flag) = flags.get(task_id) {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
        } else {
            let flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
            flags.insert(task_id.to_owned(), flag);
        }
    }

    fn remove(&self, task_id: &str) {
        self.flags
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(task_id);
    }

    fn is_set(flag: &std::sync::atomic::AtomicBool) -> bool {
        flag.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// The Duthub A2A executor for one bound DUT.
#[derive(Clone)]
pub struct DuthubExecutor {
    state: Arc<dyn McpState>,
    dut_id: Arc<str>,
    ownership: Arc<TaskOwnership>,
    dedup: Arc<Mutex<DedupState>>,
    cancel_flags: Arc<CancelFlags>,
    audit: Option<Arc<AuditSink>>,
    /// Test-only: hold tasks in `Working` for this long before running the
    /// tool, so `CancelTask` can be exercised deterministically.
    test_delay: Option<Duration>,
}

impl std::fmt::Debug for DuthubExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DuthubExecutor")
            .field("dut_id", &self.dut_id)
            .field("ownership", &self.ownership)
            .field("has_audit", &self.audit.is_some())
            .field("test_delay", &self.test_delay)
            .finish_non_exhaustive()
    }
}

impl DuthubExecutor {
    pub fn new(state: Arc<dyn McpState>, dut_id: impl Into<Arc<str>>) -> Self {
        Self {
            state,
            dut_id: dut_id.into(),
            ownership: Arc::new(TaskOwnership::new()),
            dedup: Arc::new(Mutex::new(DedupState::new())),
            cancel_flags: Arc::new(CancelFlags::new()),
            audit: None,
            test_delay: None,
        }
    }

    /// Attach an audit sink so A2A skill requests emit structured audit lines.
    #[must_use]
    pub fn with_audit(mut self, audit: Arc<AuditSink>) -> Self {
        self.audit = Some(audit);
        self
    }

    /// **Test-only** hook: delay execution so a task stays `Working` and can
    /// be canceled. Never call from production code.
    #[must_use]
    pub fn with_test_delay(mut self, delay: Duration) -> Self {
        self.test_delay = Some(delay);
        self
    }

    fn record_audit(&self, skill: &str, outcome: AuditOutcome, detail: &str) {
        if let Some(audit) = &self.audit {
            let summary = format!("skill={skill} {detail}");
            audit.record(
                "a2a.skill",
                Some("a2a"),
                None,
                Some(&self.dut_id),
                outcome,
                Some(&summary),
            );
        }
    }

    /// Ownership registry shared with the request authorizer.
    #[must_use]
    pub fn ownership(&self) -> Arc<TaskOwnership> {
        self.ownership.clone()
    }

    /// The DUT this executor is bound to.
    #[must_use]
    pub fn dut_id(&self) -> Arc<str> {
        self.dut_id.clone()
    }

    fn status_event(task_id: &str, context_id: &str, state: TaskState) -> StreamResponse {
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.to_owned(),
            context_id: context_id.to_owned(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            metadata: None,
        })
    }

    fn failed_event(task_id: &str, context_id: &str, reason: &str) -> StreamResponse {
        let message = Message::new(Role::Agent, vec![Part::text(reason.to_owned())]);
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.to_owned(),
            context_id: context_id.to_owned(),
            status: TaskStatus {
                state: TaskState::Failed,
                message: Some(message),
                timestamp: None,
            },
            metadata: None,
        })
    }

    fn artifact_event(
        task_id: &str,
        context_id: &str,
        skill: &str,
        result: serde_json::Value,
    ) -> StreamResponse {
        StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: task_id.to_owned(),
            context_id: context_id.to_owned(),
            artifact: Artifact {
                artifact_id: new_artifact_id(),
                name: Some(skill.to_owned()),
                description: None,
                parts: vec![Part::data(result)],
                metadata: None,
                extensions: None,
            },
            append: None,
            last_chunk: Some(true),
            metadata: None,
        })
    }

    async fn run_execution(
        &self,
        ctx: ExecutorContext,
        sink: futures::channel::mpsc::Sender<Result<StreamResponse, a2a::A2AError>>,
    ) {
        let mut sink = sink;
        let task_id = ctx.task_id.clone();
        let context_id = ctx.context_id.clone();
        let message_id = ctx
            .message
            .as_ref()
            .map(|message| message.message_id.clone());
        self.ownership.register(&task_id, &self.dut_id);
        let cancel_flag = self.cancel_flags.arm(&task_id);

        // 1. Parse the skill request out of the message.
        let Some(request) = ctx.message.as_ref().and_then(parse_skill_request) else {
            self.record_audit("<invalid>", AuditOutcome::Denied, "missing_data_part");
            self.fail(
                &mut sink,
                &task_id,
                &context_id,
                "POLICY: message must carry one DataPart {skill, dutId?, args}; \
                 TextPart-only messages are rejected",
            )
            .await;
            self.ownership.forget(&task_id);
            self.cancel_flags.remove(&task_id);
            return;
        };

        // 2. Whitelist + cross-DUT + args shaping (policy layer).
        let args = match evidence::prepare_tool_args(
            &request.skill,
            &self.dut_id,
            request.dut_id.as_deref(),
            request.args.clone(),
        ) {
            Ok(args) => args,
            Err(error) => {
                self.record_audit(&request.skill, AuditOutcome::Denied, &error.message);
                self.fail(&mut sink, &task_id, &context_id, &error.message)
                    .await;
                self.ownership.forget(&task_id);
                self.cancel_flags.remove(&task_id);
                return;
            }
        };

        // 3. Working status (task is now observable as TASK_STATE_WORKING).
        if !self
            .emit(
                &mut sink,
                Self::status_event(&task_id, &context_id, TaskState::Working),
            )
            .await
        {
            self.cancel_flags.remove(&task_id);
            return;
        }

        // 4. Test-only delay window; cancellation during it stops silently.
        if let Some(delay) = self.test_delay {
            let deadline = tokio::time::Instant::now() + delay;
            loop {
                if CancelFlags::is_set(&cancel_flag) {
                    self.cancel_flags.remove(&task_id);
                    return;
                }
                if tokio::time::Instant::now() >= deadline {
                    break;
                }
                tokio::time::sleep(CANCEL_POLL_INTERVAL).await;
            }
        }

        // 5. messageId dedup keyed on `(message_id, skill, sha256(args))`:
        //    replay reuses the recorded artifact only when skill & args match.
        let dedup_key = message_id
            .as_deref()
            .map(|id| DedupState::key(id, &request.skill, &args));
        let recorded = dedup_key.as_ref().and_then(|key| {
            self.dedup
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(key)
        });

        let result = match recorded {
            Some(result) => {
                self.record_audit(&request.skill, AuditOutcome::Ok, "replayed=true");
                result
            }
            None => {
                match evidence::call_evidence_tool(&self.state, &self.dut_id, &request.skill, args)
                    .await
                {
                    Ok(result) => {
                        if let Some(key) = dedup_key {
                            self.dedup
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner)
                                .record(key, &result);
                        }
                        self.record_audit(&request.skill, AuditOutcome::Ok, "replayed=false");
                        result
                    }
                    Err(failure) => {
                        self.record_audit(&request.skill, AuditOutcome::Error, &failure.message);
                        self.fail(&mut sink, &task_id, &context_id, &failure.message)
                            .await;
                        self.ownership.forget(&task_id);
                        self.cancel_flags.remove(&task_id);
                        return;
                    }
                }
            }
        };

        if CancelFlags::is_set(&cancel_flag) {
            self.cancel_flags.remove(&task_id);
            return;
        }

        // 6. Artifact + terminal Completed status.
        let artifact = Self::artifact_event(&task_id, &context_id, &request.skill, result);
        if !self.emit(&mut sink, artifact).await {
            self.cancel_flags.remove(&task_id);
            return;
        }
        let _ = self
            .emit(
                &mut sink,
                Self::status_event(&task_id, &context_id, TaskState::Completed),
            )
            .await;
        self.cancel_flags.remove(&task_id);
    }

    async fn emit(
        &self,
        sink: &mut futures::channel::mpsc::Sender<Result<StreamResponse, a2a::A2AError>>,
        event: StreamResponse,
    ) -> bool {
        sink.send(Ok(event)).await.is_ok()
    }

    async fn fail(
        &self,
        sink: &mut futures::channel::mpsc::Sender<Result<StreamResponse, a2a::A2AError>>,
        task_id: &str,
        context_id: &str,
        reason: &str,
    ) {
        let _ = self
            .emit(sink, Self::failed_event(task_id, context_id, reason))
            .await;
    }
}

fn parse_skill_request(message: &Message) -> Option<SkillRequest> {
    message.parts.iter().find_map(|part| match &part.content {
        PartContent::Data(value) => serde_json::from_value::<SkillRequest>(value.clone()).ok(),
        _ => None,
    })
}

impl AgentExecutor for DuthubExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, a2a::A2AError>> {
        let this = self.clone();
        let (tx, rx) = futures::channel::mpsc::channel(8);
        tokio::spawn(async move {
            this.run_execution(ctx, tx).await;
        });
        Box::pin(rx)
    }

    fn cancel(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, a2a::A2AError>> {
        // Signal the running execution to stop emitting, then publish the
        // CANCELED status (DefaultRequestHandler applies it to the store).
        self.cancel_flags.set(&ctx.task_id);
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Canceled,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        Box::pin(futures::stream::once(async move { Ok(event) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_request_parses_camel_case() {
        let value = serde_json::json!({
            "skill": "dut_info",
            "dutId": "dut-a",
            "args": {"limit": 5}
        });
        let request: SkillRequest = serde_json::from_value(value).unwrap();
        assert_eq!(request.skill, "dut_info");
        assert_eq!(request.dut_id.as_deref(), Some("dut-a"));
        assert_eq!(request.args["limit"], serde_json::json!(5));
    }

    #[test]
    fn skill_request_without_optional_fields() {
        let value = serde_json::json!({ "skill": "dut_list" });
        let request: SkillRequest = serde_json::from_value(value).unwrap();
        assert_eq!(request.skill, "dut_list");
        assert!(request.dut_id.is_none());
    }

    #[test]
    fn dedup_state_caps_entries() {
        let mut state = DedupState::new();
        for index in 0..(DEDUP_CAP + 10) {
            let key = DedupState::key(&index.to_string(), "dut_info", &serde_json::json!({}));
            state.record(key, &serde_json::json!(index));
        }
        assert_eq!(state.results.len(), DEDUP_CAP);
    }

    #[test]
    fn dedup_key_distinguishes_skill_and_args() {
        let k1 = DedupState::key("m1", "dut_info", &serde_json::json!({"a": 1}));
        let k2 = DedupState::key("m1", "dut_info", &serde_json::json!({"a": 2}));
        let k3 = DedupState::key("m1", "serial_read", &serde_json::json!({"a": 1}));
        assert_ne!(k1, k2);
        assert_ne!(k1, k3);
    }
}
