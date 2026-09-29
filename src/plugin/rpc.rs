//! JSON-RPC 2.0, one message per line, and the error codes of the source
//! plugin protocol.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

use crate::api::HttpError;

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL: i64 = -32603;
pub const AUTH_REQUIRED: i64 = -32001;
pub const NOT_FOUND: i64 = -32002;
pub const UNAVAILABLE: i64 = -32003;
pub const RATE_LIMITED: i64 = -32004;
pub const NETWORK: i64 = -32005;

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

pub type RpcResult = Result<Value, RpcError>;

impl RpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self { code, message: message.into(), data: None }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self::new(INVALID_PARAMS, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(NOT_FOUND, message)
    }

    pub fn auth_required() -> Self {
        Self::new(AUTH_REQUIRED, "not signed in")
    }

    fn to_json(&self) -> Value {
        let mut e = json!({"code": self.code, "message": self.message});
        if let Some(data) = &self.data {
            e["data"] = data.clone();
        }
        e
    }
}

/// Map API and transport failures onto the protocol codes.
impl From<anyhow::Error> for RpcError {
    fn from(e: anyhow::Error) -> Self {
        let message = format!("{e:#}");
        if let Some(h) = e.chain().find_map(|c| c.downcast_ref::<HttpError>()) {
            let code = match h.status {
                401 => AUTH_REQUIRED,
                403 => UNAVAILABLE,
                404 => NOT_FOUND,
                429 => RATE_LIMITED,
                500..=599 => NETWORK,
                _ => INTERNAL,
            };
            let mut err = Self::new(code, message);
            if code == RATE_LIMITED {
                err.data = Some(json!({"retry_after": h.retry_after.unwrap_or(30)}));
            }
            return err;
        }
        if e.chain().any(|c| c.downcast_ref::<reqwest::Error>().is_some_and(|r| r.is_timeout() || r.is_connect() || r.is_request())) {
            return Self::new(NETWORK, message);
        }
        Self::new(INTERNAL, message)
    }
}

/// One line read from the host.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Request { id: Value, method: String, params: Value },
    Notification { method: String, params: Value },
    /// Answer to a request of ours.
    Response { id: Value, result: RpcResult },
    /// Unparseable line, or not a JSON-RPC message: answer with this error.
    Invalid { id: Value, error: RpcError },
}

impl PartialEq for RpcError {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code
    }
}

pub fn parse(line: &str) -> Incoming {
    let v: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return Incoming::Invalid { id: Value::Null, error: RpcError::new(PARSE_ERROR, e.to_string()) },
    };
    let id = v.get("id").cloned();
    let params = v.get("params").cloned().unwrap_or(Value::Null);
    match (v.get("method").and_then(Value::as_str), id) {
        (Some(method), Some(id)) => Incoming::Request { id, method: method.to_string(), params },
        (Some(method), None) => Incoming::Notification { method: method.to_string(), params },
        (None, Some(id)) if v.get("error").is_some() => {
            let e = &v["error"];
            let error = RpcError {
                code: e.get("code").and_then(Value::as_i64).unwrap_or(INTERNAL),
                message: e.get("message").and_then(Value::as_str).unwrap_or_default().to_string(),
                data: e.get("data").cloned(),
            };
            Incoming::Response { id, result: Err(error) }
        }
        (None, Some(id)) if v.get("result").is_some() => Incoming::Response { id, result: Ok(v["result"].clone()) },
        (None, id) => Incoming::Invalid {
            id: id.unwrap_or(Value::Null),
            error: RpcError::new(INVALID_REQUEST, "not a JSON-RPC 2.0 message"),
        },
    }
}

/// How long the host gets to answer one of our requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<RpcResult>>>>;

/// Serialised writes to stdout: every task sends whole messages here. Also
/// tracks our own requests until the host answers them.
#[derive(Clone)]
pub struct Out {
    tx: mpsc::UnboundedSender<Value>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
}

impl Out {
    /// Start the stdout writer. It stops once every `Out` is dropped.
    pub fn stdout() -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let task = tokio::spawn(async move {
            let mut stdout = tokio::io::stdout();
            while let Some(msg) = rx.recv().await {
                let mut line = msg.to_string();
                line.push('\n');
                if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                    break;
                }
            }
        });
        (Self::from_sender(tx), task)
    }

    /// `Out` whose messages land in a channel instead of stdout.
    #[cfg(test)]
    pub fn capture() -> (Self, mpsc::UnboundedReceiver<Value>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self::from_sender(tx), rx)
    }

    fn from_sender(tx: mpsc::UnboundedSender<Value>) -> Self {
        Self { tx, pending: Pending::default(), next_id: Arc::default() }
    }

    pub fn respond(&self, id: Value, result: RpcResult) {
        let msg = match result {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e.to_json()}),
        };
        let _ = self.tx.send(msg);
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.tx.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// Plugin → host request; resolves with the host's answer.
    pub async fn request(&self, method: &str, params: Value) -> RpcResult {
        let id = format!("q{}", self.next_id.fetch_add(1, Ordering::Relaxed) + 1);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), tx);
        let _ = self.tx.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let answer = tokio::time::timeout(REQUEST_TIMEOUT, rx).await;
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
        match answer {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(RpcError::new(INTERNAL, "plugin stopping")),
            Err(_) => Err(RpcError::new(INTERNAL, format!("host did not answer {method}"))),
        }
    }

    /// Route the host's answer to the matching [`Out::request`].
    pub fn deliver(&self, id: &Value, result: RpcResult) {
        let waiter = id.as_str().and_then(|id| self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(id));
        match waiter {
            Some(waiter) => {
                let _ = waiter.send(result);
            }
            None => tracing::debug!("answer to unknown request {id}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_classified() {
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":3,"method":"search","params":{"query":"x"}}"#),
            Incoming::Request { id: json!(3), method: "search".into(), params: json!({"query": "x"}) }
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","method":"output.changed","params":{}}"#),
            Incoming::Notification { method: "output.changed".into(), params: json!({}) }
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":"a","result":null}"#),
            Incoming::Response { id: json!("a"), result: Ok(Value::Null) }
        );
        assert_eq!(
            parse(r#"{"jsonrpc":"2.0","id":"a","error":{"code":-32602,"message":"ms"}}"#),
            Incoming::Response { id: json!("a"), result: Err(RpcError::new(INVALID_PARAMS, "ms")) }
        );
        assert!(matches!(parse("{oops"), Incoming::Invalid { error: RpcError { code: PARSE_ERROR, .. }, .. }));
        assert!(matches!(parse(r#"{"id":1}"#), Incoming::Invalid { error: RpcError { code: INVALID_REQUEST, .. }, .. }));
    }

    #[tokio::test]
    async fn requests_get_their_answers() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let out = Out::from_sender(tx);
        let call = tokio::spawn({
            let out = out.clone();
            async move { out.request("player.pause", json!({})).await }
        });
        let sent = rx.recv().await.unwrap();
        assert_eq!(sent["method"], "player.pause");
        out.deliver(&sent["id"], Ok(json!("done")));
        assert_eq!(call.await.unwrap().unwrap(), json!("done"));
    }

    #[test]
    fn http_errors_map_to_protocol_codes() {
        let http = |status, retry_after| anyhow::Error::from(HttpError { status, retry_after, body: String::new() });
        assert_eq!(RpcError::from(http(401, None)).code, AUTH_REQUIRED);
        assert_eq!(RpcError::from(http(404, None).context("album/get")).code, NOT_FOUND);
        let limited = RpcError::from(http(429, Some(12)));
        assert_eq!(limited.code, RATE_LIMITED);
        assert_eq!(limited.data, Some(json!({"retry_after": 12})));
        assert_eq!(RpcError::from(http(503, None)).code, NETWORK);
        assert_eq!(RpcError::from(anyhow::anyhow!("other")).code, INTERNAL);
    }
}
