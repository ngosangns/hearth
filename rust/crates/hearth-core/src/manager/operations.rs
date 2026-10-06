//! `OperationScheduler` — schedules and serializes operations per target service (`serviceId`,
//! or every id in `target_service_ids` for a bulk start), publishing
//! `operation.accepted`/`operation.updated` events as they progress. Manager-wide work such as
//! shutdown still serializes on `"__manager__"`; ordinary multi-service starts do not.
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use uuid::Uuid;

use crate::catalog::ServiceId;
use crate::state::{
    Operation, OperationError, OperationKind, OperationStatus, OperationTraceEntry,
    ServiceOperationKind,
};
use crate::supervisor::types::format_iso8601_millis;
use crate::sync::KeyedLock;

use super::event_store::ManagerEventStore;

const MANAGER_TARGET: &str = "__manager__";

/// How long a settled operation stays readable (`GET /v1/operations/:id`) and its `requestId`
/// stays idempotent. Operations used to be kept for the daemon's whole life — one entry in each of
/// three maps per start/stop/restart, forever.
const SETTLED_OPERATION_TTL: Duration = Duration::from_secs(10 * 60);
/// Upper bound on settled operations kept, whatever their age — a burst (a client retry loop, a
/// scripted bulk restart) is evicted oldest-first rather than waiting out the TTL.
const MAX_SETTLED_OPERATIONS: usize = 1024;

fn now() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
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
pub type OperationExecute =
    Box<dyn FnOnce(OperationHandle) -> BoxFuture<'static, Result<(), OperationError>> + Send>;
pub type OperationRejected = Box<dyn FnOnce(OperationHandle) -> BoxFuture<'static, ()> + Send>;

pub struct OperationScheduler {
    events: Arc<ManagerEventStore>,
    closing: AtomicBool,
    operations: Mutex<HashMap<String, OperationHandle>>,
    request_ids: Mutex<HashMap<String, OperationHandle>>,
    /// `target -> id of the operation currently occupying that target's queue slot`. A bulk op
    /// registers every service it touches. Membership (not just presence in `operations`, which
    /// keeps every operation ever scheduled) is what `is_queued`/`drain_services` need — the
    /// *current* occupant, not some earlier completed operation that happened to share a target.
    active_targets: Mutex<HashMap<String, String>>,
    queues: KeyedLock<String>,
    /// One `watch` cell per scheduled operation, flipped to `true` once its spawned task finishes.
    /// `wait()`/`drain_services()` key off this instead of racing to (re-)acquire the target's
    /// `KeyedLock` themselves: `tokio::spawn` only *schedules* the operation's task, it doesn't
    /// guarantee it has started (let alone acquired the lock) by the time a caller calls `wait()`
    /// immediately after `schedule()` — a lock-acquisition race that let `wait()` return before the
    /// operation had even begun running. `watch::Receiver::changed()` has no such race: a receiver
    /// created before the sender's value flips always observes the flip, and one created after simply
    /// sees the already-`true` initial read.
    done: Mutex<HashMap<String, tokio::sync::watch::Receiver<bool>>>,
    /// Settled operation ids, oldest first, with when they settled — the eviction queue for
    /// `operations`/`request_ids`/`done`.
    settled: Mutex<VecDeque<(Instant, String)>>,
}

/// Lock keys for an operation, sorted and deduped so overlapping multi-target ops cannot
/// deadlock. A single-service op locks that service. A bulk op locks every target service.
/// Manager-wide work (shutdown) with neither still uses `__manager__`.
fn targets_for(
    service_id: &Option<ServiceId>,
    target_service_ids: Option<&[ServiceId]>,
) -> Vec<String> {
    if let Some(id) = service_id {
        return vec![id.clone()];
    }
    if let Some(ids) = target_service_ids {
        if !ids.is_empty() {
            let mut keys: Vec<String> = ids.to_vec();
            keys.sort();
            keys.dedup();
            return keys;
        }
    }
    vec![MANAGER_TARGET.to_string()]
}

fn targets_of(operation: &Operation) -> Vec<String> {
    targets_for(
        &operation.service_id,
        operation.target_service_ids.as_deref(),
    )
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
            settled: Mutex::new(VecDeque::new()),
        })
    }

    /// Records `id` as settled and evicts every settled operation past the TTL or the cap.
    fn settle(&self, id: &str) {
        let now = Instant::now();
        let evicted: Vec<String> = {
            let mut settled = self.settled.lock().unwrap();
            settled.push_back((now, id.to_string()));
            let mut evicted = Vec::new();
            while settled
                .front()
                .is_some_and(|(at, _)| now.duration_since(*at) > SETTLED_OPERATION_TTL)
                || settled.len() > MAX_SETTLED_OPERATIONS
            {
                if let Some((_, id)) = settled.pop_front() {
                    evicted.push(id);
                }
            }
            evicted
        };
        if evicted.is_empty() {
            return;
        }
        let mut operations = self.operations.lock().unwrap();
        let mut request_ids = self.request_ids.lock().unwrap();
        let mut done = self.done.lock().unwrap();
        for id in evicted {
            done.remove(&id);
            let Some(handle) = operations.remove(&id) else {
                continue;
            };
            let request_id = handle.lock().unwrap().request_id.clone();
            // Only if the request id still maps to this operation — never evict a newer one.
            if request_ids
                .get(&request_id)
                .is_some_and(|current| Arc::ptr_eq(current, &handle))
            {
                request_ids.remove(&request_id);
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<Operation> {
        self.operations
            .lock()
            .unwrap()
            .get(id)
            .map(|h| h.lock().unwrap().clone())
    }

    pub fn is_queued(&self, operation: &Operation) -> bool {
        let targets = targets_of(operation);
        let active = self.active_targets.lock().unwrap();
        targets
            .iter()
            .any(|target| active.get(target).map(String::as_str) == Some(operation.id.as_str()))
    }

    pub fn close_mutations(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    /// Waits for every currently in-flight *service-targeted* operation to finish (excludes the
    /// manager-wide target) — used by shutdown to let in-flight work settle before stopping services.
    pub async fn drain_services(&self) {
        let mut ids: Vec<String> = self
            .active_targets
            .lock()
            .unwrap()
            .iter()
            .filter(|(target, _)| *target != MANAGER_TARGET)
            .map(|(_, id)| id.clone())
            .collect();
        // A bulk op registers every target service; wait once per operation.
        ids.sort();
        ids.dedup();
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

    pub fn resolve_request(
        &self,
        input: &OperationInput,
    ) -> Result<Option<Operation>, RequestIdConflict> {
        let existing = self
            .request_ids
            .lock()
            .unwrap()
            .get(&input.request_id)
            .cloned();
        let Some(existing) = existing else {
            return Ok(None);
        };
        let snapshot = existing.lock().unwrap().clone();
        if snapshot.kind != input.kind
            || snapshot.service_id != input.service_id
            || snapshot.action != input.action
            || !same_service_ids(&snapshot.target_service_ids, &input.target_service_ids)
        {
            return Err(RequestIdConflict);
        }
        Ok(Some(snapshot))
    }

    pub fn schedule(
        self: &Arc<Self>,
        input: OperationInput,
        execute: OperationExecute,
        rejected: Option<OperationRejected>,
    ) -> Result<Operation, RequestIdConflict> {
        let (operation, release) = self.schedule_inner(input, execute, rejected, false)?;
        if let Some(release) = release {
            let _ = release.send(());
        }
        Ok(operation)
    }

    /// Like `schedule`, but when this call *creates* the operation the worker does not observe
    /// state until `release` is signaled. Callers that publish `desired: running` before the start
    /// worker runs hold that sender across the publish. Dropping it abandons the operation: the
    /// rejected callback runs and the operation fails, so a row queued for it is not left behind.
    /// An idempotent replay of an existing request returns `None` — that worker is already released.
    pub fn schedule_when_released(
        self: &Arc<Self>,
        input: OperationInput,
        execute: OperationExecute,
        rejected: Option<OperationRejected>,
    ) -> Result<(Operation, Option<tokio::sync::oneshot::Sender<()>>), RequestIdConflict> {
        self.schedule_inner(input, execute, rejected, true)
    }

    fn schedule_inner(
        self: &Arc<Self>,
        input: OperationInput,
        execute: OperationExecute,
        rejected: Option<OperationRejected>,
        defer: bool,
    ) -> Result<(Operation, Option<tokio::sync::oneshot::Sender<()>>), RequestIdConflict> {
        if let Some(existing) = self.resolve_request(&input)? {
            return Ok((existing, None));
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
            trace: vec![OperationTraceEntry {
                at: created_at,
                message: "Operation accepted".to_string(),
            }],
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
        event_data.insert(
            "operationId".to_string(),
            serde_json::Value::String(operation.id.clone()),
        );
        event_data.insert(
            "requestId".to_string(),
            serde_json::Value::String(operation.request_id.clone()),
        );
        event_data.insert(
            "serviceId".to_string(),
            operation
                .service_id
                .clone()
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        self.events.publish("operation.accepted", event_data);

        let targets = targets_for(
            &operation.service_id,
            operation.target_service_ids.as_deref(),
        );
        {
            let mut active = self.active_targets.lock().unwrap();
            for target in &targets {
                active.insert(target.clone(), operation.id.clone());
            }
        }
        let (done_tx, done_rx) = tokio::sync::watch::channel(false);
        self.done
            .lock()
            .unwrap()
            .insert(operation.id.clone(), done_rx);
        let (release_tx, release_rx) = if defer {
            let (tx, rx) = tokio::sync::oneshot::channel();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };
        let scheduler = self.clone();
        let kind = operation.kind;
        let targets_for_task = targets.clone();
        let operation_id_for_task = operation.id.clone();
        let handle_for_task = handle.clone();
        tokio::spawn(async move {
            if let Some(release_rx) = release_rx {
                if release_rx.await.is_err() {
                    if let Some(rejected) = rejected {
                        rejected(handle_for_task.clone()).await;
                    }
                    scheduler.transition(
                        &handle_for_task,
                        OperationStatus::Failed,
                        "Operation abandoned before it started",
                    );
                    {
                        let mut active = scheduler.active_targets.lock().unwrap();
                        for target in &targets_for_task {
                            if active.get(target).map(String::as_str)
                                == Some(operation_id_for_task.as_str())
                            {
                                active.remove(target);
                            }
                        }
                    }
                    let _ = done_tx.send(true);
                    scheduler.settle(&operation_id_for_task);
                    return;
                }
            }
            // Acquire every target lock in sorted order (see `targets_for`) so overlapping
            // multi-service ops cannot deadlock. Disjoint sets run in parallel.
            let mut guards = Vec::with_capacity(targets_for_task.len());
            for target in &targets_for_task {
                guards.push(scheduler.queues.lock(target).await);
            }
            let is_closing = scheduler.closing.load(Ordering::SeqCst);
            if is_closing && matches!(kind, OperationKind::Service | OperationKind::BulkStart) {
                if let Some(rejected) = rejected {
                    rejected(handle_for_task.clone()).await;
                }
                handle_for_task.lock().unwrap().error = Some(OperationError {
                    code: "manager_closing".to_string(),
                    message: "Manager is shutting down".to_string(),
                });
                scheduler.transition(
                    &handle_for_task,
                    OperationStatus::Failed,
                    "Operation rejected because manager is shutting down",
                );
            } else {
                scheduler.transition(
                    &handle_for_task,
                    OperationStatus::Running,
                    "Operation started",
                );
                match execute(handle_for_task.clone()).await {
                    Ok(()) => scheduler.transition(
                        &handle_for_task,
                        OperationStatus::Succeeded,
                        "Operation completed",
                    ),
                    Err(error) => {
                        let message = format!("Operation failed: {}", error.message);
                        handle_for_task.lock().unwrap().error = Some(error);
                        scheduler.transition(&handle_for_task, OperationStatus::Failed, &message);
                    }
                }
            }
            // Only clear slots that are still OURS. A later operation on a shared target
            // overwrites that entry, and removing it unconditionally made `drain_services()` see an
            // empty map and return while that newer operation was still running — so
            // `HearthManager::shutdown` raced a start in flight. Mirrors the TS source's
            // `if (this.queues.get(target) === current)` guard.
            {
                let mut active = scheduler.active_targets.lock().unwrap();
                for target in &targets_for_task {
                    if active.get(target).map(String::as_str)
                        == Some(operation_id_for_task.as_str())
                    {
                        active.remove(target);
                    }
                }
            }
            drop(guards);
            let _ = done_tx.send(true);
            scheduler.settle(&operation_id_for_task);
        });
        Ok((operation, release_tx))
    }

    pub fn trace(&self, handle: &OperationHandle, message: &str) {
        let (id, service_id, status) = {
            let mut op = handle.lock().unwrap();
            op.updated_at = now();
            let at = op.updated_at.clone();
            op.trace.push(OperationTraceEntry {
                at,
                message: message.to_string(),
            });
            (op.id.clone(), op.service_id.clone(), op.status)
        };
        // The operation's REAL status, not a hardcoded `Running` — a trace entry appended while an
        // operation is queued or already settled would otherwise tell every subscriber it is
        // running.
        self.publish_updated(&id, status, &service_id);
    }

    fn transition(&self, handle: &OperationHandle, status: OperationStatus, message: &str) {
        let (id, service_id) = {
            let mut op = handle.lock().unwrap();
            op.status = status;
            op.updated_at = now();
            let at = op.updated_at.clone();
            op.trace.push(OperationTraceEntry {
                at,
                message: message.to_string(),
            });
            (op.id.clone(), op.service_id.clone())
        };
        self.publish_updated(&id, status, &service_id);
    }

    fn publish_updated(&self, id: &str, status: OperationStatus, service_id: &Option<ServiceId>) {
        let mut event_data = serde_json::Map::new();
        event_data.insert(
            "operationId".to_string(),
            serde_json::Value::String(id.to_string()),
        );
        event_data.insert("status".to_string(), serde_json::to_value(status).unwrap());
        event_data.insert(
            "serviceId".to_string(),
            service_id
                .clone()
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        self.events.publish("operation.updated", event_data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn service_input(
        request_id: &str,
        service_id: &str,
        action: ServiceOperationKind,
    ) -> OperationInput {
        OperationInput {
            request_id: request_id.to_string(),
            kind: OperationKind::Service,
            service_id: Some(service_id.to_string()),
            target_service_ids: None,
            action: Some(action),
        }
    }

    fn bulk_input(request_id: &str, targets: &[&str]) -> OperationInput {
        OperationInput {
            request_id: request_id.to_string(),
            kind: OperationKind::BulkStart,
            service_id: None,
            target_service_ids: Some(targets.iter().map(|s| (*s).to_string()).collect()),
            action: None,
        }
    }

    fn manager_input(request_id: &str) -> OperationInput {
        OperationInput {
            request_id: request_id.to_string(),
            kind: OperationKind::ManagerShutdown,
            service_id: None,
            target_service_ids: None,
            action: None,
        }
    }

    #[tokio::test]
    async fn schedule_runs_execute_and_transitions_to_succeeded() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let operation = scheduler
            .schedule(
                service_input("req-1", "api", ServiceOperationKind::Start),
                Box::new(|_handle| Box::pin(async { Ok(()) })),
                None,
            )
            .unwrap();
        assert_eq!(operation.status, OperationStatus::Queued);
        scheduler.wait(&operation).await;
        let final_state = scheduler.get(&operation.id).unwrap();
        assert_eq!(final_state.status, OperationStatus::Succeeded);
    }

    #[tokio::test]
    async fn a_deferred_operation_waits_for_release_and_dropping_it_rejects() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let started = Arc::new(AtomicBool::new(false));
        let started_flag = started.clone();
        let rejected = Arc::new(AtomicBool::new(false));
        let rejected_flag = rejected.clone();
        let (operation, release) = scheduler
            .schedule_when_released(
                service_input("req-held", "api", ServiceOperationKind::Start),
                Box::new(move |_handle| {
                    let started_flag = started_flag.clone();
                    Box::pin(async move {
                        started_flag.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                }),
                Some(Box::new(move |_handle| {
                    let rejected_flag = rejected_flag.clone();
                    Box::pin(async move {
                        rejected_flag.store(true, Ordering::SeqCst);
                    })
                })),
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            !started.load(Ordering::SeqCst),
            "the worker must not run before release"
        );
        assert_eq!(
            scheduler.get(&operation.id).unwrap().status,
            OperationStatus::Queued
        );
        drop(release);
        scheduler.wait(&operation).await;
        assert!(!started.load(Ordering::SeqCst));
        assert!(rejected.load(Ordering::SeqCst));
        assert_eq!(
            scheduler.get(&operation.id).unwrap().status,
            OperationStatus::Failed
        );

        let started = Arc::new(AtomicBool::new(false));
        let started_flag = started.clone();
        let (operation, release) = scheduler
            .schedule_when_released(
                service_input("req-go", "api", ServiceOperationKind::Start),
                Box::new(move |_handle| {
                    let started_flag = started_flag.clone();
                    Box::pin(async move {
                        started_flag.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        release.expect("a new operation is held").send(()).unwrap();
        scheduler.wait(&operation).await;
        assert!(started.load(Ordering::SeqCst));
        assert_eq!(
            scheduler.get(&operation.id).unwrap().status,
            OperationStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn schedule_transitions_to_failed_on_execute_error() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let operation = scheduler
            .schedule(
                service_input("req-1", "api", ServiceOperationKind::Start),
                Box::new(|_handle| {
                    Box::pin(async {
                        Err(OperationError {
                            code: "boom".to_string(),
                            message: "it broke".to_string(),
                        })
                    })
                }),
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
        let first = scheduler
            .schedule(
                service_input("req-1", "api", ServiceOperationKind::Start),
                Box::new(|_h| Box::pin(async { Ok(()) })),
                None,
            )
            .unwrap();
        let second = scheduler
            .schedule(
                service_input("req-1", "api", ServiceOperationKind::Start),
                Box::new(|_h| Box::pin(async { Ok(()) })),
                None,
            )
            .unwrap();
        assert_eq!(first.id, second.id);
    }

    #[tokio::test]
    async fn same_request_id_with_different_action_conflicts() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        scheduler
            .schedule(
                service_input("req-1", "api", ServiceOperationKind::Start),
                Box::new(|_h| Box::pin(async { Ok(()) })),
                None,
            )
            .unwrap();
        let err =
            scheduler.resolve_request(&service_input("req-1", "api", ServiceOperationKind::Stop));
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
        tokio::time::timeout(Duration::from_millis(500), started_a.notified())
            .await
            .expect("op a should start promptly");
        tokio::time::timeout(Duration::from_millis(500), started_b.notified())
            .await
            .expect("op b should start promptly");
        release.notify_waiters();
        scheduler.wait(&op_a).await;
        scheduler.wait(&op_b).await;
        assert_eq!(
            scheduler.get(&op_a.id).unwrap().status,
            OperationStatus::Succeeded
        );
        assert_eq!(
            scheduler.get(&op_b.id).unwrap().status,
            OperationStatus::Succeeded
        );
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
            .schedule(
                service_input("req-1", "api", ServiceOperationKind::Start),
                Box::new(move |_h| {
                    Box::pin(async move {
                        gate2.notified().await;
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        assert!(scheduler.is_queued(&operation));
        gate.notify_one();
        scheduler.wait(&operation).await;
        assert!(!scheduler.is_queued(&operation));
    }

    #[tokio::test]
    async fn settled_operations_beyond_the_cap_are_evicted_oldest_first() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let mut ids = Vec::new();
        for n in 0..(MAX_SETTLED_OPERATIONS + 2) {
            let operation = scheduler
                .schedule(
                    service_input(&format!("req-{n}"), "api", ServiceOperationKind::Status),
                    Box::new(|_h| Box::pin(async { Ok(()) })),
                    None,
                )
                .unwrap();
            scheduler.wait(&operation).await;
            ids.push(operation.id);
        }
        // `wait` returns on the done signal, just before `settle` runs; wait for the last settle.
        let deadline = Instant::now() + Duration::from_secs(5);
        while scheduler
            .settled
            .lock()
            .unwrap()
            .back()
            .map(|(_, id)| id.as_str())
            != Some(ids.last().unwrap().as_str())
        {
            assert!(
                Instant::now() < deadline,
                "the last operation never settled"
            );
            tokio::task::yield_now().await;
        }
        assert!(
            scheduler.get(&ids[0]).is_none(),
            "the oldest settled operation is evicted"
        );
        assert!(
            scheduler.get(ids.last().unwrap()).is_some(),
            "the newest is kept"
        );
        assert!(scheduler.operations.lock().unwrap().len() <= MAX_SETTLED_OPERATIONS);
        assert!(scheduler.request_ids.lock().unwrap().len() <= MAX_SETTLED_OPERATIONS);
        assert!(
            scheduler
                .resolve_request(&service_input("req-0", "api", ServiceOperationKind::Status))
                .unwrap()
                .is_none(),
            "an evicted request id is free again"
        );
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

    #[tokio::test]
    async fn disjoint_bulk_starts_run_concurrently() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let started_a = Arc::new(tokio::sync::Notify::new());
        let started_b = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let started_a2 = started_a.clone();
        let release_a = release.clone();
        let op_a = scheduler
            .schedule(
                bulk_input("bulk-a", &["a1", "a2"]),
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
                bulk_input("bulk-b", &["b1", "b2"]),
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
        tokio::time::timeout(Duration::from_millis(500), started_a.notified())
            .await
            .expect("bulk a should start promptly");
        tokio::time::timeout(Duration::from_millis(500), started_b.notified())
            .await
            .expect("bulk b should start promptly on a disjoint set");
        release.notify_waiters();
        scheduler.wait(&op_a).await;
        scheduler.wait(&op_b).await;
        assert_eq!(
            scheduler.get(&op_a.id).unwrap().status,
            OperationStatus::Succeeded
        );
        assert_eq!(
            scheduler.get(&op_b.id).unwrap().status,
            OperationStatus::Succeeded
        );
    }

    #[tokio::test]
    async fn overlapping_bulk_starts_serialize_on_shared_services() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let order = Arc::new(Mutex::new(Vec::new()));
        let first_hold = Arc::new(tokio::sync::Notify::new());
        let first_started = Arc::new(tokio::sync::Notify::new());
        let order1 = order.clone();
        let hold1 = first_hold.clone();
        let started1 = first_started.clone();
        let op1 = scheduler
            .schedule(
                bulk_input("bulk-1", &["shared", "left"]),
                Box::new(move |_h| {
                    Box::pin(async move {
                        order1.lock().unwrap().push(1);
                        started1.notify_one();
                        hold1.notified().await;
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        tokio::time::timeout(Duration::from_millis(500), first_started.notified())
            .await
            .expect("first overlapping bulk should start");
        let order2 = order.clone();
        let second_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let second_flag = second_started.clone();
        let op2 = scheduler
            .schedule(
                bulk_input("bulk-2", &["shared", "right"]),
                Box::new(move |_h| {
                    Box::pin(async move {
                        second_flag.store(true, Ordering::SeqCst);
                        order2.lock().unwrap().push(2);
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            !second_started.load(Ordering::SeqCst),
            "second bulk must wait on the shared service lock"
        );
        first_hold.notify_one();
        scheduler.wait(&op1).await;
        scheduler.wait(&op2).await;
        assert!(second_started.load(Ordering::SeqCst));
        assert_eq!(*order.lock().unwrap(), vec![1, 2]);
    }

    #[tokio::test]
    async fn overlapping_bulks_with_reversed_key_order_do_not_deadlock() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let release = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let started_a = started.clone();
        let release_a = release.clone();
        let op_a = scheduler
            .schedule(
                // Deliberately reverse lexicographic order in the input — locks must still
                // acquire sorted so the pair cannot deadlock against the other bulk.
                bulk_input("bulk-rev-a", &["z", "a"]),
                Box::new(move |_h| {
                    Box::pin(async move {
                        started_a.fetch_add(1, Ordering::SeqCst);
                        release_a.notified().await;
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        let started_b = started.clone();
        let op_b = scheduler
            .schedule(
                bulk_input("bulk-rev-b", &["a", "z"]),
                Box::new(move |_h| {
                    Box::pin(async move {
                        started_b.fetch_add(1, Ordering::SeqCst);
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        // One holds both keys (sorted a then z); the other waits on the first key. Releasing
        // the holder must let the waiter finish — no deadlock from input order.
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
        release.notify_one();
        tokio::time::timeout(Duration::from_millis(500), scheduler.wait(&op_a))
            .await
            .expect("bulk a must finish without deadlock");
        tokio::time::timeout(Duration::from_millis(500), scheduler.wait(&op_b))
            .await
            .expect("bulk b must finish without deadlock");
        assert_eq!(started.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn bulk_and_single_service_on_the_same_id_serialize() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let order = Arc::new(Mutex::new(Vec::new()));
        let hold = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let order1 = order.clone();
        let hold1 = hold.clone();
        let started1 = started.clone();
        let bulk = scheduler
            .schedule(
                bulk_input("bulk-same", &["api", "web"]),
                Box::new(move |_h| {
                    Box::pin(async move {
                        order1.lock().unwrap().push("bulk");
                        started1.notify_one();
                        hold1.notified().await;
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        tokio::time::timeout(Duration::from_millis(500), started.notified())
            .await
            .expect("bulk should start");
        let order2 = order.clone();
        let single_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let single_flag = single_started.clone();
        let single = scheduler
            .schedule(
                service_input("single-api", "api", ServiceOperationKind::Start),
                Box::new(move |_h| {
                    Box::pin(async move {
                        single_flag.store(true, Ordering::SeqCst);
                        order2.lock().unwrap().push("single");
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            !single_started.load(Ordering::SeqCst),
            "single-service start must wait while a bulk holds that service"
        );
        hold.notify_one();
        scheduler.wait(&bulk).await;
        scheduler.wait(&single).await;
        assert_eq!(
            *order.lock().unwrap(),
            vec!["bulk".to_string(), "single".to_string()]
        );
    }

    #[tokio::test]
    async fn manager_shutdown_still_uses_the_manager_target() {
        let scheduler = OperationScheduler::new(ManagerEventStore::new(None, None));
        let hold = Arc::new(tokio::sync::Notify::new());
        let started = Arc::new(tokio::sync::Notify::new());
        let hold1 = hold.clone();
        let started1 = started.clone();
        let first = scheduler
            .schedule(
                manager_input("shutdown-1"),
                Box::new(move |_h| {
                    Box::pin(async move {
                        started1.notify_one();
                        hold1.notified().await;
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        tokio::time::timeout(Duration::from_millis(500), started.notified())
            .await
            .expect("first manager op should start");
        let second_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = second_started.clone();
        let second = scheduler
            .schedule(
                manager_input("shutdown-2"),
                Box::new(move |_h| {
                    Box::pin(async move {
                        flag.store(true, Ordering::SeqCst);
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert!(
            !second_started.load(Ordering::SeqCst),
            "manager-wide ops must still serialize on __manager__"
        );
        // A bulk on ordinary services must not wait on the manager lock.
        let bulk_started = Arc::new(tokio::sync::Notify::new());
        let bulk_flag = bulk_started.clone();
        let release_bulk = Arc::new(tokio::sync::Notify::new());
        let release_bulk2 = release_bulk.clone();
        let bulk = scheduler
            .schedule(
                bulk_input("bulk-during-shutdown", &["x", "y"]),
                Box::new(move |_h| {
                    Box::pin(async move {
                        bulk_flag.notify_one();
                        release_bulk2.notified().await;
                        Ok(())
                    })
                }),
                None,
            )
            .unwrap();
        tokio::time::timeout(Duration::from_millis(500), bulk_started.notified())
            .await
            .expect("bulk start must not queue behind __manager__");
        hold.notify_one();
        release_bulk.notify_one();
        scheduler.wait(&first).await;
        scheduler.wait(&second).await;
        scheduler.wait(&bulk).await;
        assert!(second_started.load(Ordering::SeqCst));
    }
}
