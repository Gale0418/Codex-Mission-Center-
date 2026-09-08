//! Mission Center 的 bounded、唯讀 runtime walking skeleton。
//!
//! Runtime 是不可信 telemetry ingress：所有輸入先做 bytes/schema/privacy
//! 驗證，再進入固定上限的 in-memory reducer。這個 crate 不讀寫
//! `MissionCenter/tasks.md`，也不保存 prompt、reasoning 或 secret。

use mission_center_core::scan_forbidden_content;
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt;
#[cfg(not(windows))]
use std::fs::OpenOptions;
use std::io::{self, BufRead, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd"
))]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: &str = "1.0";
pub const MAX_EVENT_BYTES: usize = 1024 * 1024;
pub const MAX_REPLAY_EVENTS: usize = 10_000;
pub const MAX_REPLAY_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_JSON_DEPTH: usize = 32;
pub const MAX_JSON_NODES: usize = 2048;
pub const MAX_FIELD_BYTES: usize = 512;
pub const MAX_LIST_ITEMS: usize = 64;
pub const MAX_AGENTS: usize = 64;
pub const MAX_RECENT_EVENT_IDS: usize = 20;
pub const STALE_AFTER: Duration = Duration::from_secs(60);
pub const HUD_RUNTIME_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const MAX_HOOK_INPUT_BYTES: usize = 64 * 1024;
pub const MAX_STDIN_BYTES: usize = MAX_HOOK_INPUT_BYTES;
pub const MAX_HTTP_REQUEST_BYTES: usize = 16 * 1024;
pub const MAX_HTTP_BODY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_HUD_ASSET_COUNT: usize = 8;
pub const MAX_HUD_ASSET_TOTAL_BYTES: usize = 32 * 1024 * 1024;
pub const LAUNCH_COOLDOWN: Duration = Duration::from_secs(3);
pub const COOLDOWN_SECONDS: u64 = 3;
pub const HEALTH_TIMEOUT: Duration = Duration::from_millis(500);
pub const HEALTH_TIMEOUT_MILLIS: u64 = 500;
pub const HUD_ASSET_DIR: &str = "mission-center-assets";
pub const RUNTIME_STATE_FILE: &str = "runtime-state.json";
pub const HUD_ASSETS: &[&str] = &[
    "visual-summary.html",
    "visual-state.json",
    "mission-bridge-background.webp",
    "mission-bridge-background.webp.json",
    "mission-fleet-bridge-background.webp",
    "mission-fleet-bridge-background.webp.json",
    "mission-starfield.webp",
    "mission-starfield.webp.json",
];
pub const HUD_ALLOWED_ASSETS: &[&str] = HUD_ASSETS;
/// Files included in the launch identity. `visual-state.json` is optional
/// derived state and therefore is served when present but does not prevent a
/// launcher from starting (matching the Python oracle).
pub const HUD_MANAGED_ASSETS: &[&str] = &[
    "mission-bridge-background.webp",
    "mission-bridge-background.webp.json",
    "mission-fleet-bridge-background.webp",
    "mission-fleet-bridge-background.webp.json",
    "mission-starfield.webp",
    "mission-starfield.webp.json",
    "visual-summary.html",
];
// Compatibility names shared by the Python adapter and external callers.
pub const MAX_RUNTIME_LINE_BYTES: usize = MAX_EVENT_BYTES;
pub const MAX_REPLAY_FILE_BYTES: usize = MAX_REPLAY_BYTES;
pub const MAX_RUNTIME_FIELD_LENGTH: usize = MAX_FIELD_BYTES;
pub const MAX_RUNTIME_VALUE_DEPTH: usize = MAX_JSON_DEPTH;
pub const MAX_RUNTIME_VALUE_NODES: usize = MAX_JSON_NODES;
pub const MAX_RUNTIME_LIST_LENGTH: usize = MAX_LIST_ITEMS;
pub const MAX_AGENT_STATE_COUNT: usize = MAX_AGENTS;
pub const MAX_TASK_ID_COUNT: usize = MAX_LIST_ITEMS;
pub const MAX_LINK_AGENT_COUNT: usize = MAX_AGENTS;

/// Stable machine-readable errors with bounded remediation text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidJson,
    ValidationFailed,
    InvalidUtf8,
    Schema,
    EventTooLarge,
    ReplayEventLimit,
    ReplayByteLimit,
    JsonDepthLimit,
    JsonNodeLimit,
    FieldTooLong,
    ItemLimit,
    Duplicate,
    OutOfOrder,
    PrivacyViolation,
    InvalidTaskLink,
    UnknownTask,
    AgentLimit,
    Stale,
    Io,
    UnsupportedTransport,
    InvalidHost,
    PortBind,
    HealthMismatch,
    ReuseRejected,
    VersionMismatch,
    HookInputTooLarge,
    PathTraversal,
    AssetUnavailable,
    UnsafePath,
    Shutdown,
    BrowserUnavailable,
}
impl ErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::ValidationFailed => "validation_failed",
            Self::InvalidUtf8 => "invalid_utf8",
            Self::Schema => "schema_error",
            Self::EventTooLarge => "event_too_large",
            Self::ReplayEventLimit => "replay_event_limit",
            Self::ReplayByteLimit => "replay_byte_limit",
            Self::JsonDepthLimit => "json_depth_limit",
            Self::JsonNodeLimit => "json_node_limit",
            Self::FieldTooLong => "field_too_long",
            Self::ItemLimit => "item_limit",
            Self::Duplicate => "duplicate_event",
            Self::OutOfOrder => "out_of_order",
            Self::PrivacyViolation => "privacy_violation",
            Self::InvalidTaskLink => "invalid_task_link",
            Self::UnknownTask => "unknown_task",
            Self::AgentLimit => "agent_limit",
            Self::Stale => "stale",
            Self::Io => "io_error",
            Self::UnsupportedTransport => "unsupported_transport",
            Self::InvalidHost => "invalid_host",
            Self::PortBind => "port_bind_failed",
            Self::HealthMismatch => "health_mismatch",
            Self::ReuseRejected => "reuse_rejected",
            Self::VersionMismatch => "version_mismatch",
            Self::HookInputTooLarge => "hook_input_too_large",
            Self::PathTraversal => "path_traversal",
            Self::AssetUnavailable => "asset_unavailable",
            Self::UnsafePath => "unsafe_path",
            Self::Shutdown => "shutdown_failed",
            Self::BrowserUnavailable => "browser_unavailable",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeError {
    pub code: ErrorCode,
    pub message: String,
}
impl RuntimeError {
    fn new(code: ErrorCode, _detail: impl AsRef<str>) -> Self {
        let detail = match code {
            ErrorCode::InvalidJson => "JSON 格式無效",
            ErrorCode::ValidationFailed => "輸入驗證失敗",
            ErrorCode::InvalidUtf8 => "輸入不是 UTF-8",
            ErrorCode::Schema => "runtime schema 不符合契約",
            ErrorCode::EventTooLarge => "event 超過 1 MiB",
            ErrorCode::ReplayEventLimit => "replay 事件超過 10,000 筆",
            ErrorCode::ReplayByteLimit => "replay 超過 8 MiB",
            ErrorCode::JsonDepthLimit => "JSON 深度超過 32",
            ErrorCode::JsonNodeLimit => "JSON 節點超過 2,048",
            ErrorCode::FieldTooLong => "文字欄位超過 512 bytes",
            ErrorCode::ItemLimit => "陣列或物件項目超過 64",
            ErrorCode::Duplicate => "event 已存在",
            ErrorCode::OutOfOrder => "event sequence 過舊",
            ErrorCode::PrivacyViolation => "輸入含禁止的隱私或 secret 內容",
            ErrorCode::InvalidTaskLink => "task link 不符合契約",
            ErrorCode::UnknownTask => "taskId 不在 caller allowlist",
            ErrorCode::AgentLimit => "agent 數量超過 64",
            ErrorCode::Stale => "runtime 已超過 60 秒未更新",
            ErrorCode::Io => "stdio I/O 失敗或已斷線",
            ErrorCode::UnsupportedTransport => "transport 尚未支援",
            ErrorCode::InvalidHost => "HTTP 只允許 loopback host",
            ErrorCode::PortBind => "loopback port bind 失敗",
            ErrorCode::HealthMismatch => "server health 身分驗證不符",
            ErrorCode::ReuseRejected => "不安全的既有 server reuse 已拒絕",
            ErrorCode::VersionMismatch => "既有 server 版本不符",
            ErrorCode::HookInputTooLarge => "hook 輸入超過 64 KiB",
            ErrorCode::PathTraversal => "HTTP 路徑含 traversal",
            ErrorCode::AssetUnavailable => "HUD asset 不可用",
            ErrorCode::UnsafePath => "檔案路徑含 symlink 或 reparse point",
            ErrorCode::Shutdown => "server shutdown 失敗",
            ErrorCode::BrowserUnavailable => "瀏覽器 opener 不可用；server 仍可使用",
        };
        Self {
            code,
            message: bounded_text(&format!("{detail}；請修正輸入後重試"), MAX_FIELD_BYTES),
        }
    }
    pub const fn code_str(&self) -> &'static str {
        self.code.as_str()
    }
    pub const fn code(&self) -> ErrorCode {
        self.code
    }
}
impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}
impl std::error::Error for RuntimeError {}
impl From<io::Error> for RuntimeError {
    fn from(value: io::Error) -> Self {
        Self::new(ErrorCode::Io, value.to_string())
    }
}
pub type RuntimeResult<T> = Result<T, RuntimeError>;

fn bounded_text(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Working,
    WaitingApproval,
    Blocked,
    Finished,
    Failed,
    Stale,
    Disconnected,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    None,
    Approval,
    Question,
    Blocked,
    Error,
    Verification,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityKind {
    Unknown,
    Idle,
    Working,
    CommandExecution,
    FileChange,
    ToolUse,
    WebSearch,
    WaitingInput,
    Verification,
    Blocked,
    Error,
}
fn default_activity_kind() -> ActivityKind {
    ActivityKind::Unknown
}

/// 嚴格 versioned event envelope；未知欄位與缺少必要欄位都拒絕。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventEnvelope {
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    #[serde(rename = "eventId")]
    pub event_id: String,
    pub timestamp: String,
    pub provider: String,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "threadId")]
    pub thread_id: Option<String>,
    #[serde(default)]
    #[serde(rename = "turnId")]
    pub turn_id: Option<String>,
    #[serde(rename = "agentId")]
    pub agent_id: String,
    #[serde(rename = "parentAgentId")]
    pub parent_agent_id: Option<String>,
    #[serde(rename = "taskIds")]
    pub task_ids: Vec<String>,
    #[serde(rename = "eventType")]
    pub event_type: String,
    pub activity: String,
    pub attention: AttentionKind,
    pub sequence: u64,
    pub state: AgentState,
    #[serde(default = "default_activity_kind", rename = "activityKind")]
    pub activity_kind: ActivityKind,
}

fn valid_id(value: &str, task: bool) -> bool {
    let limit = if task { 64 } else { 128 };
    !value.is_empty()
        && value.len() <= limit
        && value.bytes().enumerate().all(|(i, b)| {
            b.is_ascii_alphanumeric() || (i > 0 && matches!(b, b'.' | b'_' | b':' | b'-'))
        })
}
fn check_bounds(value: &Value) -> RuntimeResult<()> {
    let mut stack = vec![(value, 0usize)];
    let mut nodes = 0usize;
    while let Some((current, depth)) = stack.pop() {
        if depth > MAX_JSON_DEPTH {
            return Err(RuntimeError::new(
                ErrorCode::JsonDepthLimit,
                "JSON 巢狀深度超過 32",
            ));
        }
        nodes += 1;
        if nodes > MAX_JSON_NODES {
            return Err(RuntimeError::new(
                ErrorCode::JsonNodeLimit,
                "JSON 節點數超過 2048",
            ));
        }
        match current {
            Value::String(s) if s.len() > MAX_FIELD_BYTES => {
                return Err(RuntimeError::new(
                    ErrorCode::FieldTooLong,
                    "文字欄位超過 512 bytes",
                ));
            }
            Value::Object(map) => {
                if map.len() > MAX_LIST_ITEMS {
                    return Err(RuntimeError::new(
                        ErrorCode::ItemLimit,
                        "物件欄位超過 64 個",
                    ));
                }
                for (key, nested) in map {
                    if key.len() > MAX_FIELD_BYTES {
                        return Err(RuntimeError::new(
                            ErrorCode::FieldTooLong,
                            "欄位名稱超過 512 bytes",
                        ));
                    }
                    stack.push((nested, depth + 1));
                }
            }
            Value::Array(items) => {
                if items.len() > MAX_LIST_ITEMS {
                    return Err(RuntimeError::new(
                        ErrorCode::ItemLimit,
                        "陣列項目超過 64 個",
                    ));
                }
                for nested in items {
                    stack.push((nested, depth + 1));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn two_digits(bytes: &[u8], at: usize) -> Option<u32> {
    let first = *bytes.get(at)?;
    let second = *bytes.get(at + 1)?;
    if !first.is_ascii_digit() || !second.is_ascii_digit() {
        return None;
    }
    Some(u32::from(first - b'0') * 10 + u32::from(second - b'0'))
}

fn valid_rfc3339(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || !bytes.get(4).is_some_and(|b| *b == b'-')
        || !bytes.get(7).is_some_and(|b| *b == b'-')
        || !bytes.get(10).is_some_and(|b| *b == b'T' || *b == b't')
        || !bytes.get(13).is_some_and(|b| *b == b':')
        || !bytes.get(16).is_some_and(|b| *b == b':')
    {
        return false;
    }
    if !bytes[..4].iter().all(u8::is_ascii_digit) {
        return false;
    }
    let month = two_digits(bytes, 5);
    let day = two_digits(bytes, 8);
    let hour = two_digits(bytes, 11);
    let minute = two_digits(bytes, 14);
    let second = two_digits(bytes, 17);
    let (Some(month), Some(day), Some(hour), Some(minute), Some(second)) =
        (month, day, hour, minute, second)
    else {
        return false;
    };
    if !(1..=12).contains(&month)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || second > 60
    {
        return false;
    }
    let leap = u32::from(bytes[0].wrapping_sub(b'0')) * 1000
        + u32::from(bytes[1].wrapping_sub(b'0')) * 100
        + u32::from(bytes[2].wrapping_sub(b'0')) * 10
        + u32::from(bytes[3].wrapping_sub(b'0'));
    let days = [
        31,
        if leap % 4 == 0 && (leap % 100 != 0 || leap % 400 == 0) {
            29
        } else {
            28
        },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day == 0 || day > days[(month - 1) as usize] {
        return false;
    }
    let mut at = 19;
    if bytes.get(at) == Some(&b'.') {
        at += 1;
        let start = at;
        while bytes.get(at).is_some_and(u8::is_ascii_digit) {
            at += 1;
        }
        if at == start {
            return false;
        }
    }
    match bytes.get(at) {
        Some(b'Z' | b'z') => at += 1,
        Some(b'+' | b'-') => {
            if at + 6 != bytes.len()
                || bytes.get(at + 3) != Some(&b':')
                || !bytes[at + 1..at + 3].iter().all(u8::is_ascii_digit)
                || !bytes[at + 4..].iter().all(u8::is_ascii_digit)
            {
                return false;
            }
            let oh = two_digits(bytes, at + 1).unwrap_or(99);
            let om = two_digits(bytes, at + 4).unwrap_or(99);
            if oh > 23 || om > 59 {
                return false;
            }
            at += 6;
        }
        _ => return false,
    }
    at == bytes.len()
}

/// 使用 core 的共用 scanner；命中即拒絕，絕不清洗。
pub fn scan_privacy(value: &Value) -> Vec<String> {
    if check_bounds(value).is_err() {
        return vec!["$ bounded input violation".to_owned()];
    }
    scan_forbidden_content(value)
}

struct StrictJson(Value);
struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = StrictJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON value")
    }
    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJson(Value::Null))
    }
    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJson(Value::Bool(value)))
    }
    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJson(Value::Number(value.into())))
    }
    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJson(Value::Number(value.into())))
    }
    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(StrictJson)
            .ok_or_else(|| E::custom("non-finite number"))
    }
    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJson(Value::String(value.to_owned())))
    }
    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(StrictJson(Value::String(value)))
    }
    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element::<StrictJson>()? {
            values.push(value.0);
        }
        Ok(StrictJson(Value::Array(values)))
    }
    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("duplicate JSON key"));
            }
            let value = map.next_value::<StrictJson>()?;
            values.insert(key, value.0);
        }
        Ok(StrictJson(Value::Object(values)))
    }
}

impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

impl EventEnvelope {
    pub fn parse(bytes: &[u8]) -> RuntimeResult<Self> {
        Self::from_json_bytes(bytes)
    }
    pub fn from_json_bytes(bytes: &[u8]) -> RuntimeResult<Self> {
        if bytes.len() > MAX_EVENT_BYTES {
            return Err(RuntimeError::new(
                ErrorCode::EventTooLarge,
                "event UTF-8 bytes 超過 1 MiB",
            ));
        }
        std::str::from_utf8(bytes)
            .map_err(|_| RuntimeError::new(ErrorCode::InvalidUtf8, "event 不是 UTF-8"))?;
        let value = match serde_json::from_slice::<StrictJson>(bytes) {
            Ok(value) => value.0,
            Err(error) if error.to_string().contains("duplicate JSON key") => {
                return Err(RuntimeError::new(
                    ErrorCode::ValidationFailed,
                    "JSON 不得包含重複欄位",
                ));
            }
            Err(error) => {
                return Err(RuntimeError::new(ErrorCode::InvalidJson, error.to_string()));
            }
        };
        Self::from_value(value)
    }
    pub fn from_json(value: &str) -> RuntimeResult<Self> {
        Self::from_json_bytes(value.as_bytes())
    }
    pub fn from_value(value: Value) -> RuntimeResult<Self> {
        let canonical_bytes = serde_json::to_vec(&value)
            .map_err(|_| RuntimeError::new(ErrorCode::InvalidJson, "canonical JSON failed"))?;
        if canonical_bytes.len() > MAX_EVENT_BYTES {
            return Err(RuntimeError::new(
                ErrorCode::EventTooLarge,
                "event UTF-8 bytes 超過 1 MiB",
            ));
        }
        check_bounds(&value)?;
        if !scan_privacy(&value).is_empty() {
            return Err(RuntimeError::new(
                ErrorCode::PrivacyViolation,
                "event 含禁止的 privacy/secret 內容",
            ));
        }
        if value.get("schemaVersion") != Some(&Value::String(SCHEMA_VERSION.to_owned())) {
            return Err(RuntimeError::new(
                ErrorCode::Schema,
                "schemaVersion 必須為 1.0",
            ));
        }
        let event: Self = serde_json::from_value(value)
            .map_err(|e| RuntimeError::new(ErrorCode::Schema, e.to_string()))?;
        event.validate()
    }
    pub fn validate(&self) -> RuntimeResult<Self> {
        let value = serde_json::to_value(self)
            .map_err(|_| RuntimeError::new(ErrorCode::Schema, "event serialization failed"))?;
        check_bounds(&value)?;
        if !scan_privacy(&value).is_empty() {
            return Err(RuntimeError::new(
                ErrorCode::PrivacyViolation,
                "event 含禁止的 privacy/secret 內容",
            ));
        }
        if self.schema_version != SCHEMA_VERSION || self.provider != "codex" {
            return Err(RuntimeError::new(
                ErrorCode::Schema,
                "schemaVersion/provider 不受支援",
            ));
        }
        for (name, value) in [
            ("timestamp", &self.timestamp),
            ("provider", &self.provider),
            ("eventId", &self.event_id),
            ("sessionId", &self.session_id),
            ("agentId", &self.agent_id),
            ("eventType", &self.event_type),
            ("activity", &self.activity),
        ] {
            if value.is_empty() {
                return Err(RuntimeError::new(
                    ErrorCode::Schema,
                    format!("{name} 不可為空"),
                ));
            }
            if value.len() > MAX_FIELD_BYTES {
                return Err(RuntimeError::new(
                    ErrorCode::FieldTooLong,
                    format!("{name} 超過 512 bytes"),
                ));
            }
        }
        if !valid_rfc3339(&self.timestamp) {
            return Err(RuntimeError::new(
                ErrorCode::Schema,
                "timestamp 必須為 RFC3339",
            ));
        }
        for (name, value) in [
            ("eventId", Some(&self.event_id)),
            ("sessionId", Some(&self.session_id)),
            ("threadId", self.thread_id.as_ref()),
            ("turnId", self.turn_id.as_ref()),
            ("agentId", Some(&self.agent_id)),
            ("parentAgentId", self.parent_agent_id.as_ref()),
        ] {
            if let Some(value) = value
                && !valid_id(value, false)
            {
                return Err(RuntimeError::new(
                    ErrorCode::Schema,
                    format!("{name} 不是合法 opaque ID"),
                ));
            }
        }
        if self.task_ids.len() > MAX_LIST_ITEMS
            || self.task_ids.iter().any(|id| !valid_id(id, true))
            || self.task_ids.iter().collect::<HashSet<_>>().len() != self.task_ids.len()
        {
            return Err(RuntimeError::new(
                ErrorCode::InvalidTaskLink,
                "taskIds 必須是 <=64 個不重複明確 ID",
            ));
        }
        Ok(self.clone())
    }
}

/// Caller 提供 canonical task allowlist；runtime 僅驗證明確 taskId 是否存在。
#[derive(Debug, Clone, Default)]
pub struct TaskLinks {
    allowlist: HashSet<String>,
    links: BTreeMap<String, Vec<String>>,
}
impl TaskLinks {
    pub fn new<I, S>(allowed_tasks: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            allowlist: allowed_tasks.into_iter().map(Into::into).collect(),
            links: BTreeMap::new(),
        }
    }
    pub fn link(&mut self, agent_id: &str, task_ids: &[String]) -> RuntimeResult<()> {
        if !valid_id(agent_id, false)
            || task_ids.len() > MAX_LIST_ITEMS
            || task_ids.iter().any(|id| !valid_id(id, true))
            || task_ids.iter().collect::<HashSet<_>>().len() != task_ids.len()
        {
            return Err(RuntimeError::new(
                ErrorCode::InvalidTaskLink,
                "agent/task link 格式或數量無效",
            ));
        }
        if task_ids.iter().any(|id| !self.allowlist.contains(id)) {
            return Err(RuntimeError::new(
                ErrorCode::UnknownTask,
                "taskId 不在 caller allowlist",
            ));
        }
        self.links.insert(agent_id.to_owned(), task_ids.to_vec());
        Ok(())
    }
    pub fn get(&self, agent_id: &str) -> &[String] {
        self.links.get(agent_id).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Validate a complete link map against the caller's canonical task allowlist.
pub fn validate_task_links(
    links: &BTreeMap<String, Vec<String>>,
    allowed_tasks: &HashSet<String>,
) -> RuntimeResult<()> {
    if links.len() > MAX_LINK_AGENT_COUNT {
        return Err(RuntimeError::new(
            ErrorCode::ItemLimit,
            "task link agent 數量超過 64",
        ));
    }
    let mut validated = TaskLinks::new(allowed_tasks.iter().cloned());
    for (agent_id, task_ids) in links {
        validated.link(agent_id, task_ids)?;
    }
    Ok(())
}

/// Free-function ingress helper for adapters that do not need to name the type.
pub fn validate_runtime_event(event: EventEnvelope) -> RuntimeResult<EventEnvelope> {
    event.validate()
}

pub fn parse_runtime_event(bytes: &[u8]) -> RuntimeResult<EventEnvelope> {
    EventEnvelope::from_json_bytes(bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSnapshot {
    pub provider: String,
    pub session_id: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub agent_id: String,
    pub parent_agent_id: Option<String>,
    pub task_ids: Vec<String>,
    pub state: AgentState,
    pub activity: String,
    pub attention: AttentionKind,
    pub requires_attention: bool,
    pub started_at: String,
    pub last_seen_at: String,
    pub sequence: u64,
    pub recent_event_ids: VecDeque<String>,
    pub activity_kind: ActivityKind,
    last_seen: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceStatus {
    Connected,
    Replay,
    File,
    Stale,
    Disconnected,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderCapabilities {
    pub approve: bool,
    pub reject: bool,
    pub focus: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttentionRecord {
    #[serde(rename = "agentId")]
    pub agent_id: String,
    pub kind: AttentionKind,
    pub activity: String,
    #[serde(rename = "taskIds")]
    pub task_ids: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentRecord {
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(default)]
    #[serde(rename = "threadId")]
    pub thread_id: Option<String>,
    #[serde(rename = "turnId")]
    pub turn_id: Option<String>,
    #[serde(rename = "agentId")]
    pub agent_id: String,
    #[serde(rename = "parentAgentId")]
    pub parent_agent_id: Option<String>,
    #[serde(rename = "taskIds")]
    pub task_ids: Vec<String>,
    pub state: AgentState,
    pub activity: String,
    pub attention: AttentionKind,
    #[serde(rename = "requiresAttention")]
    pub requires_attention: bool,
    #[serde(default)]
    #[serde(rename = "startedAt")]
    pub started_at: String,
    #[serde(default)]
    #[serde(rename = "lastSeenAt")]
    pub last_seen_at: String,
    #[serde(default)]
    pub sequence: u64,
    #[serde(default)]
    #[serde(rename = "recentEventIds")]
    pub recent_event_ids: Vec<String>,
    #[serde(rename = "activityKind")]
    pub activity_kind: ActivityKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeState {
    #[serde(rename = "schemaVersion")]
    pub schema_version: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "sourceStatus")]
    pub source_status: SourceStatus,
    pub capabilities: ProviderCapabilities,
    pub attention: Vec<AttentionRecord>,
    pub agents: Vec<AgentRecord>,
}
impl RuntimeState {
    pub fn validate(&self) -> RuntimeResult<Self> {
        let value = serde_json::to_value(self).map_err(|_| {
            RuntimeError::new(ErrorCode::Schema, "RuntimeState serialization failed")
        })?;
        check_bounds(&value)?;
        if !scan_privacy(&value).is_empty() {
            return Err(RuntimeError::new(
                ErrorCode::PrivacyViolation,
                "RuntimeState 含禁止內容",
            ));
        }
        if self.schema_version != SCHEMA_VERSION
            || !valid_rfc3339(&self.updated_at)
            || self.agents.len() > MAX_AGENTS
            || self.attention.len() > MAX_AGENTS
        {
            return Err(RuntimeError::new(
                ErrorCode::Schema,
                "RuntimeState schema 或數量無效",
            ));
        }
        for agent in &self.agents {
            if !(agent.provider.is_empty() || agent.provider == "codex")
                || !valid_id(&agent.agent_id, false)
                || agent.task_ids.len() > MAX_LIST_ITEMS
                || agent.task_ids.iter().any(|id| !valid_id(id, true))
                || agent.activity.len() > MAX_FIELD_BYTES
                || (!agent.last_seen_at.is_empty() && !valid_rfc3339(&agent.last_seen_at))
                || (!agent.started_at.is_empty() && !valid_rfc3339(&agent.started_at))
                || (!agent.session_id.is_empty() && !valid_id(&agent.session_id, false))
                || agent.recent_event_ids.len() > MAX_RECENT_EVENT_IDS
                || agent.recent_event_ids.iter().any(|id| !valid_id(id, false))
            {
                return Err(RuntimeError::new(
                    ErrorCode::Schema,
                    "RuntimeState agent 欄位無效",
                ));
            }
        }
        for item in &self.attention {
            if matches!(item.kind, AttentionKind::None)
                || !valid_id(&item.agent_id, false)
                || item.activity.is_empty()
                || item.task_ids.len() > MAX_LIST_ITEMS
                || item.task_ids.iter().any(|id| !valid_id(id, true))
            {
                return Err(RuntimeError::new(
                    ErrorCode::Schema,
                    "RuntimeState attention 欄位無效",
                ));
            }
        }
        Ok(self.clone())
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyResult {
    Applied,
    Duplicate,
    OutOfOrder,
}
#[derive(Debug, Clone)]
pub struct RuntimeReducer {
    agents: BTreeMap<String, AgentSnapshot>,
    canonical_tasks: HashSet<String>,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
    events_applied: usize,
    source_status: String,
    last_transport: Instant,
}
impl Default for RuntimeReducer {
    fn default() -> Self {
        Self::new()
    }
}
impl RuntimeReducer {
    pub fn new() -> Self {
        Self {
            agents: BTreeMap::new(),
            canonical_tasks: HashSet::new(),
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            events_applied: 0,
            source_status: "disconnected".to_owned(),
            last_transport: Instant::now(),
        }
    }
    pub fn touch_transport(&mut self) {
        self.last_transport = Instant::now();
        self.source_status = "connected".to_owned();
    }
    pub fn with_task_allowlist<I, S>(allowed_tasks: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut reducer = Self::new();
        reducer.canonical_tasks = allowed_tasks.into_iter().map(Into::into).collect();
        reducer
    }
    pub fn apply(&mut self, event: EventEnvelope) -> RuntimeResult<ApplyResult> {
        let allowed = self.canonical_tasks.clone();
        self.apply_with_allowlist(event, &allowed)
    }
    pub fn apply_with_allowlist(
        &mut self,
        event: EventEnvelope,
        allowed_tasks: &HashSet<String>,
    ) -> RuntimeResult<ApplyResult> {
        let event = event.validate()?;
        if event
            .task_ids
            .iter()
            .any(|task| !allowed_tasks.contains(task))
        {
            return Err(RuntimeError::new(
                ErrorCode::UnknownTask,
                "taskId 不在 caller allowlist",
            ));
        }
        self.touch_transport();
        if self.seen.contains(&event.event_id) {
            return Ok(ApplyResult::Duplicate);
        }
        if self
            .agents
            .get(&event.agent_id)
            .is_some_and(|current| event.sequence <= current.sequence)
        {
            return Ok(ApplyResult::OutOfOrder);
        }
        if !self.agents.contains_key(&event.agent_id) && self.agents.len() >= MAX_AGENTS {
            return Err(RuntimeError::new(
                ErrorCode::AgentLimit,
                "agent state 上限為 64",
            ));
        }
        let now = Instant::now();
        let started_at = self
            .agents
            .get(&event.agent_id)
            .map(|a| a.started_at.clone())
            .unwrap_or_else(|| event.timestamp.clone());
        let mut recent = self
            .agents
            .get(&event.agent_id)
            .map(|a| a.recent_event_ids.clone())
            .unwrap_or_default();
        recent.push_back(event.event_id.clone());
        while recent.len() > MAX_RECENT_EVENT_IDS {
            recent.pop_front();
        }
        let id = event.event_id.clone();
        let snapshot = AgentSnapshot {
            provider: event.provider,
            session_id: event.session_id,
            thread_id: event.thread_id,
            turn_id: event.turn_id,
            agent_id: event.agent_id.clone(),
            parent_agent_id: event.parent_agent_id,
            task_ids: event.task_ids,
            state: event.state,
            activity: event.activity,
            attention: event.attention,
            requires_attention: !matches!(event.attention, AttentionKind::None),
            started_at,
            last_seen_at: event.timestamp,
            sequence: event.sequence,
            recent_event_ids: recent,
            activity_kind: event.activity_kind,
            last_seen: now,
        };
        self.agents.insert(event.agent_id, snapshot);
        self.events_applied = self.events_applied.saturating_add(1);
        self.seen.insert(id.clone());
        self.seen_order.push_back(id);
        // Keep every replay ID (10k); this is bounded and makes replay deterministic.
        while self.seen_order.len() > MAX_REPLAY_EVENTS {
            if let Some(old) = self.seen_order.pop_front() {
                self.seen.remove(&old);
            }
        }
        Ok(ApplyResult::Applied)
    }
    pub fn age(&mut self, socket_closed: bool) {
        self.age_at(Instant::now(), socket_closed);
    }
    /// 允許測試／host 以單調時鐘明確判斷 stale，避免依賴 wall clock。
    pub fn age_at(&mut self, now: Instant, socket_closed: bool) {
        let stale = now.saturating_duration_since(self.last_transport) >= STALE_AFTER;
        if socket_closed {
            self.source_status = "disconnected".to_owned();
        } else if stale {
            self.source_status = "stale".to_owned();
        }
        let mut any_agent_stale = false;
        for agent in self.agents.values_mut() {
            if socket_closed {
                agent.state = AgentState::Disconnected;
                agent.activity = "Disconnected".to_owned();
                agent.attention = AttentionKind::Blocked;
                agent.requires_attention = true;
                agent.activity_kind = ActivityKind::Blocked;
            } else if now.saturating_duration_since(agent.last_seen) >= STALE_AFTER
                && !matches!(agent.state, AgentState::Disconnected)
            {
                agent.state = AgentState::Stale;
                agent.activity = "No recent provider activity".to_owned();
                agent.attention = AttentionKind::None;
                agent.requires_attention = false;
                agent.activity_kind = ActivityKind::Idle;
                any_agent_stale = true;
            }
        }
        if !socket_closed && any_agent_stale {
            self.source_status = "stale".to_owned();
        }
    }
    pub fn source_status(&self) -> &str {
        &self.source_status
    }
    pub fn agents(&self) -> impl Iterator<Item = &AgentSnapshot> {
        self.agents.values()
    }
    pub fn runtime_state(&self, updated_at: &str) -> RuntimeResult<RuntimeState> {
        let source_status = match self.source_status.as_str() {
            "connected" => SourceStatus::Connected,
            "replay" => SourceStatus::Replay,
            "file" => SourceStatus::File,
            "stale" => SourceStatus::Stale,
            _ => SourceStatus::Disconnected,
        };
        let agents: Vec<AgentRecord> = self
            .agents
            .values()
            .map(|agent| AgentRecord {
                provider: agent.provider.clone(),
                session_id: agent.session_id.clone(),
                thread_id: agent.thread_id.clone(),
                turn_id: agent.turn_id.clone(),
                agent_id: agent.agent_id.clone(),
                parent_agent_id: agent.parent_agent_id.clone(),
                task_ids: agent.task_ids.clone(),
                state: agent.state,
                activity: agent.activity.clone(),
                attention: agent.attention,
                requires_attention: agent.requires_attention,
                started_at: agent.started_at.clone(),
                last_seen_at: agent.last_seen_at.clone(),
                sequence: agent.sequence,
                recent_event_ids: agent.recent_event_ids.iter().cloned().collect(),
                activity_kind: agent.activity_kind,
            })
            .collect();
        let attention = agents
            .iter()
            .filter(|agent| agent.requires_attention)
            .map(|agent| AttentionRecord {
                agent_id: agent.agent_id.clone(),
                kind: agent.attention,
                activity: agent.activity.clone(),
                task_ids: agent.task_ids.clone(),
            })
            .collect();
        RuntimeState {
            schema_version: SCHEMA_VERSION.to_owned(),
            updated_at: updated_at.to_owned(),
            source_status,
            capabilities: ProviderCapabilities {
                approve: false,
                reject: false,
                focus: false,
            },
            attention,
            agents,
        }
        .validate()
    }
}
pub fn replay_jsonl(input: &[u8]) -> RuntimeResult<RuntimeReducer> {
    replay_jsonl_with_allowlist(input, &HashSet::new())
}
pub fn replay_jsonl_with_allowlist(
    input: &[u8],
    allowed_tasks: &HashSet<String>,
) -> RuntimeResult<RuntimeReducer> {
    if input.len() > MAX_REPLAY_BYTES {
        return Err(RuntimeError::new(
            ErrorCode::ReplayByteLimit,
            "replay 內容超過 8 MiB",
        ));
    }
    let mut reducer = RuntimeReducer::new();
    let mut events = 0usize;
    for line in input.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_EVENT_BYTES {
            return Err(RuntimeError::new(
                ErrorCode::EventTooLarge,
                "replay 單行超過 1 MiB",
            ));
        }
        events += 1;
        if events > MAX_REPLAY_EVENTS {
            return Err(RuntimeError::new(
                ErrorCode::ReplayEventLimit,
                "replay 事件超過 10,000",
            ));
        }
        let event = EventEnvelope::from_json_bytes(line)?;
        let _ = reducer.apply_with_allowlist(event, allowed_tasks)?;
    }
    reducer.source_status = "replay".to_owned();
    Ok(reducer)
}

/// stdio JSONL transport；不經 shell，並對每個 frame 施加 1 MiB 上限。
pub struct StdioTransport<R, W> {
    reader: R,
    writer: W,
}
impl<R: BufRead, W: Write> StdioTransport<R, W> {
    pub fn new(reader: R, writer: W) -> Self {
        Self { reader, writer }
    }
    pub fn recv(&mut self) -> RuntimeResult<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            let chunk = self.reader.fill_buf()?;
            if chunk.is_empty() {
                if line.is_empty() {
                    return Err(RuntimeError::new(ErrorCode::Io, "stdio 已斷線"));
                }
                break;
            }
            let take = chunk
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(chunk.len(), |position| position + 1);
            if line.len().saturating_add(take) > MAX_EVENT_BYTES {
                return Err(RuntimeError::new(
                    ErrorCode::EventTooLarge,
                    "stdio frame 超過 1 MiB",
                ));
            }
            line.extend_from_slice(&chunk[..take]);
            self.reader.consume(take);
            if line.last() == Some(&b'\n') {
                break;
            }
        }
        std::str::from_utf8(&line)
            .map_err(|_| RuntimeError::new(ErrorCode::InvalidUtf8, "stdio frame 不是 UTF-8"))?;
        while matches!(line.last(), Some(b'\n' | b'\r')) {
            line.pop();
        }
        Ok(line)
    }
    pub fn send_json(&mut self, value: &Value) -> RuntimeResult<()> {
        if !value.is_object() {
            return Err(RuntimeError::new(
                ErrorCode::Schema,
                "stdio frame 必須是 JSON object",
            ));
        }
        check_bounds(value)?;
        if !scan_privacy(value).is_empty() {
            return Err(RuntimeError::new(
                ErrorCode::PrivacyViolation,
                "stdio frame 含禁止內容",
            ));
        }
        let bytes = serde_json::to_vec(value)
            .map_err(|e| RuntimeError::new(ErrorCode::InvalidJson, e.to_string()))?;
        if bytes.len().saturating_add(1) > MAX_EVENT_BYTES {
            return Err(RuntimeError::new(
                ErrorCode::EventTooLarge,
                "stdio frame 超過 1 MiB",
            ));
        }
        self.writer.write_all(&bytes)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }
    pub fn recv_event(&mut self) -> RuntimeResult<EventEnvelope> {
        let frame = self.recv()?;
        EventEnvelope::from_json_bytes(&frame)
    }
}
pub const WEBSOCKET_UNSUPPORTED: &str = "websocket transport unsupported offline；請使用 stdio";
pub fn stdio_command(executable: &str) -> RuntimeResult<Vec<String>> {
    if executable.is_empty() || executable.len() > MAX_FIELD_BYTES {
        return Err(RuntimeError::new(
            ErrorCode::Schema,
            "stdio executable 路徑無效",
        ));
    }
    Ok(vec![
        executable.to_owned(),
        "app-server".to_owned(),
        "--listen".to_owned(),
        "stdio://".to_owned(),
    ])
}
pub fn websocket_transport() -> RuntimeResult<()> {
    Err(RuntimeError::new(
        ErrorCode::UnsupportedTransport,
        WEBSOCKET_UNSUPPORTED,
    ))
}

pub fn bind_loopback(host: &str, port: u16) -> RuntimeResult<TcpListener> {
    let address = match host {
        "127.0.0.1" => ("127.0.0.1", port),
        "::1" => ("[::1]", port),
        _ => return Err(RuntimeError::new(ErrorCode::Schema, "HTTP 只允許 loopback")),
    };
    let listener = TcpListener::bind(address).map_err(RuntimeError::from)?;
    let ip = listener.local_addr().map_err(RuntimeError::from)?.ip();
    if !matches!(ip, IpAddr::V4(value) if value.is_loopback())
        && !matches!(ip, IpAddr::V6(value) if value.is_loopback())
    {
        return Err(RuntimeError::new(
            ErrorCode::Schema,
            "HTTP 非 loopback 位址",
        ));
    }
    Ok(listener)
}

/// Pure HTTP routing function used by the tiny loopback companion and tests.
/// It intentionally exposes only read-only health/snapshot endpoints.
pub fn loopback_http_response(
    host: &str,
    method: &str,
    path: &str,
    state: &RuntimeState,
) -> RuntimeResult<Vec<u8>> {
    if !matches!(host, "127.0.0.1" | "::1") {
        return Err(RuntimeError::new(
            ErrorCode::Schema,
            "HTTP Host 必須是 loopback",
        ));
    }
    if !matches!(method, "GET" | "HEAD") {
        return Err(RuntimeError::new(
            ErrorCode::UnsupportedTransport,
            "HTTP 僅允許 GET/HEAD",
        ));
    }
    state.validate()?;
    let body = match path {
        "/_mission-center/health" | "/health" => {
            serde_json::json!({"status":"ok","sourceStatus":state.source_status})
        }
        "/_mission-center/snapshot" | "/snapshot" => serde_json::to_value(state)
            .map_err(|_| RuntimeError::new(ErrorCode::Schema, "snapshot serialization failed"))?,
        _ => {
            return Err(RuntimeError::new(
                ErrorCode::Schema,
                "HTTP path 不在 allowlist",
            ));
        }
    };
    let payload = serde_json::to_vec(&body)
        .map_err(|_| RuntimeError::new(ErrorCode::Schema, "HTTP serialization failed"))?;
    let mut response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", payload.len()).into_bytes();
    if method == "GET" {
        response.extend_from_slice(&payload);
    }
    Ok(response)
}

pub fn serve_http_once(mut stream: TcpStream, state: &RuntimeState) -> RuntimeResult<()> {
    let mut request = [0_u8; 8192];
    let count = stream.read(&mut request).map_err(RuntimeError::from)?;
    let text = std::str::from_utf8(&request[..count])
        .map_err(|_| RuntimeError::new(ErrorCode::InvalidUtf8, "HTTP request 不是 UTF-8"))?;
    let mut lines = text.lines();
    let mut parts = lines.next().unwrap_or_default().split_whitespace();
    let method = parts.next().unwrap_or_default();
    let path = parts.next().unwrap_or_default();
    let host = lines
        .find_map(|line| {
            line.strip_prefix("Host:")
                .or_else(|| line.strip_prefix("host:"))
                .map(str::trim)
        })
        .unwrap_or_default();
    let response = match loopback_http_response(host_only(host), method, path, state) {
        Ok(response) => response,
        Err(_error) => "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .as_bytes()
            .to_vec(),
    };
    stream.write_all(&response).map_err(RuntimeError::from)
}

fn host_only(value: &str) -> &str {
    if let Some(rest) = value.strip_prefix('[') {
        return rest.split(']').next().unwrap_or_default();
    }
    if let Some((host, port)) = value.rsplit_once(':')
        && port.bytes().all(|byte| byte.is_ascii_digit())
    {
        return host;
    }
    value
}

// ---------------------------------------------------------------------------
// HUD launcher/server lifecycle
// ---------------------------------------------------------------------------

/// Managed files which the Rust companion may serve.  This list intentionally
/// mirrors the Python oracle; arbitrary files are never exposed.
pub fn hud_asset_names() -> &'static [&'static str] {
    HUD_ASSETS
}

/// Verify an explicitly requested loopback bind target before opening it.
/// `0` asks the OS for an ephemeral port; no non-loopback address is accepted.
pub fn validate_loopback_host(host: &str) -> RuntimeResult<()> {
    if matches!(host, "127.0.0.1" | "::1") {
        Ok(())
    } else {
        Err(RuntimeError::new(
            ErrorCode::InvalidHost,
            "HTTP 只允許 127.0.0.1 或 ::1",
        ))
    }
}

/// Stable, non-secret workspace identity.  Only the canonical path is used;
/// task contents and prompt text are never written by this module.
pub fn workspace_identity(workspace: &Path) -> RuntimeResult<String> {
    let canonical = workspace
        .canonicalize()
        .map_err(|_| RuntimeError::new(ErrorCode::AssetUnavailable, "workspace 不可解析"))?;
    let identity = canonical.to_string_lossy().to_ascii_lowercase();
    Ok(mission_center_core::sha256_digest(identity.as_bytes()))
}

pub fn fingerprint_workspace(workspace: &Path) -> RuntimeResult<String> {
    workspace_identity(workspace)
}

#[cfg(not(windows))]
fn ensure_safe_root(root: &Path) -> RuntimeResult<PathBuf> {
    let canonical = root
        .canonicalize()
        .map_err(|_| RuntimeError::new(ErrorCode::AssetUnavailable, "HUD output 不可解析"))?;
    let metadata = std::fs::symlink_metadata(root)
        .map_err(|_| RuntimeError::new(ErrorCode::AssetUnavailable, "HUD output 不可讀取"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() || is_reparse(&metadata) {
        return Err(RuntimeError::new(
            ErrorCode::UnsafePath,
            "HUD output 不可為 symlink/reparse path",
        ));
    }
    Ok(canonical)
}

#[cfg(not(windows))]
fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    let _ = metadata;
    false
}

/// Open and read one bounded regular file. The returned bytes come from the
/// already-open handle, so a later path replacement cannot redirect the read.
/// This filesystem loader is Unix-only. Windows deliberately rejects path
/// loading and must use `FrozenHudAssets`; a future native handle-relative
/// adapter can provide dynamic Windows assets without weakening this boundary.
#[cfg(not(windows))]
fn read_safe_regular_file(root: &Path, candidate: &Path) -> RuntimeResult<Vec<u8>> {
    let canonical_root = ensure_safe_root(root)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(any(target_os = "linux", target_os = "android"))]
    options.custom_flags(0x20000); // O_NOFOLLOW on Linux/Android.
    #[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
    options.custom_flags(0x100); // O_NOFOLLOW on Darwin/BSD.
    #[cfg(windows)]
    options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    let file = options.open(candidate).map_err(|_| {
        if std::fs::symlink_metadata(candidate)
            .map(|metadata| metadata.file_type().is_symlink() || is_reparse(&metadata))
            .unwrap_or(false)
        {
            RuntimeError::new(ErrorCode::UnsafePath, "HUD asset 是 symlink/reparse path")
        } else {
            RuntimeError::new(ErrorCode::AssetUnavailable, "HUD asset 不存在或不可開啟")
        }
    })?;
    let metadata = file.metadata().map_err(|_| {
        RuntimeError::new(ErrorCode::AssetUnavailable, "HUD asset metadata 不可讀取")
    })?;
    if !metadata.is_file() || is_reparse(&metadata) {
        return Err(RuntimeError::new(
            ErrorCode::UnsafePath,
            "HUD asset final handle 不可為 symlink/reparse path",
        ));
    }
    let canonical = candidate
        .canonicalize()
        .map_err(|_| RuntimeError::new(ErrorCode::AssetUnavailable, "HUD asset 不可解析"))?;
    if !canonical.starts_with(&canonical_root) {
        return Err(RuntimeError::new(
            ErrorCode::UnsafePath,
            "HUD asset 超出 output containment",
        ));
    }
    // Check every existing ancestor as well. This catches a symlinked/reparse
    // directory even when the final file itself is ordinary.
    let mut current = candidate.parent();
    while let Some(path) = current {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|_| RuntimeError::new(ErrorCode::AssetUnavailable, "HUD parent 不存在"))?;
        if metadata.file_type().is_symlink() || is_reparse(&metadata) {
            return Err(RuntimeError::new(
                ErrorCode::UnsafePath,
                "HUD parent 不可為 symlink/reparse path",
            ));
        }
        if path == root {
            break;
        }
        current = path.parent();
    }
    let size = metadata.len();
    if size > MAX_HTTP_BODY_BYTES as u64 {
        return Err(RuntimeError::new(
            ErrorCode::AssetUnavailable,
            "HUD asset 超過大小上限",
        ));
    }
    let mut bytes = Vec::with_capacity(size as usize);
    file.take(MAX_HTTP_BODY_BYTES as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| RuntimeError::new(ErrorCode::AssetUnavailable, "HUD asset 不可讀取"))?;
    if bytes.len() as u64 != size {
        return Err(RuntimeError::new(
            ErrorCode::AssetUnavailable,
            "HUD asset 在讀取期間變更",
        ));
    }
    Ok(bytes)
}

#[derive(Clone)]
struct LoadedHudAssets {
    files: HashMap<String, Vec<u8>>,
    runtime_state: Vec<u8>,
    fingerprint: String,
}

/// Cross-platform, already-frozen serving bundle. The constructor performs
/// all validation in memory; no path is accepted and no filesystem operation
/// occurs while constructing or serving this bundle.
#[derive(Clone, Debug)]
pub struct FrozenHudAssets {
    files: HashMap<String, Vec<u8>>,
    runtime_state: Vec<u8>,
    fingerprint: String,
}

impl FrozenHudAssets {
    pub fn new<I, K>(files: I, runtime_state: Vec<u8>) -> RuntimeResult<Self>
    where
        I: IntoIterator<Item = (K, Vec<u8>)>,
        K: Into<String>,
    {
        let mut map = HashMap::new();
        let mut total = 0usize;
        for (key, value) in files {
            let key = key.into();
            if !HUD_ASSETS.contains(&key.as_str())
                || key.split('/').count() != 1
                || key.contains('\\')
                || value.len() > MAX_HTTP_BODY_BYTES
                || map.insert(key, value).is_some()
            {
                return Err(RuntimeError::new(
                    ErrorCode::UnsafePath,
                    "frozen HUD asset 不在 allowlist 或重複",
                ));
            }
        }
        for name in HUD_MANAGED_ASSETS {
            let Some(value) = map.get(*name) else {
                return Err(RuntimeError::new(
                    ErrorCode::AssetUnavailable,
                    "frozen HUD managed asset 缺失",
                ));
            };
            total = total.saturating_add(value.len());
            if total > MAX_HUD_ASSET_TOTAL_BYTES {
                return Err(RuntimeError::new(
                    ErrorCode::AssetUnavailable,
                    "frozen HUD asset 總 bytes 超過上限",
                ));
            }
        }
        if let Some(value) = map.get("visual-state.json") {
            total = total.saturating_add(value.len());
            if total > MAX_HUD_ASSET_TOTAL_BYTES {
                return Err(RuntimeError::new(
                    ErrorCode::AssetUnavailable,
                    "frozen HUD asset 總 bytes 超過上限",
                ));
            }
        }
        if runtime_state.len() > MAX_HTTP_BODY_BYTES {
            return Err(RuntimeError::new(
                ErrorCode::AssetUnavailable,
                "frozen runtime-state 超過大小上限",
            ));
        }
        let state: RuntimeState = serde_json::from_slice(&runtime_state).map_err(|_| {
            RuntimeError::new(ErrorCode::Schema, "frozen runtime-state schema 無效")
        })?;
        state.validate()?;
        let mut fingerprint_input = Vec::new();
        for name in HUD_MANAGED_ASSETS {
            let value = map.get(*name).expect("managed asset checked above");
            fingerprint_input.extend_from_slice(name.as_bytes());
            fingerprint_input.push(0);
            fingerprint_input.extend_from_slice(value);
            fingerprint_input.push(0);
        }
        Ok(Self {
            files: map,
            runtime_state,
            fingerprint: mission_center_core::sha256_digest(&fingerprint_input),
        })
    }

    pub fn from_state<I, K>(files: I, state: RuntimeState) -> RuntimeResult<Self>
    where
        I: IntoIterator<Item = (K, Vec<u8>)>,
        K: Into<String>,
    {
        state.validate()?;
        let bytes = serde_json::to_vec(&state).map_err(|_| {
            RuntimeError::new(ErrorCode::Schema, "runtime state serialization failed")
        })?;
        Self::new(files, bytes)
    }

    fn into_loaded(self) -> LoadedHudAssets {
        LoadedHudAssets {
            files: self.files,
            runtime_state: self.runtime_state,
            fingerprint: self.fingerprint,
        }
    }
}

/// Load the complete serving snapshot before the listener starts. Once this
/// returns, request handling has no filesystem dependency at all.
#[cfg(not(windows))]
fn load_hud_assets(output: &Path, fallback_state: &RuntimeState) -> RuntimeResult<LoadedHudAssets> {
    let output = ensure_safe_root(output)?;
    let assets_root = output.join(HUD_ASSET_DIR);
    let assets_root = ensure_safe_root(&assets_root)?;
    let mut files = HashMap::new();
    let mut fingerprint_input = Vec::new();
    let mut total = 0usize;
    for name in HUD_MANAGED_ASSETS {
        let content = read_safe_regular_file(&assets_root, &assets_root.join(name))?;
        total = total.saturating_add(content.len());
        if total > MAX_HUD_ASSET_TOTAL_BYTES {
            return Err(RuntimeError::new(
                ErrorCode::AssetUnavailable,
                "HUD asset 總 bytes 超過上限",
            ));
        }
        fingerprint_input.extend_from_slice(name.as_bytes());
        fingerprint_input.push(0);
        fingerprint_input.extend_from_slice(&content);
        fingerprint_input.push(0);
        files.insert((*name).to_owned(), content);
    }
    // visual-state is allowlisted derived state, but is optional like the
    // Python companion. It is still snapshotted if present.
    let optional = assets_root.join("visual-state.json");
    if optional.exists() {
        let content = read_safe_regular_file(&assets_root, &optional)?;
        total = total.saturating_add(content.len());
        if total > MAX_HUD_ASSET_TOTAL_BYTES {
            return Err(RuntimeError::new(
                ErrorCode::AssetUnavailable,
                "HUD asset 總 bytes 超過上限",
            ));
        }
        files.insert("visual-state.json".to_owned(), content);
    }
    if files.len() > MAX_HUD_ASSET_COUNT {
        return Err(RuntimeError::new(
            ErrorCode::AssetUnavailable,
            "HUD asset 檔案數超過上限",
        ));
    }
    let runtime_dir = output.join("mission-center-runtime");
    let runtime_path = runtime_dir.join(RUNTIME_STATE_FILE);
    let runtime_state = if runtime_dir.exists() {
        let runtime_dir = ensure_safe_root(&runtime_dir)?;
        if runtime_path.exists() {
            let bytes = read_safe_regular_file(&runtime_dir, &runtime_path)?;
            let state: RuntimeState = serde_json::from_slice(&bytes)
                .map_err(|_| RuntimeError::new(ErrorCode::Schema, "runtime-state schema 無效"))?;
            state.validate()?;
            bytes
        } else {
            serde_json::to_vec(fallback_state).map_err(|_| {
                RuntimeError::new(ErrorCode::Schema, "runtime state serialization failed")
            })?
        }
    } else {
        serde_json::to_vec(fallback_state).map_err(|_| {
            RuntimeError::new(ErrorCode::Schema, "runtime state serialization failed")
        })?
    };
    if runtime_state.len() > MAX_HTTP_BODY_BYTES {
        return Err(RuntimeError::new(
            ErrorCode::AssetUnavailable,
            "runtime-state 超過大小上限",
        ));
    }
    Ok(LoadedHudAssets {
        files,
        runtime_state,
        fingerprint: mission_center_core::sha256_digest(&fingerprint_input),
    })
}

/// Fingerprint all managed HUD assets below an `output` directory on Unix.
/// Windows intentionally rejects path-based loading; callers must construct a
/// [`FrozenHudAssets`] bundle from compile-time or native handle-relative code.
pub fn fingerprint_hud_assets(output: &Path) -> RuntimeResult<String> {
    #[cfg(windows)]
    {
        let _ = output;
        Err(RuntimeError::new(
            ErrorCode::UnsafePath,
            "Windows filesystem HUD loader disabled；請提供 frozen bundle",
        ))
    }
    #[cfg(not(windows))]
    {
        Ok(load_hud_assets(output, &disconnected_runtime_state())?.fingerprint)
    }
}

fn generated_nonce() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    mission_center_core::sha256_digest(format!("{now}:{count}").as_bytes())
}

fn disconnected_runtime_state() -> RuntimeState {
    RuntimeState {
        schema_version: SCHEMA_VERSION.to_owned(),
        updated_at: "1970-01-01T00:00:00Z".to_owned(),
        source_status: SourceStatus::Disconnected,
        capabilities: ProviderCapabilities {
            approve: false,
            reject: false,
            focus: false,
        },
        attention: Vec::new(),
        agents: Vec::new(),
    }
}

#[derive(Debug, Clone)]
pub struct HudServerConfig {
    pub workspace: PathBuf,
    /// Output directory containing `mission-center-assets` and optionally
    /// `mission-center-runtime/runtime-state.json`.
    pub output: PathBuf,
    pub port: u16,
    pub version: String,
    pub session_nonce: Option<String>,
    pub state: RuntimeState,
    pub cooldown: Duration,
    pub frozen_assets: Option<FrozenHudAssets>,
}

impl HudServerConfig {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        Self {
            output: workspace.join("output"),
            workspace,
            port: 0,
            version: HUD_RUNTIME_VERSION.to_owned(),
            session_nonce: None,
            state: disconnected_runtime_state(),
            cooldown: LAUNCH_COOLDOWN,
            frozen_assets: None,
        }
    }
    pub fn with_output(mut self, output: impl Into<PathBuf>) -> Self {
        self.output = output.into();
        self
    }
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }
    pub fn with_nonce(mut self, nonce: impl Into<String>) -> Self {
        self.session_nonce = Some(nonce.into());
        self
    }
    pub fn with_state(mut self, state: RuntimeState) -> Self {
        self.state = state;
        self
    }
    pub fn with_frozen_assets(mut self, assets: FrozenHudAssets) -> Self {
        self.frozen_assets = Some(assets);
        self
    }
}

pub trait BrowserOpener: Send + Sync {
    /// Return false when no browser/sidebar is available.  A failed opener is
    /// advisory: it must never stop or invalidate the loopback server.
    fn open(&self, url: &str) -> bool;
}

#[derive(Debug, Default)]
pub struct NoopBrowserOpener;
impl BrowserOpener for NoopBrowserOpener {
    fn open(&self, _url: &str) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchStatus {
    Launched,
    Reused,
    Cooldown,
}

#[derive(Clone)]
pub struct HudLaunchOutcome {
    pub status: LaunchStatus,
    pub url: String,
    pub workspace_fingerprint: String,
    pub hud_asset_fingerprint: String,
    pub version: String,
    pub browser_opened: bool,
    pub browser_status: &'static str,
    pub server: HudServer,
}

impl fmt::Debug for HudLaunchOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HudLaunchOutcome")
            .field("status", &self.status)
            .field("url", &self.url)
            .field("workspace_fingerprint", &self.workspace_fingerprint)
            .field("hud_asset_fingerprint", &self.hud_asset_fingerprint)
            .field("version", &self.version)
            .field("browser_opened", &self.browser_opened)
            .field("browser_status", &self.browser_status)
            .finish()
    }
}

struct ServerInner {
    stop: AtomicBool,
    running: AtomicBool,
    requests_served: std::sync::atomic::AtomicUsize,
    address: SocketAddr,
    workspace_fingerprint: String,
    hud_asset_fingerprint: String,
    session_nonce: String,
    version: String,
    assets: Arc<HashMap<String, Vec<u8>>>,
    runtime_state: Arc<Vec<u8>>,
    join: Mutex<Option<JoinHandle<()>>>,
    last_launch: Mutex<Instant>,
    cooldown: Duration,
}

#[derive(Clone)]
pub struct HudServer(Arc<ServerInner>);
pub type RuntimeHudServer = HudServer;
pub type RuntimeHudLauncher = HudLauncher;

impl fmt::Debug for HudServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HudServer")
            .field("address", &self.0.address)
            .field("version", &self.0.version)
            .field("running", &self.is_running())
            .finish()
    }
}

impl HudServer {
    pub fn address(&self) -> SocketAddr {
        self.0.address
    }
    pub fn port(&self) -> u16 {
        self.0.address.port()
    }
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port())
    }
    pub fn workspace_fingerprint(&self) -> &str {
        &self.0.workspace_fingerprint
    }
    pub fn hud_asset_fingerprint(&self) -> &str {
        &self.0.hud_asset_fingerprint
    }
    pub fn session_nonce(&self) -> &str {
        &self.0.session_nonce
    }
    pub fn version(&self) -> &str {
        &self.0.version
    }
    pub fn is_running(&self) -> bool {
        self.0.running.load(Ordering::Acquire) && !self.0.stop.load(Ordering::Acquire)
    }
    /// Wait until the server has handled one connection or the bounded
    /// timeout elapses. This lets a foreground CLI expose its URL before
    /// serving while still guaranteeing a cleanup path for tests/callers.
    pub fn wait_for_request(&self, timeout: Duration) -> bool {
        let baseline = self.0.requests_served.load(Ordering::Acquire);
        let deadline = Instant::now() + timeout;
        loop {
            if self.0.requests_served.load(Ordering::Acquire) > baseline {
                return true;
            }
            if !self.is_running() || Instant::now() >= deadline {
                return self.0.requests_served.load(Ordering::Acquire) > baseline;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
    pub fn health_check(&self) -> RuntimeResult<()> {
        probe_health(
            self.0.address,
            self.workspace_fingerprint(),
            self.session_nonce(),
            self.hud_asset_fingerprint(),
            self.version(),
        )
    }
    pub fn shutdown(&self) -> RuntimeResult<()> {
        if !self.0.running.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        self.0.stop.store(true, Ordering::Release);
        // Wake a nonblocking accept loop immediately.  Ignore connection
        // errors because the loop may already have observed the stop flag.
        if let Ok(stream) = TcpStream::connect_timeout(&self.0.address, HEALTH_TIMEOUT) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let join = self
            .0
            .join
            .lock()
            .map_err(|_| RuntimeError::new(ErrorCode::Shutdown, "server join lock 已 poisoned"))?
            .take();
        if let Some(join) = join {
            join.join().map_err(|_| {
                RuntimeError::new(ErrorCode::Shutdown, "server thread 無法正常結束")
            })?;
        }
        Ok(())
    }
}

fn health_payload_for(server: &ServerInner) -> Value {
    health_payload(
        &server.workspace_fingerprint,
        &server.session_nonce,
        &server.hud_asset_fingerprint,
        &server.version,
    )
}

pub fn health_payload(
    workspace_fingerprint: &str,
    session_nonce: &str,
    asset_fingerprint: &str,
    version: &str,
) -> Value {
    serde_json::json!({
        "service": "mission-center-hud",
        "status": "ok",
        "version": version,
        "workspaceFingerprint": workspace_fingerprint,
        "sessionNonce": session_nonce,
        "hudAssetFingerprint": asset_fingerprint,
    })
}

fn verify_health_payload(
    value: &Value,
    workspace_fingerprint: &str,
    nonce: &str,
    asset_fingerprint: &str,
    version: &str,
) -> RuntimeResult<()> {
    let matches = value.get("service") == Some(&Value::String("mission-center-hud".to_owned()))
        && value.get("status") == Some(&Value::String("ok".to_owned()))
        && value.get("version") == Some(&Value::String(version.to_owned()))
        && value.get("workspaceFingerprint")
            == Some(&Value::String(workspace_fingerprint.to_owned()))
        && value.get("sessionNonce") == Some(&Value::String(nonce.to_owned()))
        && value.get("hudAssetFingerprint") == Some(&Value::String(asset_fingerprint.to_owned()));
    if matches {
        Ok(())
    } else {
        Err(RuntimeError::new(
            ErrorCode::HealthMismatch,
            "health nonce/workspace/version 不符",
        ))
    }
}

fn probe_health(
    address: SocketAddr,
    workspace_fingerprint: &str,
    nonce: &str,
    asset_fingerprint: &str,
    version: &str,
) -> RuntimeResult<()> {
    let mut stream = TcpStream::connect_timeout(&address, HEALTH_TIMEOUT)
        .map_err(|_| RuntimeError::new(ErrorCode::HealthMismatch, "health server 不可連線"))?;
    stream
        .set_read_timeout(Some(HEALTH_TIMEOUT))
        .map_err(RuntimeError::from)?;
    stream
        .set_write_timeout(Some(HEALTH_TIMEOUT))
        .map_err(RuntimeError::from)?;
    let request = format!(
        "GET /_mission-center/health HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        address.port()
    );
    stream
        .write_all(request.as_bytes())
        .map_err(RuntimeError::from)?;
    let mut bytes = Vec::new();
    stream
        .take((MAX_HTTP_BODY_BYTES.min(64 * 1024)) as u64)
        .read_to_end(&mut bytes)
        .map_err(RuntimeError::from)?;
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| RuntimeError::new(ErrorCode::HealthMismatch, "health response 格式無效"))?;
    if !bytes.starts_with(b"HTTP/1.1 200") {
        return Err(RuntimeError::new(
            ErrorCode::HealthMismatch,
            "health response 非 200",
        ));
    }
    let value: Value = serde_json::from_slice(&bytes[split + 4..])
        .map_err(|_| RuntimeError::new(ErrorCode::HealthMismatch, "health payload 無效"))?;
    verify_health_payload(
        &value,
        workspace_fingerprint,
        nonce,
        asset_fingerprint,
        version,
    )
}

/// Probe a launcher-owned loopback HUD without creating a server.  This is
/// intentionally public so the CLI hook can verify an existing Rust child
/// process before reusing its metadata; callers must provide the nonce and
/// fingerprints from the same bounded receipt.
pub fn probe_loopback_health(
    port: u16,
    workspace_fingerprint: &str,
    session_nonce: &str,
    asset_fingerprint: &str,
    version: &str,
) -> RuntimeResult<()> {
    if port == 0 {
        return Err(RuntimeError::new(
            ErrorCode::HealthMismatch,
            "HUD port 不可為零",
        ));
    }
    probe_health(
        SocketAddr::from(([127, 0, 0, 1], port)),
        workspace_fingerprint,
        session_nonce,
        asset_fingerprint,
        version,
    )
}

fn run_server(listener: TcpListener, server: Arc<ServerInner>) {
    let _ = listener.set_nonblocking(true);
    while !server.stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if serve_hud_http_once(stream, &server).is_ok_and(|outcome| outcome.accepted) {
                    server.requests_served.fetch_add(1, Ordering::AcqRel);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
    server.running.store(false, Ordering::Release);
}

/// Outcome of one parsed HUD request. A request only counts toward
/// `wait_for_request` when it is an allowed loopback GET/HEAD and returns a
/// successful (2xx) allowlisted resource response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HudRequestOutcome {
    pub accepted: bool,
    pub status_code: u16,
}

fn start_server(
    config: &HudServerConfig,
    workspace_fingerprint: String,
    loaded: LoadedHudAssets,
) -> RuntimeResult<HudServer> {
    if config.version.is_empty() || config.version.len() > MAX_FIELD_BYTES {
        return Err(RuntimeError::new(ErrorCode::Schema, "runtime version 無效"));
    }
    if config
        .session_nonce
        .as_ref()
        .is_some_and(|nonce| nonce.is_empty() || nonce.len() > 128)
    {
        return Err(RuntimeError::new(ErrorCode::Schema, "session nonce 無效"));
    }
    config.state.validate()?;
    let listener = bind_loopback("127.0.0.1", config.port)
        .map_err(|_| RuntimeError::new(ErrorCode::PortBind, "loopback port bind 失敗"))?;
    let address = listener.local_addr().map_err(RuntimeError::from)?;
    let inner = Arc::new(ServerInner {
        stop: AtomicBool::new(false),
        running: AtomicBool::new(true),
        requests_served: std::sync::atomic::AtomicUsize::new(0),
        address,
        workspace_fingerprint,
        hud_asset_fingerprint: loaded.fingerprint,
        session_nonce: config.session_nonce.clone().unwrap_or_else(generated_nonce),
        version: config.version.clone(),
        assets: Arc::new(loaded.files),
        runtime_state: Arc::new(loaded.runtime_state),
        join: Mutex::new(None),
        last_launch: Mutex::new(Instant::now()),
        cooldown: config.cooldown,
    });
    let thread_server = Arc::clone(&inner);
    let join = thread::Builder::new()
        .name("mission-center-hud".to_owned())
        .spawn(move || run_server(listener, thread_server))
        .map_err(|_| RuntimeError::new(ErrorCode::Io, "HUD server thread 無法啟動"))?;
    inner
        .join
        .lock()
        .map_err(|_| RuntimeError::new(ErrorCode::Io, "server join lock 已 poisoned"))?
        .replace(join);
    let server = HudServer(inner);
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    loop {
        match server.health_check() {
            Ok(()) => return Ok(server),
            Err(error) if Instant::now() < deadline && server.is_running() => {
                let _ = error;
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                let _ = server.shutdown();
                return Err(error);
            }
        }
    }
}

fn assets_for_config(config: &HudServerConfig) -> RuntimeResult<LoadedHudAssets> {
    if let Some(bundle) = &config.frozen_assets {
        return Ok(bundle.clone().into_loaded());
    }
    #[cfg(windows)]
    {
        Err(RuntimeError::new(
            ErrorCode::UnsafePath,
            "Windows filesystem HUD loader disabled；請提供 frozen bundle",
        ))
    }
    #[cfg(not(windows))]
    {
        load_hud_assets(&config.output, &config.state)
    }
}

pub fn start_hud_server(config: HudServerConfig) -> RuntimeResult<HudServer> {
    let workspace_fingerprint = workspace_identity(&config.workspace)?;
    let loaded = assets_for_config(&config)?;
    start_server(&config, workspace_fingerprint, loaded)
}

pub struct HudLauncher {
    servers: Mutex<HashMap<String, HudServer>>,
    opener: Arc<dyn BrowserOpener>,
}

impl Default for HudLauncher {
    fn default() -> Self {
        Self {
            servers: Mutex::new(HashMap::new()),
            opener: Arc::new(NoopBrowserOpener),
        }
    }
}

impl HudLauncher {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_opener(opener: Arc<dyn BrowserOpener>) -> Self {
        Self {
            servers: Mutex::new(HashMap::new()),
            opener,
        }
    }
    pub fn launch(
        &self,
        config: HudServerConfig,
        open_ui: bool,
    ) -> RuntimeResult<HudLaunchOutcome> {
        let workspace_fingerprint = workspace_identity(&config.workspace)?;
        let loaded = assets_for_config(&config)?;
        let asset_fingerprint = loaded.fingerprint.clone();
        let mut servers = self
            .servers
            .lock()
            .map_err(|_| RuntimeError::new(ErrorCode::Io, "launcher lock 已 poisoned"))?;
        if let Some(existing) = servers.get(&workspace_fingerprint).cloned() {
            if existing.version() != config.version {
                return Err(RuntimeError::new(
                    ErrorCode::VersionMismatch,
                    "既有 server 版本不符，拒絕 reuse",
                ));
            }
            if let Some(expected_nonce) = config.session_nonce.as_deref()
                && expected_nonce != existing.session_nonce()
            {
                return Err(RuntimeError::new(
                    ErrorCode::ReuseRejected,
                    "既有 server nonce 不符，拒絕 reuse",
                ));
            }
            if existing.is_running()
                && existing.health_check().is_ok()
                && existing.hud_asset_fingerprint() == asset_fingerprint
            {
                let mut last = existing.0.last_launch.lock().map_err(|_| {
                    RuntimeError::new(ErrorCode::Io, "server launch lock 已 poisoned")
                })?;
                let within_cooldown = last.elapsed() < existing.0.cooldown;
                if !within_cooldown {
                    *last = Instant::now();
                }
                drop(last);
                let outcome = HudLaunchOutcome {
                    status: if within_cooldown {
                        LaunchStatus::Cooldown
                    } else {
                        LaunchStatus::Reused
                    },
                    url: existing.url(),
                    workspace_fingerprint,
                    hud_asset_fingerprint: asset_fingerprint,
                    version: config.version,
                    browser_opened: false,
                    browser_status: "not_requested",
                    server: existing,
                };
                drop(servers);
                return Ok(self.maybe_open(outcome, open_ui));
            }
            // A stale/dead entry cannot be used as proof of identity. Remove
            // only our own in-memory entry, and always stop its listener before
            // binding a fresh loopback socket. This avoids an orphaned thread
            // retaining the old port when an asset has changed.
            if let Some(stale) = servers.remove(&workspace_fingerprint) {
                stale.shutdown()?;
            }
        }
        let server = start_server(&config, workspace_fingerprint.clone(), loaded)?;
        servers.insert(workspace_fingerprint.clone(), server.clone());
        let outcome = HudLaunchOutcome {
            status: LaunchStatus::Launched,
            url: server.url(),
            workspace_fingerprint,
            hud_asset_fingerprint: asset_fingerprint,
            version: config.version,
            browser_opened: false,
            browser_status: "not_requested",
            server,
        };
        drop(servers);
        Ok(self.maybe_open(outcome, open_ui))
    }
    pub fn launch_or_reuse(
        &self,
        config: HudServerConfig,
        open_ui: bool,
    ) -> RuntimeResult<HudLaunchOutcome> {
        self.launch(config, open_ui)
    }
    fn maybe_open(&self, mut outcome: HudLaunchOutcome, open_ui: bool) -> HudLaunchOutcome {
        if !open_ui {
            return outcome;
        }
        if self.opener.open(&outcome.url) {
            outcome.browser_opened = true;
            outcome.browser_status = "opened";
        } else {
            outcome.browser_status = "unavailable_server_kept";
        }
        outcome
    }
    pub fn shutdown_all(&self) -> RuntimeResult<()> {
        let mut servers = self
            .servers
            .lock()
            .map_err(|_| RuntimeError::new(ErrorCode::Shutdown, "launcher lock 已 poisoned"))?;
        let values: Vec<_> = servers.drain().map(|(_, server)| server).collect();
        drop(servers);
        for server in values {
            server.shutdown()?;
        }
        Ok(())
    }
}

/// Bounded hook envelope. Prompt content is intentionally omitted, so a
/// launcher cannot retain or persist prompt/reasoning/secret text.
#[derive(Debug, Clone, Deserialize)]
pub struct HookInput {
    #[serde(rename = "hook_event_name")]
    pub hook_event_name: String,
    pub cwd: Option<String>,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub permission_mode: Option<String>,
}

pub fn parse_bounded_hook_input(bytes: &[u8]) -> RuntimeResult<Option<HookInput>> {
    if bytes.len() > MAX_HOOK_INPUT_BYTES {
        return Err(RuntimeError::new(
            ErrorCode::HookInputTooLarge,
            "hook 輸入超過 64 KiB",
        ));
    }
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| RuntimeError::new(ErrorCode::InvalidUtf8, "hook 輸入不是 UTF-8"))?;
    let input: HookInput = serde_json::from_str(text)
        .map_err(|_| RuntimeError::new(ErrorCode::InvalidJson, "hook JSON 格式無效"))?;
    if input.hook_event_name.len() > MAX_FIELD_BYTES
        || input
            .cwd
            .as_ref()
            .is_some_and(|s| s.len() > MAX_FIELD_BYTES)
        || input
            .session_id
            .as_ref()
            .is_some_and(|s| s.len() > MAX_FIELD_BYTES)
        || input
            .turn_id
            .as_ref()
            .is_some_and(|s| s.len() > MAX_FIELD_BYTES)
        || input
            .permission_mode
            .as_ref()
            .is_some_and(|s| s.len() > MAX_FIELD_BYTES)
    {
        return Err(RuntimeError::new(
            ErrorCode::FieldTooLong,
            "hook 欄位超過 512 bytes",
        ));
    }
    Ok(Some(input))
}

pub fn validate_health_payload(
    payload: &Value,
    workspace_fingerprint: &str,
    nonce: &str,
    asset_fingerprint: &str,
    version: &str,
) -> RuntimeResult<()> {
    verify_health_payload(
        payload,
        workspace_fingerprint,
        nonce,
        asset_fingerprint,
        version,
    )
}

fn percent_decode_path(raw: &str) -> RuntimeResult<String> {
    let mut result = Vec::with_capacity(raw.len());
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(RuntimeError::new(
                    ErrorCode::PathTraversal,
                    "HTTP percent encoding 無效",
                ));
            }
            let hi = (bytes[index + 1] as char).to_digit(16);
            let lo = (bytes[index + 2] as char).to_digit(16);
            let (Some(hi), Some(lo)) = (hi, lo) else {
                return Err(RuntimeError::new(
                    ErrorCode::PathTraversal,
                    "HTTP percent encoding 無效",
                ));
            };
            result.push((hi * 16 + lo) as u8);
            index += 3;
        } else {
            result.push(bytes[index]);
            index += 1;
        }
    }
    let path = String::from_utf8(result)
        .map_err(|_| RuntimeError::new(ErrorCode::PathTraversal, "HTTP path 不是 UTF-8"))?;
    if path.is_empty()
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('\0')
        || path.contains('\\')
        || path.split('/').any(|part| part == "..")
    {
        return Err(RuntimeError::new(
            ErrorCode::PathTraversal,
            "HTTP path traversal 被拒絕",
        ));
    }
    Ok(path)
}

fn http_bytes(
    status: &str,
    content_type: &str,
    body: &[u8],
    head: bool,
    location: Option<&str>,
) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if let Some(location) = location {
        response.push_str(&format!("Location: {location}\r\n"));
    }
    response.push_str("\r\n");
    let mut bytes = response.into_bytes();
    if !head {
        bytes.extend_from_slice(body);
    }
    bytes
}

fn write_http_response(stream: &mut TcpStream, response: &[u8]) -> RuntimeResult<()> {
    stream.write_all(response).map_err(RuntimeError::from)?;
    stream.flush().map_err(RuntimeError::from)?;
    // A graceful write-side close makes the response boundary deterministic
    // on Windows, where dropping a socket immediately after write_all can be
    // observed by the client as WSAECONNRESET instead of EOF.
    stream.shutdown(Shutdown::Write).map_err(RuntimeError::from)
}

fn serve_hud_http_once(
    mut stream: TcpStream,
    server: &ServerInner,
) -> RuntimeResult<HudRequestOutcome> {
    // Accepted sockets may inherit the listener's nonblocking mode on macOS.
    // The per-connection timeouts below require a blocking socket; otherwise a
    // request arriving just after accept can be dropped with WouldBlock.
    stream.set_nonblocking(false).map_err(RuntimeError::from)?;
    stream
        .set_read_timeout(Some(HEALTH_TIMEOUT))
        .map_err(RuntimeError::from)?;
    stream
        .set_write_timeout(Some(HEALTH_TIMEOUT))
        .map_err(RuntimeError::from)?;
    let mut request = Vec::new();
    let mut one = [0_u8; 1024];
    loop {
        let count = stream.read(&mut one).map_err(RuntimeError::from)?;
        if count == 0 {
            break;
        }
        request.extend_from_slice(&one[..count]);
        if request.len() > MAX_HTTP_REQUEST_BYTES {
            let response = http_bytes(
                "413 Payload Too Large",
                "text/plain; charset=utf-8",
                b"",
                false,
                None,
            );
            write_http_response(&mut stream, &response)?;
            return Ok(HudRequestOutcome {
                accepted: false,
                status_code: 413,
            });
        }
        if request.windows(4).any(|part| part == b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&request)
        .map_err(|_| RuntimeError::new(ErrorCode::InvalidUtf8, "HTTP request 不是 UTF-8"))?;
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or_default();
    let mut first_parts = first.split_whitespace();
    let method = first_parts.next().unwrap_or_default();
    let raw_path = first_parts.next().unwrap_or_default();
    let host = lines
        .find_map(|line| {
            line.strip_prefix("Host:")
                .or_else(|| line.strip_prefix("host:"))
                .map(str::trim)
        })
        .unwrap_or_default();
    let host = host_only(host);
    let valid_host = matches!(host, "127.0.0.1" | "::1");
    let head = method == "HEAD";
    let (response, status_code, accepted) = if !valid_host {
        let response = http_bytes(
            "403 Forbidden",
            "text/plain; charset=utf-8",
            b"",
            head,
            None,
        );
        (response, 403, false)
    } else if !matches!(method, "GET" | "HEAD") {
        let response = http_bytes(
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            b"",
            false,
            None,
        );
        (response, 405, false)
    } else {
        match hud_http_response(server, method, raw_path) {
            Ok((status, content_type, body, location)) => {
                let status_code = status
                    .split_once(' ')
                    .and_then(|(code, _)| code.parse::<u16>().ok())
                    .unwrap_or(500);
                (
                    http_bytes(status, content_type, &body, head, location.as_deref()),
                    status_code,
                    (200..300).contains(&status_code),
                )
            }
            Err(error) if error.code == ErrorCode::PathTraversal => (
                http_bytes(
                    "400 Bad Request",
                    "text/plain; charset=utf-8",
                    b"",
                    head,
                    None,
                ),
                400,
                false,
            ),
            Err(_) => (
                http_bytes(
                    "404 Not Found",
                    "text/plain; charset=utf-8",
                    b"",
                    head,
                    None,
                ),
                404,
                false,
            ),
        }
    };
    write_http_response(&mut stream, &response)?;
    Ok(HudRequestOutcome {
        accepted,
        status_code,
    })
}

fn hud_http_response(
    server: &ServerInner,
    _method: &str,
    raw_path: &str,
) -> RuntimeResult<(&'static str, &'static str, Vec<u8>, Option<String>)> {
    let path = percent_decode_path(raw_path.split('?').next().unwrap_or_default())?;
    if path == "/_mission-center/health" || path == "/health" {
        let body = serde_json::to_vec(&health_payload_for(server))
            .map_err(|_| RuntimeError::new(ErrorCode::Schema, "health serialization failed"))?;
        return Ok(("200 OK", "application/json; charset=utf-8", body, None));
    }
    if path == "/" || path == "/index.html" {
        return Ok((
            "302 Found",
            "text/plain; charset=utf-8",
            Vec::new(),
            Some("/mission-center-assets/visual-summary.html".to_owned()),
        ));
    }
    let is_runtime_state = matches!(
        path.as_str(),
        "/mission-center-runtime/runtime-state.json" | "/snapshot"
    );
    if is_runtime_state {
        return Ok((
            "200 OK",
            "application/json; charset=utf-8",
            (*server.runtime_state).clone(),
            None,
        ));
    }
    let (name, content_type) = if let Some(name) = path.strip_prefix("/mission-center-assets/") {
        if name.is_empty() || name.contains('/') || !HUD_ASSETS.contains(&name) {
            return Err(RuntimeError::new(
                ErrorCode::AssetUnavailable,
                "HUD path 不在 allowlist",
            ));
        }
        let content_type = if name.ends_with(".html") {
            "text/html; charset=utf-8"
        } else if name.ends_with(".json") {
            "application/json; charset=utf-8"
        } else {
            "image/webp"
        };
        (name, content_type)
    } else {
        return Err(RuntimeError::new(
            ErrorCode::AssetUnavailable,
            "HTTP path 不在 allowlist",
        ));
    };
    let body = server
        .assets
        .get(name)
        .cloned()
        .ok_or_else(|| RuntimeError::new(ErrorCode::AssetUnavailable, "HUD asset 不存在"))?;
    Ok(("200 OK", content_type, body, None))
}

/// Parse and serve one HUD request against a running server; useful for
/// deterministic tests without starting a browser or an external process.
pub fn serve_hud_request(
    stream: TcpStream,
    server: &HudServer,
) -> RuntimeResult<HudRequestOutcome> {
    serve_hud_http_once(stream, &server.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_and_event_parser_reject_duplicate_json_keys_as_validation() {
        let duplicate = br#"{"schemaVersion":"1.0","eventId":"e","eventId":"other","timestamp":"2026-08-29T00:00:00Z","provider":"codex","sessionId":"s","agentId":"a","eventType":"started","activity":"x","attention":"none","sequence":1,"state":"working","activityKind":"tool_use"}"#;
        assert_eq!(
            EventEnvelope::from_json_bytes(duplicate).unwrap_err().code,
            ErrorCode::ValidationFailed
        );
        assert_eq!(
            replay_jsonl(duplicate).unwrap_err().code,
            ErrorCode::ValidationFailed
        );
    }

    fn event(seq: u64, id: &str) -> EventEnvelope {
        EventEnvelope {
            schema_version: SCHEMA_VERSION.into(),
            event_id: id.into(),
            timestamp: "2026-08-29T00:00:00Z".into(),
            provider: "codex".into(),
            session_id: "s".into(),
            thread_id: Some("t".into()),
            turn_id: None,
            agent_id: "a".into(),
            parent_agent_id: None,
            task_ids: vec![],
            event_type: "test".into(),
            activity: "Working".into(),
            attention: AttentionKind::None,
            sequence: seq,
            state: AgentState::Working,
            activity_kind: ActivityKind::Working,
        }
    }
    #[test]
    fn strict_envelope_and_bounds() {
        let bytes = serde_json::to_vec(&event(1, "e")).unwrap();
        assert_eq!(
            EventEnvelope::from_json_bytes(&bytes).unwrap().event_id,
            "e"
        );
        assert!(EventEnvelope::from_value(json!({"schemaVersion":"1.0","eventId":"e","timestamp":"x","provider":"codex","sessionId":"s","agentId":"a","taskIds":[],"eventType":"x","activity":"x","attention":"none","sequence":1,"state":"working","unknown":1})).is_err());
    }
    #[test]
    fn privacy_rejects_variants() {
        for value in [
            json!({"activity":"Bearer abc"}),
            json!({"activity":"Bearer\tsecret"}),
            json!({"activity":"token = abc"}),
            json!({"activity":"token\t=\tsecret"}),
            json!({"activity":"api key : abc"}),
            json!({"activity":"eyJhbGciOiJIUzI1NiJ9.x.y"}),
            json!({"activity":"-----BEGIN PRIVATE KEY-----"}),
        ] {
            assert!(!scan_privacy(&value).is_empty());
        }
        assert!(scan_privacy(&json!({"activity":"enjoy this work"})).is_empty());
    }
    #[test]
    fn dedupe_and_order_are_noops() {
        let mut reducer = RuntimeReducer::new();
        assert_eq!(
            reducer.apply(event(2, "same")).unwrap(),
            ApplyResult::Applied
        );
        assert_eq!(
            reducer.apply(event(3, "same")).unwrap(),
            ApplyResult::Duplicate
        );
        assert_eq!(
            reducer.apply(event(1, "old")).unwrap(),
            ApplyResult::OutOfOrder
        );
    }
    #[test]
    fn replay_limits_and_stdio() {
        let input = (0..MAX_REPLAY_EVENTS)
            .map(|i| serde_json::to_string(&event(i as u64 + 1, &format!("e{i}"))).unwrap() + "\n")
            .collect::<String>();
        assert_eq!(replay_jsonl(input.as_bytes()).unwrap().agents().count(), 1);
        let mut over = input.clone();
        over.push_str(
            &serde_json::to_string(&event(MAX_REPLAY_EVENTS as u64 + 1, "over-limit")).unwrap(),
        );
        over.push('\n');
        assert_eq!(
            replay_jsonl(over.as_bytes()).unwrap_err().code,
            ErrorCode::ReplayEventLimit
        );
        let mut t = StdioTransport::new(io::Cursor::new(b"{}\n".to_vec()), Vec::<u8>::new());
        assert_eq!(t.recv().unwrap(), b"{}");
        let event_bytes = serde_json::to_vec(&event(1, "stdio-event")).unwrap();
        let mut typed = StdioTransport::new(
            io::Cursor::new([event_bytes, b"\n".to_vec()].concat()),
            Vec::<u8>::new(),
        );
        assert_eq!(typed.recv_event().unwrap().event_id, "stdio-event");
        let mut sender = StdioTransport::new(io::Cursor::new(Vec::new()), Vec::<u8>::new());
        assert_eq!(
            sender
                .send_json(&json!({"activity":"token = abc"}))
                .unwrap_err()
                .code,
            ErrorCode::PrivacyViolation
        );
        let mut exact_frame = StdioTransport::new(
            io::Cursor::new([vec![b'x'; MAX_EVENT_BYTES - 1], vec![b'\n']].concat()),
            Vec::<u8>::new(),
        );
        assert_eq!(exact_frame.recv().unwrap().len(), MAX_EVENT_BYTES - 1);
        let mut plus_frame = StdioTransport::new(
            io::Cursor::new([vec![b'x'; MAX_EVENT_BYTES], vec![b'\n']].concat()),
            Vec::<u8>::new(),
        );
        assert_eq!(
            plus_frame.recv().unwrap_err().code,
            ErrorCode::EventTooLarge
        );
        assert_eq!(
            replay_jsonl(&vec![b'\n'; MAX_REPLAY_BYTES])
                .unwrap()
                .source_status(),
            "replay"
        );
        assert_eq!(
            replay_jsonl(&vec![b'\n'; MAX_REPLAY_BYTES + 1])
                .unwrap_err()
                .code,
            ErrorCode::ReplayByteLimit
        );
    }

    #[test]
    fn hard_bounds_fail_closed() {
        let oversized = vec![b'x'; MAX_EVENT_BYTES + 1];
        assert_eq!(
            EventEnvelope::from_json_bytes(&oversized).unwrap_err().code,
            ErrorCode::EventTooLarge
        );
        let mut deep = json!(null);
        for _ in 0..=MAX_JSON_DEPTH {
            deep = json!([deep]);
        }
        assert_eq!(
            EventEnvelope::from_value(deep).unwrap_err().code,
            ErrorCode::JsonDepthLimit
        );
        let many = Value::Array((0..MAX_JSON_NODES).map(|_| json!(null)).collect());
        assert_eq!(
            EventEnvelope::from_value(many).unwrap_err().code,
            ErrorCode::ItemLimit
        );
        fn binary_tree(depth: usize) -> Value {
            if depth == 0 {
                json!(null)
            } else {
                json!([binary_tree(depth - 1), binary_tree(depth - 1)])
            }
        }
        assert_eq!(
            EventEnvelope::from_value(binary_tree(11)).unwrap_err().code,
            ErrorCode::JsonNodeLimit
        );
        assert_eq!(
            replay_jsonl(&vec![b'x'; MAX_REPLAY_BYTES + 1])
                .unwrap_err()
                .code,
            ErrorCode::ReplayByteLimit
        );
    }

    #[test]
    fn links_stale_disconnect_and_transport_contract() {
        let mut links = TaskLinks::new(["MC-001"]);
        assert!(links.link("agent", &["MC-001".to_owned()]).is_ok());
        assert_eq!(
            links
                .link("agent", &["MC-002".to_owned()])
                .unwrap_err()
                .code,
            ErrorCode::UnknownTask
        );
        assert_eq!(
            links
                .link("agent", &["MC-001".to_owned(), "MC-001".to_owned()])
                .unwrap_err()
                .code,
            ErrorCode::InvalidTaskLink
        );
        let allowed: Vec<String> = (0..65).map(|i| format!("MC-{i:03}")).collect();
        let mut bounded_links = TaskLinks::new(allowed.clone());
        assert!(bounded_links.link("agent", &allowed[..64]).is_ok());
        assert_eq!(
            bounded_links.link("agent", &allowed).unwrap_err().code,
            ErrorCode::InvalidTaskLink
        );
        let mut linked_event = event(1, "linked-event");
        linked_event.task_ids = vec!["MC-001".to_owned()];
        assert_eq!(
            RuntimeReducer::new()
                .apply(linked_event.clone())
                .unwrap_err()
                .code,
            ErrorCode::UnknownTask
        );
        assert_eq!(
            RuntimeReducer::with_task_allowlist(["MC-001"])
                .apply(linked_event)
                .unwrap(),
            ApplyResult::Applied
        );
        let mut reducer = RuntimeReducer::new();
        reducer.apply(event(1, "stale-event")).unwrap();
        let mut second = event(1, "stale-event-b");
        second.agent_id = "b".into();
        reducer.apply(second).unwrap();
        reducer.age_at(Instant::now() + STALE_AFTER + Duration::from_secs(1), false);
        assert!(
            reducer
                .agents()
                .all(|agent| agent.state == AgentState::Stale
                    && agent.activity_kind == ActivityKind::Idle)
        );
        reducer.age_at(Instant::now(), true);
        assert_eq!(
            reducer.agents().next().unwrap().state,
            AgentState::Disconnected
        );
        assert_eq!(
            websocket_transport().unwrap_err().code,
            ErrorCode::UnsupportedTransport
        );
        assert_eq!(stdio_command("codex").unwrap()[1], "app-server");
        let state = reducer.runtime_state("2026-08-29T00:00:00Z").unwrap();
        assert!(
            !loopback_http_response("127.0.0.1", "GET", "/health", &state)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            loopback_http_response("evil.example", "GET", "/health", &state)
                .unwrap_err()
                .code,
            ErrorCode::Schema
        );
        assert_eq!(
            loopback_http_response("127.0.0.1", "POST", "/health", &state)
                .unwrap_err()
                .code,
            ErrorCode::UnsupportedTransport
        );
        let listener = bind_loopback("127.0.0.1", 0).unwrap();
        let address = listener.local_addr().unwrap();
        let thread_state = state.clone();
        let handle = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            serve_http_once(stream, &thread_state).unwrap();
        });
        let mut client = TcpStream::connect(address).unwrap();
        client
            .write_all(b"HEAD /snapshot HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).unwrap();
        handle.join().unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 OK"));
    }

    #[test]
    fn state_projection_is_strict_and_provider_optional() {
        let mut reducer = RuntimeReducer::new();
        reducer.apply(event(1, "state-event")).unwrap();
        let state = reducer.runtime_state("2026-08-29T00:00:00Z").unwrap();
        let encoded = serde_json::to_vec(&state).unwrap();
        assert_eq!(
            RuntimeState::validate(&serde_json::from_slice(&encoded).unwrap()).unwrap(),
            state
        );
        let minimal = json!({"schemaVersion":"1.0","updatedAt":"2026-08-29T00:00:00Z","sourceStatus":"disconnected","capabilities":{"approve":false,"reject":false,"focus":false},"attention":[],"agents":[{"agentId":"a","taskIds":[],"state":"idle","activity":"Idle","attention":"none","requiresAttention":false,"activityKind":"idle"}]});
        assert!(
            serde_json::from_value::<RuntimeState>(minimal)
                .unwrap()
                .validate()
                .is_ok()
        );
        let unknown = json!({"schemaVersion":"1.0","updatedAt":"2026-08-29T00:00:00Z","sourceStatus":"disconnected","capabilities":{"approve":false,"reject":false,"focus":false},"attention":[],"agents":[],"secret":"no"});
        assert!(serde_json::from_value::<RuntimeState>(unknown).is_err());
        let fallback = json!({"schemaVersion":"1.0","updatedAt":"2026-08-29T00:00:00Z","sourceStatus":"file-fallback","capabilities":{"approve":false,"reject":false,"focus":false},"attention":[],"agents":[]});
        assert!(serde_json::from_value::<RuntimeState>(fallback).is_err());
        let none_attention = json!({"schemaVersion":"1.0","updatedAt":"2026-08-29T00:00:00Z","sourceStatus":"disconnected","capabilities":{"approve":false,"reject":false,"focus":false},"attention":[{"agentId":"a","kind":"none","activity":"x","taskIds":[]}],"agents":[]});
        assert_eq!(
            serde_json::from_value::<RuntimeState>(none_attention)
                .unwrap()
                .validate()
                .unwrap_err()
                .code,
            ErrorCode::Schema
        );
    }

    #[test]
    fn timestamp_and_sequence_contracts_are_explicit() {
        let mut invalid = event(1, "bad-time");
        invalid.timestamp = "not-a-date".into();
        assert_eq!(invalid.validate().unwrap_err().code, ErrorCode::Schema);
        let mut reducer = RuntimeReducer::new();
        reducer.apply(event(2, "agent-a")).unwrap();
        let mut other = event(1, "agent-b");
        other.agent_id = "b".into();
        assert_eq!(reducer.apply(other).unwrap(), ApplyResult::Applied);
        let mut many_agents = RuntimeReducer::new();
        for index in 0..MAX_AGENTS {
            let mut current = event(index as u64 + 1, &format!("agent-event-{index}"));
            current.agent_id = format!("agent-{index}");
            many_agents.apply(current).unwrap();
        }
        let mut overflow = event(100, "overflow-agent");
        overflow.agent_id = "agent-overflow".into();
        assert_eq!(
            many_agents.apply(overflow).unwrap_err().code,
            ErrorCode::AgentLimit
        );
        let mut dedupe = RuntimeReducer::new();
        dedupe.apply(event(1, "old-id")).unwrap();
        for index in 2..=MAX_REPLAY_EVENTS as u64 {
            dedupe
                .apply(event(index, &format!("event-{index}")))
                .unwrap();
        }
        let replayed = event(MAX_REPLAY_EVENTS as u64 + 1, "old-id");
        assert_eq!(dedupe.apply(replayed).unwrap(), ApplyResult::Duplicate);
    }
}
