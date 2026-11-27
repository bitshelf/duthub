//! A2A task surface: task→DUT ownership registry and the 3-method strict gate.
//!
//! W3 speaks exactly three JSON-RPC methods (`SendMessage`, `GetTask`,
//! `CancelTask`); everything else the SDK knows is answered with
//! `-32601 METHOD_NOT_FOUND` by [`StrictRequestHandler`], matching the card's
//! `capabilities` (streaming/push/extended all `false`, ADR-8).

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use a2a::methods;
use a2a::{
    A2AError, AgentCard, CancelTaskRequest, GetExtendedAgentCardRequest,
    GetTaskPushNotificationConfigRequest, GetTaskRequest, ListTaskPushNotificationConfigsRequest,
    ListTaskPushNotificationConfigsResponse, ListTasksRequest, ListTasksResponse,
    SendMessageRequest, SendMessageResponse, SubscribeToTaskRequest, Task,
    TaskPushNotificationConfig,
};
use a2a_server::handler::{DefaultRequestHandler, RequestHandler};
use a2a_server::middleware::ServiceParams;
use async_trait::async_trait;
use futures::stream::BoxStream;

/// Maximum number of task ownership entries retained in memory (FIFO eviction).
const TASK_OWNERSHIP_CAP: usize = 4096;

#[derive(Debug, Default)]
struct OwnershipInner {
    tasks: HashMap<String, String>,
    order: VecDeque<String>,
}

/// Registry of task IDs this listener has minted, keyed to the DUT they
/// belong to. The authorizer consults it per request; unknown or foreign
/// task IDs are `task_not_found` (ADR-9 ownership gate).
#[derive(Debug, Default)]
pub struct TaskOwnership {
    inner: Mutex<OwnershipInner>,
}

impl TaskOwnership {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a freshly minted task for `dut_id`, evicting the oldest entry
    /// once [`TASK_OWNERSHIP_CAP`] is reached.
    pub fn register(&self, task_id: &str, dut_id: &str) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner
            .tasks
            .insert(task_id.to_owned(), dut_id.to_owned())
            .is_none()
        {
            inner.order.push_back(task_id.to_owned());
        }
        while inner.tasks.len() > TASK_OWNERSHIP_CAP {
            if let Some(oldest) = inner.order.pop_front() {
                inner.tasks.remove(&oldest);
            } else {
                break;
            }
        }
    }

    pub fn is_owned_by(&self, task_id: &str, dut_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tasks
            .get(task_id)
            .is_some_and(|owner| owner == dut_id)
    }

    /// Drop bookkeeping for a finished task (keeps the map bounded).
    pub fn forget(&self, task_id: &str) {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tasks
            .remove(task_id);
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .tasks
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `RequestHandler` that allows exactly the three supported methods and
/// answers every other A2A method with `method_not_found` (-32601).
pub struct StrictRequestHandler {
    inner: Arc<DefaultRequestHandler>,
}

impl std::fmt::Debug for StrictRequestHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StrictRequestHandler")
            .finish_non_exhaustive()
    }
}

impl StrictRequestHandler {
    #[must_use]
    pub fn new(inner: Arc<DefaultRequestHandler>) -> Self {
        Self { inner }
    }
}

/// The methods W3 serves; anything outside this set is `-32601`.
pub const ALLOWED_METHODS: [&str; 3] = [
    methods::SEND_MESSAGE,
    methods::GET_TASK,
    methods::CANCEL_TASK,
];

fn method_not_found(method: &str) -> A2AError {
    A2AError::method_not_found(method)
}

#[async_trait]
impl RequestHandler for StrictRequestHandler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        self.inner.send_message(params, req).await
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        self.inner.get_task(params, req).await
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        self.inner.cancel_task(params, req).await
    }

    async fn send_streaming_message(
        &self,
        _params: &ServiceParams,
        _req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<a2a::StreamResponse, A2AError>>, A2AError> {
        Err(method_not_found(methods::SEND_STREAMING_MESSAGE))
    }

    async fn list_tasks(
        &self,
        _params: &ServiceParams,
        _req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        Err(method_not_found(methods::LIST_TASKS))
    }

    async fn subscribe_to_task(
        &self,
        _params: &ServiceParams,
        _req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<a2a::StreamResponse, A2AError>>, A2AError> {
        Err(method_not_found(methods::SUBSCRIBE_TO_TASK))
    }

    async fn create_push_config(
        &self,
        _params: &ServiceParams,
        _req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Err(method_not_found(methods::CREATE_PUSH_CONFIG))
    }

    async fn get_push_config(
        &self,
        _params: &ServiceParams,
        _req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Err(method_not_found(methods::GET_PUSH_CONFIG))
    }

    async fn list_push_configs(
        &self,
        _params: &ServiceParams,
        _req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        Err(method_not_found(methods::LIST_PUSH_CONFIGS))
    }

    async fn delete_push_config(
        &self,
        _params: &ServiceParams,
        _req: a2a::DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        Err(method_not_found(methods::DELETE_PUSH_CONFIG))
    }

    async fn get_extended_agent_card(
        &self,
        _params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        Err(method_not_found(methods::GET_EXTENDED_AGENT_CARD))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowed_methods_are_exactly_three() {
        assert_eq!(ALLOWED_METHODS.len(), 3);
        assert!(ALLOWED_METHODS.contains(&"SendMessage"));
        assert!(ALLOWED_METHODS.contains(&"GetTask"));
        assert!(ALLOWED_METHODS.contains(&"CancelTask"));
    }

    #[test]
    fn method_not_found_uses_minus_32601() {
        let error = method_not_found(methods::LIST_TASKS);
        assert_eq!(error.code, a2a::error_code::METHOD_NOT_FOUND);
        assert_eq!(error.code, -32601);
    }

    #[test]
    fn ownership_register_check_forget() {
        let ownership = TaskOwnership::new();
        ownership.register("t1", "dut-a");
        assert!(ownership.is_owned_by("t1", "dut-a"));
        assert!(!ownership.is_owned_by("t1", "dut-b"));
        assert!(!ownership.is_owned_by("t2", "dut-a"));
        ownership.forget("t1");
        assert!(!ownership.is_owned_by("t1", "dut-a"));
        assert!(ownership.is_empty());
    }
}
