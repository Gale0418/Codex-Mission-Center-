//! Mission Center 的純資料核心：任務模型、Markdown 表格與穩定 digest。

use std::{error::Error, fmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Backlog,
    Ready,
    InProgress,
    Review,
    Done,
    Blocked,
}
pub type Status = TaskStatus;

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "Backlog",
            Self::Ready => "Ready",
            Self::InProgress => "In Progress",
            Self::Review => "Review",
            Self::Done => "Done",
            Self::Blocked => "Blocked",
        }
    }

    pub fn parse(value: &str) -> Result<Self, CoreError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "backlog" => Ok(Self::Backlog),
            "ready" => Ok(Self::Ready),
            "in progress" => Ok(Self::InProgress),
            "review" => Ok(Self::Review),
            "done" => Ok(Self::Done),
            "blocked" => Ok(Self::Blocked),
            other => Err(CoreError::UnsupportedStatus(other.to_owned())),
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub kind: String,
    pub parent: String,
    pub priority: String,
    pub status: TaskStatus,
    pub assignee: String,
    pub dependencies: Vec<String>,
    pub next_action: String,
    pub verification: String,
    pub estimate: String,
    pub tags: Vec<String>,
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreError {
    MissingTable,
    InvalidHeader,
    InvalidSeparator,
    MalformedRow {
        row: usize,
        reason: String,
    },
    WrongCellCount {
        row: usize,
        found: usize,
        expected: usize,
    },
    MissingField {
        row: usize,
        field: &'static str,
    },
    DuplicateTaskId(String),
    UnsupportedStatus(String),
    InvalidTransition {
        from: TaskStatus,
        to: TaskStatus,
    },
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingTable => f.write_str("tasks.md does not contain a Markdown table"),
            Self::InvalidHeader => f.write_str("tasks.md has an invalid table header"),
            Self::InvalidSeparator => f.write_str("tasks.md has an invalid table separator"),
            Self::MalformedRow { row, reason } => {
                write!(f, "tasks.md row {row} is malformed: {reason}")
            }
            Self::WrongCellCount {
                row,
                found,
                expected,
            } => write!(
                f,
                "tasks.md row {row} has {found} cells; expected {expected}"
            ),
            Self::MissingField { row, field } => write!(f, "tasks.md row {row} is missing {field}"),
            Self::DuplicateTaskId(id) => write!(f, "duplicate task ID: {id}"),
            Self::UnsupportedStatus(status) => write!(f, "Unsupported task status: {status}"),
            Self::InvalidTransition { from, to } => {
                write!(f, "transition from {from} to {to} is not allowed")
            }
        }
    }
}
impl Error for CoreError {}
impl CoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::MissingTable => "missing_table",
            Self::InvalidHeader => "invalid_header",
            Self::InvalidSeparator => "invalid_separator",
            Self::MalformedRow { .. } => "malformed_row",
            Self::WrongCellCount { .. } => "wrong_cell_count",
            Self::MissingField { .. } => "missing_field",
            Self::DuplicateTaskId(_) => "duplicate_task_id",
            Self::UnsupportedStatus(_) => "unsupported_status",
            Self::InvalidTransition { .. } => "invalid_transition",
        }
    }
}

/// 分割單一 Markdown row；只解逸出 `|` 與 `\`，與既有 Python 契約一致。
pub fn split_cells(line: &str) -> Result<Vec<String>, CoreError> {
    let mut text = line.trim();
    if let Some(rest) = text.strip_prefix('|') {
        text = rest;
    }
    if text.ends_with('|') && !text.ends_with("\\|") {
        text = &text[..text.len() - 1];
    }
    let mut cells = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            if ch != '|' && ch != '\\' {
                current.push('\\');
            }
            current.push(ch);
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '|' {
            cells.push(current.trim().to_owned());
            current.clear();
        } else {
            current.push(ch);
        }
    }
    if escaped {
        return Err(CoreError::MalformedRow {
            row: 0,
            reason: "incomplete escape".to_owned(),
        });
    }
    cells.push(current.trim().to_owned());
    Ok(cells)
}

fn is_table_line(line: &str) -> bool {
    line.trim_start().starts_with('|')
}
fn is_separator(cell: &str) -> bool {
    let value = cell.trim();
    let value = value.strip_prefix(':').unwrap_or(value);
    let value = value.strip_suffix(':').unwrap_or(value);
    value.len() >= 3 && value.chars().all(|c| c == '-')
}

fn canonical_header(header: &str) -> &str {
    match header.trim() {
        "識別碼" => "ID",
        "標題" => "Title",
        "類型" => "Type",
        "父層" | "上層" => "Parent",
        "優先級" => "Priority",
        "狀態" => "Status",
        "負責人" => "Owner",
        "依賴" => "Depends on",
        "下一步" => "Next action",
        "驗證" => "Verification",
        "驗證方式" => "Verification",
        "預估" => "Estimate",
        "估時" => "Estimate",
        "標籤" => "Tags",
        "備註" => "Notes",
        "ID" => "ID",
        "Title" => "Title",
        "Type" => "Type",
        "Kind" => "Kind",
        "Parent" => "Parent",
        "Priority" => "Priority",
        "Status" => "Status",
        "Owner" => "Owner",
        "Assignee" => "Assignee",
        "Depends on" => "Depends on",
        "Dependencies" => "Dependencies",
        "Next action" => "Next action",
        "Verification" => "Verification",
        "Estimate" => "Estimate",
        "Tags" => "Tags",
        "Notes" => "Notes",
        other => other,
    }
}

fn list(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

/// 解析 Markdown task table blocks；表格前後可有任意標題文字。
///
/// `tasks.md` 可能在保留遷移註記或其他文件段落後，繼續宣告另一個
/// canonical task table。第一個 table 仍維持嚴格契約；後續只有具備
/// `ID`、`Title`、`Status` 欄位的 table 會被納入，避免把任意附錄表格
/// 誤當成 lifecycle task。
pub fn parse_tasks_markdown(text: &str) -> Result<Vec<Task>, CoreError> {
    let lines: Vec<&str> = text.lines().collect();
    let tables = locate_task_table_rows(&lines)?;
    let mut tasks = Vec::new();
    for table in tables {
        let rows = table
            .row_lines
            .iter()
            .map(|&line| lines[line])
            .collect::<Vec<_>>();
        for task in parse_task_rows(&rows, &table.headers)? {
            if tasks.iter().any(|existing: &Task| existing.id == task.id) {
                return Err(CoreError::DuplicateTaskId(task.id));
            }
            tasks.push(task);
        }
    }
    Ok(tasks)
}

/// A canonical task table and the source lines that contain its task rows.
///
/// The line indexes are relative to the slice passed to
/// [`locate_task_table_rows`].  Headerless continuation rows remain attached
/// to the most recent canonical table, even across blank lines or headings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskTableRows {
    pub headers: Vec<String>,
    pub row_lines: Vec<usize>,
    pub id_index: usize,
    pub status_index: usize,
}

/// Locate all canonical task table rows in a Markdown document.
///
/// A table block is allowed to contain blank lines.  A canonical header and
/// separator always start a new table, so a later header cannot be parsed as a
/// task row.  After a heading or another non-task block, only a block whose
/// first row matches the latest task schema is treated as a continuation; this
/// deliberately does not make arbitrary appendix tables into tasks.
pub fn locate_task_table_rows(lines: &[&str]) -> Result<Vec<TaskTableRows>, CoreError> {
    let mut tables = Vec::<TaskTableRows>::new();
    let mut active: Option<usize> = None;
    let mut cursor = 0;
    while let Some(relative_start) = lines[cursor..].iter().position(|line| is_table_line(line)) {
        let start = cursor + relative_start;
        let mut end = start;
        while end < lines.len() && (is_table_line(lines[end]) || lines[end].trim().is_empty()) {
            end += 1;
        }
        let block = lines[start..end]
            .iter()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(offset, line)| (start + offset, *line))
            .collect::<Vec<_>>();

        let starts_with_header = task_headers_at(&block, 0)?.is_some();
        let starts_with_continuation = match active {
            Some(active) => is_task_continuation(
                block.first().map(|(_, line)| *line),
                &tables[active].headers,
            )?,
            None => false,
        };
        if starts_with_header || starts_with_continuation {
            let mut offset = 0;
            while offset < block.len() {
                if let Some(headers) = task_headers_at(&block, offset)? {
                    let id_index = headers
                        .iter()
                        .position(|header| header == "ID")
                        .ok_or(CoreError::InvalidHeader)?;
                    let status_index = headers
                        .iter()
                        .position(|header| header == "Status")
                        .ok_or(CoreError::InvalidHeader)?;
                    tables.push(TaskTableRows {
                        headers,
                        row_lines: Vec::new(),
                        id_index,
                        status_index,
                    });
                    active = Some(tables.len() - 1);
                    offset += 2;
                } else {
                    let active = active.ok_or(CoreError::InvalidHeader)?;
                    tables[active].row_lines.push(block[offset].0);
                    offset += 1;
                }
            }
        } else if tables.is_empty() {
            return Err(strict_task_headers(&block));
        }
        cursor = end;
    }
    if tables.is_empty() {
        return Err(CoreError::MissingTable);
    }
    Ok(tables)
}

fn task_headers_at(
    block: &[(usize, &str)],
    offset: usize,
) -> Result<Option<Vec<String>>, CoreError> {
    if offset + 1 >= block.len() {
        return Ok(None);
    }
    let headers_raw = split_cells(block[offset].1)?;
    let headers = headers_raw
        .iter()
        .map(|header| canonical_header(header).to_owned())
        .collect::<Vec<_>>();
    if !["ID", "Title", "Status"]
        .iter()
        .all(|field| headers.iter().any(|header| header == field))
    {
        return Ok(None);
    }
    let separators = split_cells(block[offset + 1].1).map_err(|_| CoreError::InvalidHeader)?;
    if headers_raw.is_empty() || headers_raw.len() != separators.len() {
        return Err(CoreError::InvalidHeader);
    }
    if separators.iter().any(|cell| !is_separator(cell)) {
        return Err(CoreError::InvalidSeparator);
    }
    Ok(Some(headers))
}

fn strict_task_headers(block: &[(usize, &str)]) -> CoreError {
    if block.len() < 2 {
        return CoreError::MissingTable;
    }
    let headers_raw = match split_cells(block[0].1) {
        Ok(headers) => headers,
        Err(error) => return error,
    };
    let separators = match split_cells(block[1].1) {
        Ok(separators) => separators,
        Err(_) => return CoreError::InvalidHeader,
    };
    if headers_raw.is_empty() || headers_raw.len() != separators.len() {
        return CoreError::InvalidHeader;
    }
    if separators.iter().any(|cell| !is_separator(cell)) {
        return CoreError::InvalidSeparator;
    }
    CoreError::InvalidHeader
}

fn is_task_continuation(line: Option<&str>, headers: &[String]) -> Result<bool, CoreError> {
    let Some(line) = line else {
        return Ok(false);
    };
    let Some(status_index) = headers.iter().position(|header| header == "Status") else {
        return Ok(false);
    };
    let cells = split_cells(line)?;
    Ok(cells.len() == headers.len()
        && !cells[0].is_empty()
        && TaskStatus::parse(&cells[status_index]).is_ok())
}

fn parse_task_rows(lines: &[&str], headers: &[String]) -> Result<Vec<Task>, CoreError> {
    let mut tasks = Vec::new();
    for (row_index, line) in lines.iter().enumerate() {
        let cells = split_cells(line).map_err(|e| match e {
            CoreError::MalformedRow { reason, .. } => CoreError::MalformedRow {
                row: row_index + 1,
                reason,
            },
            other => other,
        })?;
        if cells.len() != headers.len() {
            return Err(CoreError::WrongCellCount {
                row: row_index + 1,
                found: cells.len(),
                expected: headers.len(),
            });
        }
        if cells.iter().all(String::is_empty) {
            continue;
        }
        let value = |name: &str| {
            headers
                .iter()
                .position(|h| *h == name)
                .map(|i| cells[i].clone())
                .unwrap_or_default()
        };
        let id = value("ID");
        if id.is_empty() {
            return Err(CoreError::MissingField {
                row: row_index + 1,
                field: "ID",
            });
        }
        if tasks.iter().any(|task: &Task| task.id == id) {
            return Err(CoreError::DuplicateTaskId(id));
        }
        let status = TaskStatus::parse(&value("Status"))?;
        tasks.push(Task {
            id,
            title: value("Title"),
            kind: {
                let v = value("Type");
                if v.is_empty() { value("Kind") } else { v }
            },
            parent: value("Parent"),
            priority: value("Priority"),
            status,
            assignee: {
                let v = value("Owner");
                if v.is_empty() { value("Assignee") } else { v }
            },
            dependencies: {
                let v = value("Depends on");
                if v.is_empty() {
                    list(&value("Dependencies"))
                } else {
                    list(&v)
                }
            },
            next_action: value("Next action"),
            verification: value("Verification"),
            estimate: value("Estimate"),
            tags: list(&value("Tags")),
            notes: value("Notes"),
        });
    }
    Ok(tasks)
}
pub fn parse_tasks(text: &str) -> Result<Vec<Task>, CoreError> {
    parse_tasks_markdown(text)
}

/// Return the stable identity digest for one canonical task.
///
/// Lifecycle `status` is intentionally excluded so a completion passport
/// remains valid while the task moves through Review into Done. Fields are
/// length-framed in their canonical table order to avoid delimiter collisions
/// and platform-specific serialization differences.
pub fn canonical_task_digest(task: &Task) -> String {
    let mut bytes = b"mission-center-task:1.0\0".to_vec();
    let fields = [
        task.id.as_str(),
        task.title.as_str(),
        task.kind.as_str(),
        task.parent.as_str(),
        task.priority.as_str(),
        task.assignee.as_str(),
        &task.dependencies.join(", "),
        task.next_action.as_str(),
        task.verification.as_str(),
        task.estimate.as_str(),
        &task.tags.join(", "),
        task.notes.as_str(),
    ];
    for field in fields {
        bytes.extend_from_slice(field.len().to_string().as_bytes());
        bytes.push(b':');
        bytes.extend_from_slice(field.as_bytes());
        bytes.push(0);
    }
    sha256_digest(&bytes)
}

pub fn can_transition(from: TaskStatus, to: TaskStatus) -> bool {
    from == to
        || matches!(
            (from, to),
            (
                TaskStatus::Backlog,
                TaskStatus::Ready | TaskStatus::InProgress | TaskStatus::Blocked
            ) | (
                TaskStatus::Ready,
                TaskStatus::InProgress | TaskStatus::Blocked
            ) | (
                TaskStatus::InProgress,
                TaskStatus::Review | TaskStatus::Blocked
            ) | (
                TaskStatus::Review,
                TaskStatus::Done | TaskStatus::InProgress | TaskStatus::Blocked
            ) | (TaskStatus::Blocked, TaskStatus::InProgress)
                | (TaskStatus::Done, TaskStatus::InProgress)
        )
}

pub fn transition_status(task: &mut Task, to: TaskStatus) -> Result<(), CoreError> {
    if can_transition(task.status, to) {
        task.status = to;
        Ok(())
    } else {
        Err(CoreError::InvalidTransition {
            from: task.status,
            to,
        })
    }
}

pub fn canonicalize_hash_bytes(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'\r' {
            if input.get(index + 1) == Some(&b'\n') {
                index += 1;
            }
            output.push(b'\n');
        } else {
            output.push(input[index]);
        }
        index += 1;
    }
    output
}

/// Hot-context fingerprint，來源順序固定且只將 CRLF/CR 正規化成 LF。
pub fn workspace_fingerprint(sources: &[(&str, Option<&[u8]>)]) -> String {
    let mut bytes = b"mission-center-hot-context:1.0\0".to_vec();
    for (name, content) in sources {
        bytes.extend_from_slice(name.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&canonicalize_hash_bytes(content.unwrap_or(b"<missing>")));
        bytes.push(0);
    }
    sha256_digest(&bytes)
}

/// SHA-256，純 Rust 實作以維持 offline、零 runtime dependency。
pub fn sha256_digest(input: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut data = input.to_vec();
    let bit_len = (data.len() as u64) * 8;
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    for chunk in data.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (item, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *item = item.wrapping_add(value);
        }
    }
    h.iter().map(|value| format!("{value:08x}")).collect()
}
pub fn digest_sha256(input: &[u8]) -> String {
    sha256_digest(input)
}

/// Runtime/policy 共用的低階 privacy scanner；只回報命中，不清洗輸入。
/// 上層仍應先施加各自的 envelope/size schema 限制。
pub fn scan_forbidden_content(value: &serde_json::Value) -> Vec<String> {
    let mut errors = Vec::new();
    let mut stack = vec![(value, "$".to_owned())];
    while let Some((current, path)) = stack.pop() {
        match current {
            serde_json::Value::Object(map) => {
                for (key, nested) in map {
                    let lower = key.to_ascii_lowercase();
                    let compact: String = lower
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric())
                        .collect();
                    if [
                        "password",
                        "token",
                        "apikey",
                        "authorization",
                        "secret",
                        "credential",
                    ]
                    .iter()
                    .any(|needle| compact.contains(needle))
                    {
                        errors.push(format!("{path}.{key}"));
                    }
                    stack.push((nested, format!("{path}.{key}")));
                }
            }
            serde_json::Value::Array(items) => {
                for (index, nested) in items.iter().enumerate() {
                    stack.push((nested, format!("{path}[{index}]")));
                }
            }
            serde_json::Value::String(text) => {
                let lower = text.to_ascii_lowercase();
                let normalized: String = lower
                    .chars()
                    .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { ' ' })
                    .collect();
                let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
                let assignment = secret_assignment(&lower);
                let jwt = lower.starts_with("eyj") && lower.split('.').count() == 3;
                let private_key = lower.contains("-----begin ")
                    || lower.contains("ssh-rsa ")
                    || lower.contains("ssh-ed25519 ");
                if assignment || normalized.contains("bearer ") || jwt || private_key {
                    errors.push(path);
                }
            }
            _ => {}
        }
    }
    errors.sort();
    errors.dedup();
    errors
}

fn secret_assignment(text: &str) -> bool {
    [
        "password", "secret", "token", "api_key", "api-key", "api key", "apikey",
    ]
    .iter()
    .any(|prefix| {
        let mut offset = 0;
        while let Some(relative) = text[offset..].find(prefix) {
            let start = offset + relative;
            let boundary = start == 0
                || !text.as_bytes()[start - 1].is_ascii_alphanumeric()
                    && text.as_bytes()[start - 1] != b'_';
            let after = &text[start + prefix.len()..];
            let trimmed = after.trim_start_matches(char::is_whitespace);
            if boundary
                && matches!(trimmed.as_bytes().first(), Some(b':' | b'='))
                && trimmed[1..]
                    .trim_start_matches(char::is_whitespace)
                    .bytes()
                    .next()
                    .is_some_and(|byte| !byte.is_ascii_whitespace())
            {
                return true;
            }
            offset = start + prefix.len();
        }
        false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn digest_known_vector() {
        assert_eq!(
            sha256_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
    #[test]
    fn parses_unicode_and_escapes() {
        let tasks = parse_tasks_markdown("# 任務\r\n| ID | 標題 | 狀態 |\r\n| --- | --- | --- |\r\n| MC-1 | A \\| B \\\\ C | In Progress |").unwrap();
        assert_eq!(tasks[0].title, "A | B \\ C");
    }

    #[test]
    fn first_table_keeps_strict_header_and_separator_errors() {
        assert!(matches!(
            parse_tasks_markdown("| Note | Value |\n| --- | --- |\n| Keep | this |"),
            Err(CoreError::InvalidHeader)
        ));
        assert!(matches!(
            parse_tasks_markdown("| ID | Title | Status |\n| -- | --- | --- |"),
            Err(CoreError::InvalidSeparator)
        ));
    }

    #[test]
    fn graph_requires_review_before_done() {
        assert!(!can_transition(TaskStatus::InProgress, TaskStatus::Done));
        assert!(can_transition(TaskStatus::InProgress, TaskStatus::Review));
        assert!(can_transition(TaskStatus::Review, TaskStatus::Done));
        assert!(can_transition(TaskStatus::Done, TaskStatus::InProgress));
    }

    #[test]
    fn parses_canonical_task_tables_after_document_sections() {
        let source = "| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | First | Done |\n\n## Migration notes\n\n| ID | Title | Status |\n| --- | --- | --- |\n| MC-2 | Second | Review |\n";
        let tasks = parse_tasks_markdown(source).expect("all canonical task tables");
        assert_eq!(
            tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["MC-1", "MC-2"]
        );
    }

    #[test]
    fn parses_indented_canonical_tables_separated_by_blank_lines() {
        let source = "  | ID | Title | Status |\n  | --- | --- | --- |\n  | MC-1 | First | Done |\n\n  | ID | Title | Status |\n  | --- | --- | --- |\n  | MC-2 | Second | Review |\n";
        let tasks = parse_tasks_markdown(source).expect("both canonical tables");
        assert_eq!(
            tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["MC-1", "MC-2"]
        );
    }

    #[test]
    fn parses_heading_continuation_and_ignores_non_task_block() {
        let source = "| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | First | Ready |\n\n## Appendix\n| Note | Value |\n| --- | --- |\n| Keep | this |\n\n## Continuation\n  | MC-2 | Second | Review |\n";
        let tasks = parse_tasks_markdown(source).expect("heading continuation");
        assert_eq!(
            tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["MC-1", "MC-2"]
        );
    }

    #[test]
    fn splits_new_canonical_table_after_headerless_continuation_in_one_block() {
        let source = "| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | First | Ready |\n\n  | MC-2 | Second | Review |\n\n  | ID | Title | Status |\n  | --- | --- | --- |\n  | MC-3 | Third | Done |\n";
        let tasks = parse_tasks_markdown(source).expect("continuation and new table");
        assert_eq!(
            tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["MC-1", "MC-2", "MC-3"]
        );
    }

    #[test]
    fn parses_schema_continuation_rows_after_blank_lines() {
        let source = "| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | First | Done |\n\n| MC-2 | Second | Review |\n";
        let tasks = parse_tasks_markdown(source).expect("continuation rows");
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[1].id, "MC-2");
    }

    #[test]
    fn parses_short_chinese_verification_and_estimate_headers() {
        let source = "| 識別碼 | 標題 | 類型 | 上層 | 優先級 | 狀態 | 負責人 | 依賴 | 下一步 | 驗證 | SmokeTest | Review | 預估 | 標籤 | 備註 |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n| MC-1 | First | Task | MC-0 | P0 | Review | Codex |  | Next | Verify | NO | NO | 2h | plan | Notes |\n";
        let task = parse_tasks_markdown(source)
            .expect("short Chinese headers")
            .remove(0);
        assert_eq!(task.verification, "Verify");
        assert_eq!(task.estimate, "2h");
        assert_eq!(task.parent, "MC-0");
    }
}
