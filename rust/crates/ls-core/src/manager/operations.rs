//! Port of `OperationScheduler` (`src/core/manager.ts`) — schedules and serializes operations
//! per target (`serviceId`, or `"__manager__"` for manager-wide operations like shutdown),
//! publishing `operation.accepted`/`operation.updated` events as they progress.
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use futures::future::BoxFuture;
use uuid::Uuid;

use crate::catalog::ServiceId;
use crate::state::{Operation, OperationError, OperationKind, OperationStatus, OperationTraceEntry, ServiceOperationKind};
use crate::supervisor::types::format_iso8601_millis;

use super::event_store::ManagerEventStore;

const MANAGER_TARGET: &str = "__manager__";

fn now() -> String {
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64;
    format_iso8601_millis(millis)
}

#[derive(Debug, Clone)]
pub struct OperationInput {
    pub request_id: String,
    pub kind: OperationKind,
    pub service_id: Option<ServiceId>,
    pub target_service_ids: Option<Vec<ServiceId>>,
    pub action: Option<ServiceOperationKind>,
}

#[derive(Debug, thiserror::Error)]
#[error("requestId is already used by a different operation")]
pub struct RequestIdConflict;

pub type OperationHandle = Arc<Mutex<Operation>>;
pub type OperationExecute = Box<dyn FnOnce(OperationHandle) -> BoxFuture<'static, Result<(), OperationError>> + Send>;
pub type OperationRejected = Box<dyn FnOnce(OperationHandle) -> BoxFuture<'static, ()> + Send>;

struct KeyedLock {
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}
impl KeyedLock {
    fn new() -> Self {
        Self { locks: Mutex::new(HashMap::new()) }
    }
    fn get(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.locks.lock().unwrap().entry(key.to_string()).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
    }
}

pub struct OperationScheduler {
    events: Arc<ManagerEventStore>,
    closing: AtomicBool,
    operations: Mutex<HashMap<String, OperationHandle>>,
    request_ids: Mutex<HashMap<String, OperationHandle>>,
    /// `target -> id of the operation currently occupying that target's queue slot`. Membership
    /// (not just presence in `operations`, which keeps every operation ever scheduled) is what
    /// `is_queued`/`drain_services` need — the *current* occupant, not some earlier completed
    /// operation that happened to share the same target.
    active_targets: Mutex<HashMap<String, String>>,
    queues: KeyedLock,
    /// One `watch` cell per scheduled operation, flipped to `true` once its spawned task finishes.
    /// `wait()`/`drain_services()` key off this instead of racing to (re-)acquire the target's
    /// `KeyedLock` themselves: `tokio::spawn` only *schedules* the operation's task, it doesn't
    /// guarantee it has started (let alone acquired the lock) by the time a caller calls `wait()`
    /// immediately after `schedule()` — a lock-acquisition race that let `wait()` return before the
    /// operation had even begun running. `watch::Receiver::changed()` has no such race: a receiver
    /// created before the sender's value flips always observes the flip, and one created after simply
    /// sees the already-`true` initial read.
    done: Mutex<HashMap<String, tokio::sync::watch::Receiver<bool>>>,
}

fn target_of(service_id: &Option<ServiceId>) -> String {
    service_id.clone().unwrap_or_else(|| MANAGER_TARGET.to_string())
}

fn same_service_ids(left: &Option<Vec<ServiceId>>, right: &Option<Vec<ServiceId>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(l), Some(r)) => l == r,
        _ => false,
    }
}

impl OperationScheduler {
    pub fn new(events: Arc<ManagerEventStore>) -> Arc<Self> {
        Arc::new(Self {
            events,
            closing: AtomicBool::new(false),
            operations: Mutex::new(HashMap::new()),
            request_ids: Mutex::new(HashMap::new()),
            active_targets: Mutex::new(HashMap::new()),
            queues: KeyedLock::new(),
            done: Mutex::new(HashMap::new()),
        })
    }

    pub fn get(&self, id: &str) -> Option<Operation> {
        self.operations.lock().unwrap().get(id).map(|h| h.lock().unwrap().clone())
    }

    pub fn is_queued(&self, operation: &Operation) -> bool {
        self.active_targets.lock().unwrap().contains_key(&target_of(&operation.service_id))
    }

    pub fn close_mutations(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    /// Waits for every currently in-flight *service-targeted* operation to finish (excludes the
    /// manager-wide target) — used by shutdown to let in-flight work settle before stopping services.
    pub async fn drain_services(&self) {
        let ids: Vec<String> = self.active_targets.lock().unwrap().iter().filter(|(target, _)| *target != MANAGER_TARGET).map(|(_, id)| id.clone()).collect();
        let waits = ids.iter().map(|id| self.wait_for_id(id));
        futures::future::join_all(waits).await;
    }

    pub async fn wait(&self, operation: &Operation) {
        self.wait_for_id(&operation.id).await;
    }

    async fn wait_for_id(&self, id: &str) {
        let receiver = self.done.lock().unwrap().get(id).cloned();
        if let Some(mut receiver) = receiver {
            while !*receiver.borrow() {
                if receiver.changed().await.is_err() {
                    break;
                }
            }
        }
    }

    pub fn resolve_request(&self, input: &OperationInput) -> Result<Option<Operation>, RequestIdConflict> {
        let existing = self.request_ids.lock().unwrap().get(&input.request_id).cloned();
        let Some(existing) = existing else { return Ok(None) };
        let snapshot = existing.lock().unwrap().clone();
        if snapshot.kind != input.kind || snapshot.service_id != input.service_id || snapshot.action != input.action || !same_service_ids(&snapshot.target_service_ids, &input.target_service_ids) {
            return Err(RequestIdConflict);
        }
        Ok(Some(snapshot))
    }

    pub fn schedule(self: &Arc<Self>, input: OperationInput, execute: OperationExecute, rejected: Option<OperationRejected>) -> Result<Operation, RequestIdConflict> {
        if let Some(existing) = self.resolve_request(&input)? {
            return Ok(existing);
        }
        let created_at = now();
        let operation = Operation {
            id: Uuid::new_v4().to_string(),
            request_id: input.request_id.clone(),
            kind: input.kind,
            service_id: input.service_id.clone(),
            target_service_ids: input.target_service_ids.clone(),
            action: input.action,
            status: OperationStatus::Queued,
            created_at: created_at.clone(),
            updated_at: created_at.clone(),
            trace: vec![OperationTraceEntry { at: created_at, message: "Operation accepted".to_string() }],
            error: None,
        };
        let handle: OperationHandle = Arc::new(Mutex::new(operation.clone()));
        {
            let mut operations = self.operations.lock().unwrap();
            operations.insert(operation.id.clone(), handle.clone());
        }
        {
            let mut request_ids = self.request_ids.lock().unwrap();
            request_ids.insert(input.request_id.clone(), handle.clone());
        }
        let mut event_data = serde_json::Map::new();
        event_data.insert("operationId".to_string(), serde_json::Value::String(operation.id.clone()));
        event_data.insert("requestId".to_string(), serde_json::Value::String(operation.request_id.clone()));
        event_data.insert("serviceId".to_string(), operation.service_id.clone().map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
        self.events.publish("operation.accepted", event_data);

        let target = target_of(&operation.service_id);
        self.active_targets.lock().unwrap().insert(target.clone(), operation.id.clone());
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        self.done.lock().unwrap().insert(operation.id.clone(), done_rx);
        let scheduler = self.clone();
        let kind = operation.kind;
        let target_for_task = target.clone();
        let handle_for_task = handle.clone();
        tokio::spawn(async move {
            let lock = scheduler.queues.get(&target_for_task);
            let _guard = lock.lock().await;
            let is_closing = scheduler.closing.load(Ordering::SeqCst);
            if is_closing && matches!(kind, OperationKind::Service | OperationKind::BulkStart) {
                if let Some(rejected) = rejected {
                    rejected(handle_for_task.clone()).await;
                }
                handle_for_task.lock().unwrap().error = Some(OperationError { code: "manager_closing".to_string(), message: "Manager is shutting down".to_string() });
                scheduler.transition(&handle_for_task, OperationStatus::Failed, "Operation rejected because manager is shutting down");
            } else {
                scheduler.transition(&handle_for_task, OperationStatus::Running, "Operation started");
                match execute(handle_for_task.clone()).await {
                    Ok(()) => scheduler.transition(&handle_for_task, OperationStatus::Succeeded, "Operation completed"),
                    Err(error) => {
                        let message = format!("Operation failed: {}", error.message);
                        handle_for_task.lock().unwrap().error = Some(error);
                        scheduler.transition(&handle_for_task, OperationStatus::Failed, &message);
                    }
                }
            }
            scheduler.active_targets.lock().unwrap().remove(&target_for_task);
            let _ = done_tx.send(true);
        });
        Ok(operation)
    }

    pub fn trace(&self, handle: &OperationHandle, message: &str) {
        let (id, service_id) = {
            let mut op = handle.lock().unwrap();
            op.updated_at = now();
            let at = op.updated_at.clone();
            op.trace.push(OperationTraceEntry { at, message: message.to_string() });
            (op.id.clone(), op.service_id.clone())
        };
        self.publish_updated(&id, OperationStatus::Running, &service_id);
    }

    fn transition(&self, handle: &OperationHandle, status: OperationStatus, message: &str) {
        let (id, service_id) = {
            let mut op = handle.lock().unwrap();
            op.status = status;
            op.updated_at = now();
            let at = op.updated_at.clone();
            op.trace.push(OperationTraceEntry { at, message: message.to_string() });
            (op.id.clone(), op.service_id.clone())
        };
        self.publish_updated(&id, status, &service_id);
    }

    fn publish_updated(&self, id: &str, status: OperationStatus, service_id: &Option<ServiceId>) {
        let mut event_data = serde_json::Map::new();
        event_data.insert("operationId".to_string(), serde_json::Value::String(id.to_string()));
        event_data.insert("status".to_string(), serde_json::to_value(status).unwrap());
        event_data.insert("serviceId".to_string(), service_id.clone().map(serde_json::Value::String).unwrap_or(serde_json::Value::Null));
        self.events.publish("operation.updated", event_data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn service_input(request_id: &str, service_id: &str, action: ServiceOperationKind) -> OperationInput {
        OperationInput { request_id: request_id.to_string(), kind: OperationKind::Service, service_id: Some(service_id.to_string()), target_service_ids: None, action: Some(action) }
    }

    #[tokio::test]
    async fn schedule_runs_execute_and_transitions_to_succeeded() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let operation = scheduler
            .schedule(service_input("req-1", "api", ServiceOperationKind::Start), Box::new(|_handle| Box::pin(async { Ok(()) })), None)
            .unwrap();
        assert_eq!(operation.status, OperationStatus::Queued);
        scheduler.wait(&operation).await;
        let final_state = scheduler.get(&operation.id).unwrap();
        assert_eq!(final_state.status, OperationStatus::Succeeded);
    }

    #[tokio::test]
    async fn schedule_transitions_to_failed_on_execute_error() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let operation = scheduler
            .schedule(
                service_input("req-1", "api", ServiceOperationKind::Start),
                Box::new(|_handle| Box::pin(async { Err(OperationError { code: "boom".to_string(), message: "it broke".to_string() }) })),
                None,
            )
            .unwrap();
        scheduler.wait(&operation).await;
        let final_state = scheduler.get(&operation.id).unwrap();
        assert_eq!(final_state.status, OperationStatus::Failed);
        assert_eq!(final_state.error.unwrap().code, "boom");
    }

    #[tokio::test]
    async fn duplicate_request_id_returns_the_existing_operation() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let first = scheduler.schedule(service_input("req-1", "api", ServiceOperationKind::Start), Box::new(|_h| Box::pin(async { Ok(()) })), None).unwrap();
        let second = scheduler.schedule(service_input("req-1", "api", ServiceOperationKind::Start), Box::new(|_h| Box::pin(async { Ok(()) })), None).unwrap();
        assert_eq!(first.id, second.id);
    }

    #[tokio::test]
    async fn same_request_id_with_different_action_conflicts() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        scheduler.schedule(service_input("req-1", "api", ServiceOperationKind::Start), Box::new(|_h| Box::pin(async { Ok(()) })), None).unwrap();
        let err = scheduler.resolve_request(&service_input("req-1", "api", ServiceOperationKind::Stop));
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn operations_for_different_services_run_concurrently() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let started_a = Arc::new(tokio::sync::Notify::new());
        let started_b = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let started_a2 = started_a.clone();
        let release_a = release.clone();
        let op_a = scheduler
            .schedule(
                service_input("req-a", "a", ServiceOperationKind::Start),
                Box::new(move |_h| {
                    Box::pin(async move {
                        started_a2.notify_one();
                        release_a.notified().await;
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        let started_b2 = started_b.clone();
        let release_b = release.clone();
        let op_b = scheduler
            .schedule(
                service_input("req-b", "b", ServiceOperationKind::Start),
                Box::new(move |_h| {
                    Box::pin(async move {
                        started_b2.notify_one();
                        release_b.notified().await;
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();

        // Both should start without waiting on each other (different targets).
        tokio::time::timeout(Duration::from_millis(500), started_a.notified()).await.expect("op a should start promptly");
        tokio::time::timeout(Duration::from_millis(500), started_b.notified()).await.expect("op b should start promptly");
        release.notify_waiters();
        scheduler.wait(&op_a).await;
        scheduler.wait(&op_b).await;
        assert_eq!(scheduler.get(&op_a.id).unwrap().status, OperationStatus::Succeeded);
        assert_eq!(scheduler.get(&op_b.id).unwrap().status, OperationStatus::Succeeded);
    }

    #[tokio::test]
    async fn operations_for_the_same_service_run_one_at_a_time() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let order = Arc::new(Mutex::new(Vec::new()));
        let order1 = order.clone();
        let op1 = scheduler
            .schedule(
                service_input("req-1", "api", ServiceOperationKind::Start),
                Box::new(move |_h| {
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        order1.lock().unwrap().push(1);
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        let order2 = order.clone();
        let op2 = scheduler
            .schedule(
                service_input("req-2", "api", ServiceOperationKind::Restart),
                Box::new(move |_h| {
                    Box::pin(async move {
                        order2.lock().unwrap().push(2);
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        scheduler.wait(&op1).await;
        scheduler.wait(&op2).await;
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn close_mutations_rejects_new_service_operations_once_dequeued() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        scheduler.close_mutations();
        let rejected_called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rejected_flag = rejected_called.clone();
        let operation = scheduler
            .schedule(
                service_input("req-1", "api", ServiceOperationKind::Start),
                Box::new(|_h| Box::pin(async { panic!("execute must not run once closing") })),
                Some(Box::new(move |_h| {
                    Box::pin(async move {
                        rejected_flag.store(true, Ordering::SeqCst);
                    })
                })),
            )
            .unwrap();
        scheduler.wait(&operation).await;
        assert!(rejected_called.load(Ordering::SeqCst));
        let final_state = scheduler.get(&operation.id).unwrap();
        assert_eq!(final_state.status, OperationStatus::Failed);
        assert_eq!(final_state.error.unwrap().code, "manager_closing");
    }

    #[tokio::test]
    async fn is_queued_true_while_running_false_after_completion() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let gate = Arc::new(tokio::sync::Notify::new());
        let gate2 = gate.clone();
        let operation = scheduler
            .schedule(service_input("req-1", "api", ServiceOperationKind::Start), Box::new(move |_h| Box::pin(async move { gate2.notified().await; Ok(()) })), None)
            .unwrap();
        assert!(scheduler.is_queued(&operation));
        gate.notify_one();
        scheduler.wait(&operation).await;
        assert!(!scheduler.is_queued(&operation));
    }

    #[tokio::test]
    async fn drain_services_waits_for_service_targeted_work_only() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done2 = done.clone();
        scheduler
            .schedule(
                service_input("req-1", "api", ServiceOperationKind::Start),
                Box::new(move |_h| {
                    Box::pin(async move {
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        done2.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        scheduler.drain_services().await;
        assert!(done.load(Ordering::SeqCst));
    }
}
