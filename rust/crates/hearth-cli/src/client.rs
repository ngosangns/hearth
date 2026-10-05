//! The typed daemon API every client (CLI, TUI, MCP) shares. `Client` is one discovered daemon
//! connection with a method per endpoint, so no caller hand-builds an operation body, a log query
//! or a polling loop; `ManagerClient` caches that connection across calls and re-discovers only
//! after a transport or authentication failure.
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{json, Value};

use hearth_core::catalog::ServiceCatalog;
use hearth_core::state::{
    LogSlice, Operation, OperationStatus, ServiceLifecycleState, ServiceOperationKind,
    PROTOCOL_VERSION,
};

use crate::{
    encode_path_segment, http_client, operation_id, request, require_client_for, Client,
    LocalctlError, LocalctlResult, EXIT_FAILED, EXIT_UNAVAILABLE,
};

const OPERATION_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn unavailable(message: String) -> LocalctlError {
    LocalctlError {
        exit_code: EXIT_UNAVAILABLE,
        message,
    }
}

fn malformed(error: serde_json::Error) -> LocalctlError {
    LocalctlError {
        exit_code: EXIT_FAILED,
        message: error.to_string(),
    }
}

/// The error shapes `request` produces when the connection itself is unusable — the daemon went
/// away, restarted on another port, or rotated its token — as opposed to an API-level rejection.
fn is_connection_failure(message: &str) -> bool {
    message == "manager unavailable"
        || message == "manager request timed out"
        || message.starts_with("unauthorized")
        // A port the dead daemon used to hold answering without the hearth error envelope means
        // something else took it over — the cache entry is worthless either way.
        || message.starts_with("request_failed:")
}

impl Client {
    async fn get(&self, path: &str) -> LocalctlResult<Value> {
        request(self, path, reqwest::Method::GET, None, None)
            .await
            .map_err(unavailable)
    }

    async fn post(&self, path: &str, body: &Value) -> LocalctlResult<Value> {
        request(self, path, reqwest::Method::POST, Some(body), None)
            .await
            .map_err(unavailable)
    }

    /// `GET /v1/manager`.
    pub async fn manager_info(&self) -> LocalctlResult<Value> {
        self.get("/v1/manager").await
    }

    /// `GET /v1/services`.
    pub async fn services(&self) -> LocalctlResult<Vec<ServiceLifecycleState>> {
        let body = self.get("/v1/services").await?;
        serde_json::from_value(body["services"].clone()).map_err(malformed)
    }

    /// `GET /v1/urls` — the `{ urls, unresolved }` body.
    pub async fn urls(&self) -> LocalctlResult<Value> {
        self.get("/v1/urls").await
    }

    /// `GET /v1/catalog` — the catalog the daemon is serving right now, which may be newer than the
    /// one this process loaded at startup (`manager reload`).
    pub async fn catalog(&self) -> LocalctlResult<ServiceCatalog> {
        let body = self.get("/v1/catalog").await?;
        serde_json::from_value(body["catalog"].clone()).map_err(malformed)
    }

    /// `GET /v1/logs/:id`.
    pub async fn log(
        &self,
        service_id: &str,
        cursor: Option<u64>,
        generation: Option<u64>,
        limit: Option<u64>,
    ) -> LocalctlResult<LogSlice> {
        let query: Vec<String> = [
            ("cursor", cursor),
            ("generation", generation),
            ("limit", limit),
        ]
        .iter()
        .filter_map(|(key, value)| value.map(|v| format!("{key}={v}")))
        .collect();
        let suffix = if query.is_empty() {
            String::new()
        } else {
            format!("?{}", query.join("&"))
        };
        let body = self
            .get(&format!(
                "/v1/logs/{}{suffix}",
                encode_path_segment(service_id)
            ))
            .await?;
        serde_json::from_value(body).map_err(malformed)
    }

    /// `GET /v1/operations/:id`.
    pub async fn operation(&self, id: &str) -> LocalctlResult<Operation> {
        let body = self
            .get(&format!(
                "/v1/operations/{}",
                encode_path_segment(&operation_id(id)?)
            ))
            .await?;
        serde_json::from_value(body["operation"].clone()).map_err(malformed)
    }

    /// `POST /v1/operations`. `kill_unowned` must only ever be set after an explicit user
    /// confirmation, and only for `start` — the daemon rejects it otherwise.
    pub async fn submit(
        &self,
        action: ServiceOperationKind,
        service_id: &str,
        kill_unowned: bool,
        request_id: &str,
    ) -> LocalctlResult<Operation> {
        let mut body = json!({ "requestId": request_id, "serviceId": service_id, "action": action.as_wire_str() });
        if kill_unowned {
            body["killUnowned"] = json!(true);
        }
        let response = self.post("/v1/operations", &body).await?;
        serde_json::from_value(response["operation"].clone()).map_err(malformed)
    }

    /// `POST /v1/operations/bulk-start`. `kill_unowned` applies to every target.
    pub async fn bulk_start(
        &self,
        targets: &[String],
        kill_unowned: bool,
        request_id: &str,
    ) -> LocalctlResult<Operation> {
        let mut body = json!({ "requestId": request_id, "targets": targets });
        if kill_unowned {
            body["killUnowned"] = json!(true);
        }
        let response = self.post("/v1/operations/bulk-start", &body).await?;
        serde_json::from_value(response["operation"].clone()).map_err(malformed)
    }

    /// Polls an operation until it is terminal, or until `deadline` passes — in which case the
    /// last (still queued/running) snapshot is returned so the caller can hand out its id.
    pub async fn wait(
        &self,
        id: &str,
        deadline: Option<tokio::time::Instant>,
    ) -> LocalctlResult<Operation> {
        loop {
            let operation = self.operation(id).await?;
            if matches!(
                operation.status,
                OperationStatus::Succeeded | OperationStatus::Failed
            ) || deadline.is_some_and(|d| tokio::time::Instant::now() >= d)
            {
                return Ok(operation);
            }
            tokio::time::sleep(OPERATION_POLL_INTERVAL).await;
        }
    }

    /// Opens `GET /v1/events/stream` (SSE) and returns the response for the caller to read.
    pub async fn event_stream(
        &self,
        after: Option<u64>,
        epoch: Option<&str>,
    ) -> LocalctlResult<reqwest::Response> {
        let mut query: Vec<(&str, String)> = Vec::new();
        if let Some(after) = after {
            query.push(("after", after.to_string()));
        }
        if let Some(epoch) = epoch {
            query.push(("epoch", epoch.to_string()));
        }
        let response = http_client()
            .get(format!(
                "http://127.0.0.1:{}/v1/events/stream",
                self.metadata.port
            ))
            .query(&query)
            .bearer_auth(&self.token)
            .header("x-hearth-protocol", PROTOCOL_VERSION.to_string())
            .header("accept", "text/event-stream")
            .send()
            .await
            .map_err(|_| unavailable("manager unavailable".to_string()))?;
        if !response.status().is_success() {
            return Err(unavailable(format!(
                "event stream failed: {}",
                response.status()
            )));
        }
        Ok(response)
    }
}

/// A project's daemon, discovered lazily and cached between calls. Never spawns a daemon — callers
/// that may need one `ensure` first.
pub struct ManagerClient {
    root: PathBuf,
    catalog: ServiceCatalog,
    cached: Mutex<Option<Client>>,
}

impl ManagerClient {
    /// `catalog` only locates the daemon (its runtime directory); it is never sent anywhere.
    pub fn new(root: PathBuf, catalog: ServiceCatalog) -> Self {
        Self {
            root,
            catalog,
            cached: Mutex::new(None),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The cached connection, or a freshly discovered (and cached) one.
    pub async fn connection(&self) -> LocalctlResult<Client> {
        if let Some(client) = self.cached.lock().unwrap().clone() {
            return Ok(client);
        }
        let client = require_client_for(&self.root, &self.catalog).await?;
        *self.cached.lock().unwrap() = Some(client.clone());
        Ok(client)
    }

    /// Drops the cached connection so the next call re-discovers the daemon.
    pub fn invalidate(&self) {
        *self.cached.lock().unwrap() = None;
    }

    /// Runs `call` against the cached connection. A failure that says the connection itself is
    /// gone (dead port, stale token after an outside `manager restart`) invalidates the cache and
    /// retries ONCE on a freshly discovered connection — a long-lived MCP session otherwise hands
    /// its agent a spurious error for every first call after a restart.
    async fn with<T, F, Fut>(&self, call: F) -> LocalctlResult<T>
    where
        F: Fn(Client) -> Fut,
        Fut: std::future::Future<Output = LocalctlResult<T>>,
    {
        match call(self.connection().await?).await {
            Err(error) if is_connection_failure(&error.message) => {
                self.invalidate();
                call(self.connection().await?).await
            }
            result => result,
        }
    }

    pub async fn manager_info(&self) -> LocalctlResult<Value> {
        self.with(|c| async move { c.manager_info().await }).await
    }

    pub async fn services(&self) -> LocalctlResult<Vec<ServiceLifecycleState>> {
        self.with(|c| async move { c.services().await }).await
    }

    pub async fn urls(&self) -> LocalctlResult<Value> {
        self.with(|c| async move { c.urls().await }).await
    }

    pub async fn catalog(&self) -> LocalctlResult<ServiceCatalog> {
        self.with(|c| async move { c.catalog().await }).await
    }

    pub async fn log(
        &self,
        service_id: &str,
        cursor: Option<u64>,
        generation: Option<u64>,
        limit: Option<u64>,
    ) -> LocalctlResult<LogSlice> {
        self.with(|c| async move { c.log(service_id, cursor, generation, limit).await })
            .await
    }

    pub async fn operation(&self, id: &str) -> LocalctlResult<Operation> {
        self.with(|c| async move { c.operation(id).await }).await
    }

    pub async fn submit(
        &self,
        action: ServiceOperationKind,
        service_id: &str,
        kill_unowned: bool,
    ) -> LocalctlResult<Operation> {
        // Minted once so the retry after a timeout reuses the id. The daemon dedupes on requestId;
        // a second id would start the service twice when the first request actually landed.
        let request_id = uuid::Uuid::new_v4().to_string();
        self.with(|c| {
            let request_id = request_id.clone();
            async move {
                c.submit(action, service_id, kill_unowned, &request_id)
                    .await
            }
        })
        .await
    }

    pub async fn bulk_start(
        &self,
        targets: &[String],
        kill_unowned: bool,
    ) -> LocalctlResult<Operation> {
        let request_id = uuid::Uuid::new_v4().to_string();
        self.with(|c| {
            let request_id = request_id.clone();
            async move { c.bulk_start(targets, kill_unowned, &request_id).await }
        })
        .await
    }

    pub async fn wait(
        &self,
        id: &str,
        deadline: Option<tokio::time::Instant>,
    ) -> LocalctlResult<Operation> {
        self.with(|c| async move { c.wait(id, deadline).await })
            .await
    }

    pub async fn event_stream(
        &self,
        after: Option<u64>,
        epoch: Option<&str>,
    ) -> LocalctlResult<reqwest::Response> {
        self.with(|c| async move { c.event_stream(after, epoch).await })
            .await
    }

    /// A raw `request` against the cached connection, for endpoints without a typed method.
    pub async fn request(
        &self,
        path: &str,
        method: reqwest::Method,
        body: Option<&Value>,
    ) -> LocalctlResult<Value> {
        self.with(|c| {
            let method = method.clone();
            async move {
                request(&c, path, method, body, None)
                    .await
                    .map_err(unavailable)
            }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_transport_and_auth_failures_drop_the_cached_connection() {
        assert!(is_connection_failure("manager unavailable"));
        assert!(is_connection_failure("manager request timed out"));
        assert!(is_connection_failure(
            "unauthorized:Bearer authentication is required"
        ));
        assert!(is_connection_failure("request_failed:502"));
        assert!(!is_connection_failure(
            "service_not_found:Service is not in the catalog"
        ));
        assert!(!is_connection_failure(
            "manager_closing:Manager is shutting down"
        ));
    }
}
