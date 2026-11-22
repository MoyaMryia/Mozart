use std::{path::PathBuf, time::{Duration, Instant}};

use mozart_core::{config::WebAdapterConfig, ctl::{self, schema::Response, Seqpacket}};
use serde_json::{json, Value};

use crate::error::ApiError;

#[derive(Clone)]
pub struct RpcClient {
    config_path: Option<PathBuf>,
    config: WebAdapterConfig,
}

pub(crate) struct Session {
    pub(crate) conn: Seqpacket,
    deadline: Instant,
    next_id: u32,
    rpc_timeout_ms: u64,
}

fn timeout(sent: bool) -> ApiError {
    ApiError::code("GatewayTimeout", if sent { "控制请求超时，执行结果未知，请查询状态" } else { "连接或发送控制请求超时" })
}

fn remaining(deadline: Instant, sent: bool) -> Result<Duration, ApiError> {
    let duration = deadline.saturating_duration_since(Instant::now());
    if duration.is_zero() { Err(timeout(sent)) } else { Ok(duration) }
}

impl RpcClient {
    pub fn new(config_path: Option<PathBuf>, config: WebAdapterConfig) -> Self { Self { config_path, config } }

    pub(crate) fn connect(&self, command: u16) -> Result<Session, ApiError> {
        let config = match &self.config_path {
            Some(path) => WebAdapterConfig::load_from(Some(path)).map_err(|e| ApiError::code("Internal", format!("读取配置失败：{e}")))?,
            None => self.config.clone(),
        };
        let budget = Duration::from_millis(ctl::request_timeout_ms(config.daemon.rpc_timeout_ms, command));
        let deadline = Instant::now().checked_add(budget).ok_or_else(|| ApiError::code("InvalidArgument", "控制请求超时配置超出系统时钟范围"))?;
        let conn = Seqpacket::connect_timeout(&config.daemon_socket(), Some(remaining(deadline, false)?))
            .map_err(|e| if e.kind() == std::io::ErrorKind::TimedOut { timeout(false) } else { ApiError::code("UpstreamUnavailable", format!("连接 daemon 失败：{e}")) })?;
        Ok(Session { conn, deadline, next_id: 1, rpc_timeout_ms: config.daemon.rpc_timeout_ms })
    }

    pub async fn call(&self, command: u16, body: Value) -> Result<Response, ApiError> {
        let client = self.clone();
        tokio::task::spawn_blocking(move || client.connect(command)?.call(command, body).map_err(|(e, _)| e))
            .await.map_err(|e| ApiError::code("Internal", format!("控制任务失败：{e}")))?
    }

    pub async fn data(&self, command: u16, body: Value) -> Result<Value, ApiError> {
        let response = self.call(command, body).await?;
        response_data(response)
    }

    pub async fn fetch_result(&self, job_id: String) -> Result<Value, ApiError> {
        let client = self.clone();
        tokio::task::spawn_blocking(move || {
            let mut session = client.connect(ctl::DAEMON_GET_STATUS)?;
            let status = response_data(session.call(ctl::DAEMON_GET_STATUS, json!({})).map_err(|(e, _)| e)?)?;
            let supported = status.get("api_capabilities").and_then(Value::as_array)
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("file_fetch_safe_errors_v1")));
            if !supported { return Err(ApiError::code("Unsupported", "daemon 未声明安全文件下载能力")); }
            response_data(session.call(ctl::DAEMON_FILE_FETCH, json!({"job_id":job_id})).map_err(|(e, _)| e)?)
        }).await.map_err(|e| ApiError::code("Internal", format!("控制任务失败：{e}")))?
    }
}

pub(crate) fn response_data(response: Response) -> Result<Value, ApiError> {
    if response.status == "error" { Err(ApiError::remote(response)) } else { Ok(response.data) }
}

impl Session {
    // The boolean marks an operation whose request was sent and response is unknown.
    pub(crate) fn call(&mut self, command: u16, body: Value) -> Result<Response, (ApiError, bool)> {
        if self.next_id != 1 {
            let budget = Duration::from_millis(ctl::request_timeout_ms(self.rpc_timeout_ms, command));
            self.deadline = Instant::now().checked_add(budget)
                .ok_or_else(|| (ApiError::code("InvalidArgument", "控制请求超时配置超出系统时钟范围"), false))?;
        }
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let bytes = serde_json::to_vec(&body).map_err(|e| (ApiError::code("Internal", e.to_string()), false))?;
        let duration = remaining(self.deadline, false).map_err(|e| (e, false))?;
        self.conn.send_timeout(command, id, &bytes, Some(duration)).map_err(|e| {
            let err = if e.kind() == std::io::ErrorKind::TimedOut { timeout(false) } else { ApiError::code("BadGateway", format!("发送控制请求失败：{e}")) };
            (err, false)
        })?;
        let duration = remaining(self.deadline, true).map_err(|e| (e, true))?;
        let message = self.conn.recv(Some(duration)).map_err(|e| {
            let err = if e.kind() == std::io::ErrorKind::TimedOut { timeout(true) } else { ApiError::code("BadGateway", format!("控制响应失败：{e}；执行结果未知，请查询状态")) };
            (err, true)
        })?;
        if message.header.type_ != command || message.header.request_id != id {
            return Err((ApiError::code("BadGateway", "控制响应类型或请求标识不符；执行结果未知，请查询状态"), true));
        }
        let invalid = || (ApiError::code("BadGateway", "控制响应信封无效；执行结果未知，请查询状态"), true);
        let value: Value = serde_json::from_slice(&message.body).map_err(|_| invalid())?;
        let object = value.as_object().ok_or_else(invalid)?;
        if object.len() != 4 || !["status", "code", "reason", "data"].iter().all(|key| object.contains_key(*key)) { return Err(invalid()); }
        let response: Response = serde_json::from_value(value).map_err(|_| invalid())?;
        let valid = match response.status.as_str() {
            "ok" => response.code == "Ok" && response.reason.is_empty(),
            "error" => response.code != "Ok" && !response.code.is_empty() && !response.reason.is_empty() && response.data.is_null(),
            _ => false,
        };
        if !valid { return Err(invalid()); }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(reply: &'static [u8]) -> (tempfile::TempDir, RpcClient, std::thread::JoinHandle<()>) {
        let dir = tempfile::tempdir().unwrap();
        let mut config = WebAdapterConfig::load_from(None).unwrap();
        config.daemon.run_dir = dir.path().to_owned();
        config.daemon.socket = "test.sock".into();
        let listener = Seqpacket::bind(&config.daemon_socket()).unwrap();
        listener.listen(1).unwrap();
        let thread = std::thread::spawn(move || {
            let conn = listener.accept().unwrap();
            let request = conn.recv(Some(Duration::from_secs(2))).unwrap();
            conn.send(request.header.type_, request.header.request_id, reply).unwrap();
        });
        (dir, RpcClient::new(None, config), thread)
    }

    #[tokio::test]
    async fn error_envelope_is_preserved_and_data_maps_error() {
        let (_dir, client, thread) = server(br#"{"status":"error","code":"Busy","reason":"queue full","data":null}"#);
        let response = client.call(ctl::DAEMON_GET_STATUS, json!({})).await.unwrap();
        assert_eq!(response.code, "Busy");
        assert_eq!(response_data(response).unwrap_err().reason, "queue full");
        thread.join().unwrap();
    }

    #[tokio::test]
    async fn rejects_invalid_envelope() {
        let (_dir, client, thread) = server(br#"{"status":"ok","code":"Ok","reason":"","data":{},"extra":1}"#);
        assert_eq!(client.call(ctl::DAEMON_GET_STATUS, json!({})).await.unwrap_err().code, "BadGateway");
        thread.join().unwrap();
    }

    #[tokio::test]
    async fn fetch_has_separate_budgets_for_status_and_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = WebAdapterConfig::load_from(None).unwrap();
        config.daemon.run_dir = dir.path().to_owned();
        config.daemon.socket = "test.sock".into();
        config.daemon.rpc_timeout_ms = 800;
        let listener = Seqpacket::bind(&config.daemon_socket()).unwrap();
        listener.listen(1).unwrap();
        let thread = std::thread::spawn(move || {
            let conn = listener.accept().unwrap();
            for data in [json!({"api_capabilities":["file_fetch_safe_errors_v1"]}), json!({"path":"result"})] {
                let request = conn.recv(Some(Duration::from_secs(3))).unwrap();
                std::thread::sleep(Duration::from_millis(500));
                let reply = json!({"status":"ok", "code":"Ok", "reason":"", "data":data});
                conn.send(request.header.type_, request.header.request_id, &serde_json::to_vec(&reply).unwrap()).unwrap();
            }
        });
        let result = RpcClient::new(None, config).fetch_result("job".into()).await.unwrap();
        assert_eq!(result["path"], "result");
        thread.join().unwrap();
    }

    #[tokio::test]
    async fn fetch_requires_capability() {
        let (_dir, client, thread) = server(br#"{"status":"ok","code":"Ok","reason":"","data":{}}"#);
        assert_eq!(client.fetch_result("job".into()).await.unwrap_err().code, "Unsupported");
        thread.join().unwrap();
    }
}
