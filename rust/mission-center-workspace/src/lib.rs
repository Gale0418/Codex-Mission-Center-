//! Canonical workspace layout and read-only file access.
use mission_center_core::{
    CoreError, Task, TaskStatus, canonicalize_hash_bytes, locate_task_table_rows,
    parse_tasks_markdown, sha256_digest, split_cells, transition_status,
};
use mission_center_policy::validate_completion_passport;
use serde_json::{Value, json};
use std::{
    error::Error,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

mod derived_views;
pub use derived_views::working_set_ids;
use derived_views::{BRIEF_MAX_BYTES, FOCUS_MAX_BYTES, WORKING_SET_MAX_BYTES};

pub const MISSION_DIRECTORY: &str = "MissionCenter";
pub const TASKS_FILE: &str = "tasks.md";
pub const SNAPSHOT_FILE: &str = "snapshot.md";
pub const PROJECT_MAX_BYTES: u64 = 64 * 1024;
pub const TASKS_MAX_BYTES: u64 = 256 * 1024;
pub const GUARDRAILS_MAX_BYTES: u64 = 64 * 1024;
pub const DAILY_LOG_MAX_BYTES: u64 = 128 * 1024;
pub const SNAPSHOT_MAX_BYTES: u64 = 64 * 1024;
pub const COMPLETION_PASSPORT_MAX_BYTES: u64 = 256 * 1024;
const MAX_RECENT_ATTEMPTS: usize = 5;

#[derive(Debug)]
pub enum WorkspaceError {
    Io(std::io::Error),
    Core(CoreError),
    TooLarge { path: PathBuf, limit: u64 },
    NotFound { path: PathBuf },
    InvalidUtf8 { path: PathBuf },
    UnsafePath { path: PathBuf },
    InvalidLocator(String),
    Conflict(String),
    AlreadyStarted(String),
    Contended(PathBuf),
    InvalidReceipt(String),
    ClaimRejected(String),
    RecoveryUnknown(String),
}
impl fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "workspace I/O error: {e}"),
            Self::Core(e) => e.fmt(f),
            Self::TooLarge { path, limit } => write!(
                f,
                "file exceeds bounded read limit ({limit} bytes): {}",
                path.display()
            ),
            Self::NotFound { path } => write!(f, "file not found: {}", path.display()),
            Self::InvalidUtf8 { path } => write!(f, "file is not valid UTF-8: {}", path.display()),
            Self::UnsafePath { path } => write!(f, "unsafe path: {}", path.display()),
            Self::InvalidLocator(value) => write!(f, "invalid artifact locator: {value}"),
            Self::Conflict(value) => write!(f, "operation conflict: {value}"),
            Self::AlreadyStarted(value) => write!(f, "operation is already started: {value}"),
            Self::Contended(path) => write!(f, "writer lock is contended: {}", path.display()),
            Self::InvalidReceipt(value) => write!(f, "invalid operation receipt: {value}"),
            Self::ClaimRejected(value) => write!(f, "claim rejected: {value}"),
            Self::RecoveryUnknown(value) => write!(f, "recovery unknown: {value}"),
        }
    }
}
impl Error for WorkspaceError {}
impl From<std::io::Error> for WorkspaceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<CoreError> for WorkspaceError {
    fn from(value: CoreError) -> Self {
        Self::Core(value)
    }
}

#[derive(Debug, Clone)]
pub struct MissionWorkspace {
    root: PathBuf,
}
impl MissionWorkspace {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let root = if root.file_name().is_some_and(is_mission_directory_name) {
            root.parent()
                .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
        } else {
            root
        };
        Self { root }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn mission_dir(&self) -> PathBuf {
        self.root.join(MISSION_DIRECTORY)
    }

    /// Resolve a locator below `MissionCenter`, rejecting absolute and traversal paths.
    /// `MissionCenter/foo` is accepted for callers that pass a repository-relative locator.
    pub fn artifact_path(&self, locator: &str) -> Result<PathBuf, WorkspaceError> {
        let raw = Path::new(locator);
        if raw.as_os_str().is_empty()
            || raw.is_absolute()
            || raw.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            return Err(WorkspaceError::InvalidLocator(locator.to_owned()));
        }
        let mut components = raw.components();
        if components
            .next()
            .is_some_and(|component| is_mission_directory_name(component.as_os_str()))
        {
            // A locator relative to the repository may include its canonical directory.
            let mut path = self.root.join(MISSION_DIRECTORY);
            for component in components {
                path.push(component.as_os_str());
            }
            Ok(path)
        } else {
            Ok(self.mission_dir().join(raw))
        }
    }

    pub fn resolve_artifact(&self, locator: &Path) -> Result<PathBuf, WorkspaceError> {
        let value = locator
            .to_str()
            .ok_or_else(|| WorkspaceError::InvalidLocator("locator is not UTF-8".to_owned()))?;
        self.artifact_path(value)
    }

    pub fn read_artifact(&self, locator: &str, limit: u64) -> Result<Vec<u8>, WorkspaceError> {
        let path = self.artifact_path(locator)?;
        read_bounded(&path, limit)
    }

    pub fn read_artifact_text(&self, locator: &str, limit: u64) -> Result<String, WorkspaceError> {
        let path = self.artifact_path(locator)?;
        read_bounded_text(&path, limit)
    }

    pub fn read_path_text(
        &self,
        path: &Path,
        limit: u64,
    ) -> Result<Option<String>, WorkspaceError> {
        let mission = self.mission_dir();
        if !path.starts_with(&mission)
            || path.strip_prefix(&mission).is_ok_and(|relative| {
                relative
                    .components()
                    .any(|part| matches!(part, Component::ParentDir))
            })
        {
            return Err(WorkspaceError::UnsafePath {
                path: path.to_path_buf(),
            });
        }
        match read_bounded_text(path, limit) {
            Ok(value) => Ok(Some(value)),
            Err(WorkspaceError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn artifact_exists(&self, locator: &str) -> Result<bool, WorkspaceError> {
        let path = self.artifact_path(locator)?;
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                ensure_no_reparse(&path)?;
                Ok(path.is_file())
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(WorkspaceError::Io(error)),
        }
    }
    pub fn tasks_path(&self) -> PathBuf {
        self.mission_dir().join(TASKS_FILE)
    }
    pub fn snapshot_path(&self) -> PathBuf {
        self.mission_dir().join(SNAPSHOT_FILE)
    }
    pub fn read_tasks(&self) -> Result<(String, Vec<Task>), WorkspaceError> {
        let text = read_bounded_text(&self.tasks_path(), TASKS_MAX_BYTES)?;
        let tasks = parse_tasks_markdown(&text)?;
        Ok((text, tasks))
    }
    pub fn task_digest(&self) -> Result<String, WorkspaceError> {
        let text = read_bounded(&self.tasks_path(), TASKS_MAX_BYTES)?;
        Ok(sha256_digest(&canonicalize_hash_bytes(&text)))
    }

    pub fn completion_passport_path(&self, task_id: &str) -> Result<PathBuf, WorkspaceError> {
        let id = safe_id(task_id)?;
        Ok(self
            .root
            .join("output")
            .join("mission-center-passports")
            .join(format!("{id}.json")))
    }

    fn read_completion_passport(&self, task_id: &str) -> Result<Value, WorkspaceError> {
        let path = self.completion_passport_path(task_id)?;
        let bytes = read_bounded(&path, COMPLETION_PASSPORT_MAX_BYTES)?;
        let text = String::from_utf8(bytes)
            .map_err(|_| WorkspaceError::InvalidUtf8 { path: path.clone() })?;
        serde_json::from_str(&text).map_err(|error| {
            WorkspaceError::ClaimRejected(format!("invalid completion passport JSON: {error}"))
        })
    }

    fn validate_completion_passport(
        &self,
        passport: &Value,
        task: &Task,
    ) -> Result<(), WorkspaceError> {
        let errors = validate_completion_passport(passport, task, Some(&self.root));
        if errors.is_empty() {
            Ok(())
        } else {
            Err(WorkspaceError::ClaimRejected(format!(
                "completion passport validation failed: {}",
                errors.join("; ")
            )))
        }
    }

    /// Return `None` when a legacy task has no passport, otherwise the strict
    /// validation errors. Doctor uses absence as an explicit unknown/warning.
    pub fn completion_passport_check(
        &self,
        task: &Task,
    ) -> Result<Option<Vec<String>>, WorkspaceError> {
        match self.read_completion_passport(&task.id) {
            Ok(passport) => Ok(Some(validate_completion_passport(
                &passport,
                task,
                Some(&self.root),
            ))),
            Err(WorkspaceError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }
    pub fn fingerprint(&self) -> Result<String, WorkspaceError> {
        let project = read_optional(self.mission_dir().join("project.md"), PROJECT_MAX_BYTES)?;
        let tasks = read_optional(self.tasks_path(), TASKS_MAX_BYTES)?;
        let guardrails = read_optional(
            self.mission_dir().join("guardrails.md"),
            GUARDRAILS_MAX_BYTES,
        )?;
        let daily_log =
            read_optional(self.mission_dir().join("daily-log.md"), DAILY_LOG_MAX_BYTES)?;
        Ok(mission_center_core::workspace_fingerprint(&[
            ("project.md", project.as_deref()),
            ("tasks.md", tasks.as_deref()),
            ("guardrails.md", guardrails.as_deref()),
            ("daily-log.md", daily_log.as_deref()),
        ]))
    }

    fn canonical_snapshot_facts(&self, tasks: &[Task]) -> Result<SnapshotFacts, WorkspaceError> {
        let task_bytes =
            canonicalize_hash_bytes(&read_bounded(&self.tasks_path(), TASKS_MAX_BYTES)?);
        let project_bytes =
            read_optional(self.mission_dir().join("project.md"), PROJECT_MAX_BYTES)?
                .map(|bytes| canonicalize_hash_bytes(&bytes));
        let revision = Command::new("git")
            .args([
                "-C",
                self.root.to_string_lossy().as_ref(),
                "rev-parse",
                "HEAD",
            ])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unavailable".to_owned());
        let mut fingerprint_input = task_bytes;
        if let Some(project) = project_bytes {
            fingerprint_input.push(0);
            fingerprint_input.extend_from_slice(&project);
        }
        fingerprint_input.extend_from_slice(revision.as_bytes());
        let active = tasks.iter().find(|task| {
            matches!(
                task.status,
                mission_center_core::TaskStatus::InProgress
                    | mission_center_core::TaskStatus::Blocked
            )
        });
        Ok(SnapshotFacts {
            state: if active.is_some() {
                "active"
            } else {
                "inactive"
            }
            .to_owned(),
            active: active.map_or_else(
                || "None".to_owned(),
                |task| format!("{} {}", task.id, task.title),
            ),
            status: active.map_or_else(
                || "Inactive".to_owned(),
                |task| task.status.as_str().to_owned(),
            ),
            revision,
            fingerprint: sha256_digest(&fingerprint_input),
            dependencies: active.map_or_else(
                || "None".to_owned(),
                |task| {
                    if task.dependencies.is_empty() {
                        "None".to_owned()
                    } else {
                        task.dependencies.join(", ")
                    }
                },
            ),
            verification: active.map_or_else(
                || "None".to_owned(),
                |task| {
                    if task.verification.is_empty() {
                        "None".to_owned()
                    } else {
                        task.verification.clone()
                    }
                },
            ),
        })
    }

    fn snapshot_is_chinese(&self) -> Result<bool, WorkspaceError> {
        for path in [
            self.mission_dir().join("project.md"),
            self.tasks_path(),
            self.mission_dir().join("progress.md"),
        ] {
            let limit = if path.file_name().is_some_and(|name| name == "project.md") {
                PROJECT_MAX_BYTES
            } else {
                TASKS_MAX_BYTES
            };
            if let Ok(text) = read_bounded_text(&path, limit)
                && ["# 專案", "# 進度", "# 任務", "- 目標:", "- 目標："]
                    .iter()
                    .any(|marker| text.contains(marker))
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    /// 寫入操作刻意拒絕；Rust walking skeleton 尚未提供 mutation adapter。
    pub fn reject_write(&self) -> Result<(), WorkspaceError> {
        Err(WorkspaceError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "workspace writes are not enabled",
        )))
    }
    pub fn snapshot_active(&self) -> Result<bool, WorkspaceError> {
        if !self.snapshot_path().is_file() {
            return Ok(false);
        }
        let text = read_bounded_text(&self.snapshot_path(), SNAPSHOT_MAX_BYTES)?;
        Ok(text.lines().any(|line| {
            let value = line.trim().to_ascii_lowercase();
            value == "- state: active" || value == "state: active"
        }))
    }
}

const MANAGED_SUMMARY_MARKER: &str = "<!-- mission-center-managed-summary v=1 -->";

fn init_limit(name: &str) -> u64 {
    if name == TASKS_FILE {
        TASKS_MAX_BYTES
    } else {
        PROJECT_MAX_BYTES
    }
}

fn init_templates(language: WorkspaceLanguage) -> Vec<(&'static str, &'static str)> {
    match language {
        WorkspaceLanguage::English => vec![
            (
                "brief.md",
                "# Mission Brief\n\nGenerated after bootstrap.\n",
            ),
            (
                "working-set.md",
                "# Active Working Set\n\nGenerated after bootstrap.\n",
            ),
            (
                "critical-lessons.md",
                "# Critical Lessons\n\n## Active Lessons\n\n",
            ),
            (
                "guardrails.md",
                "# Guardrails\n\nAutomation must not change guardrails without explicit human approval.\n",
            ),
            (
                "daily-log.md",
                "# Daily Log\n\n- Last organized: 1970-01-01\n",
            ),
            (
                "project.md",
                "<!-- mission-center-managed-summary v=1 -->\n# Project\n\n- Goal:\n- Cycle:\n- Labels:\n- Activity log:\n- Open comments:\n",
            ),
            (
                "progress.md",
                "<!-- mission-center-managed-summary v=1 -->\n# Progress\n\n- Project:\n- Objective:\n- Current status:\n- Milestone:\n- Progress bar: [----------] 0%\n- Active tasks:\n  - None\n- Blocked by:\n  - None\n- Next update: Re-run sync after any task or smoke-test change.\n",
            ),
            (
                "tasks.md",
                "# Tasks\n\n| ID | Title | Type | Parent | Priority | Status | Owner | Depends on | Next action | Verification | Estimate | Labels | Comments |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n",
            ),
            ("decisions.md", "# Decisions\n\n-\n"),
            (
                "smoke-tests.md",
                "# Smoke Tests\n\n| Date | Linked task ID | What was tested | How it was tested | Expected result | Observed result | Pass / fail | Run type |\n| --- | --- | --- | --- | --- | --- | --- | --- |\n",
            ),
            (
                "notes.md",
                "# Notes\n\n## Research Log\n\n| Pre-search idea | Source | Adopted insight | License status |\n| --- | --- | --- | --- |\n",
            ),
            (
                "snapshot.md",
                "# Snapshot\n\n- Captured at:\n- Project:\n- Cycle:\n- Goal:\n- Progress:\n- Active tasks:\n- Blocked tasks:\n- Recent decisions:\n- Open questions:\n",
            ),
            (
                "closeout.md",
                "# Closeout\n\n- Summary:\n- Completed:\n- Unfinished:\n- Risks:\n- Smoke tests:\n- Retro:\n",
            ),
            (
                "visual-hub.md",
                "# Visual Hub\n\n- Current view: task states, progress, and blockers\n- Helper rule: one helper represents one task from tasks.md\n",
            ),
        ],
        WorkspaceLanguage::TraditionalChinese => vec![
            ("brief.md", "# 任務簡報\n\nBootstrap 後產生。\n"),
            ("working-set.md", "# 當前工作集\n\nBootstrap 後產生。\n"),
            ("critical-lessons.md", "# 重大教訓\n\n## 主動教訓\n\n"),
            (
                "guardrails.md",
                "# 重要護欄\n\n自動化不得未經人工核准變更護欄。\n",
            ),
            ("daily-log.md", "# 每日紀錄\n\n- 最後整理：1970-01-01\n"),
            (
                "project.md",
                "<!-- mission-center-managed-summary v=1 -->\n# 專案\n\n- 目標：\n- 週期：\n- 標籤：\n- 活動紀錄：\n- 開放問題：\n",
            ),
            (
                "progress.md",
                "<!-- mission-center-managed-summary v=1 -->\n# 進度\n\n- 專案：\n- 目標：\n- 目前狀態：\n- 里程碑：\n- 進度條：[----------] 0%\n- 進行中任務：\n  - 無\n- 阻塞原因：\n  - 無\n- 下次更新：任務或冒煙測試有變動後請重新執行同步。\n",
            ),
            (
                "tasks.md",
                "# 任務\n\n| 識別碼 | 標題 | 類型 | 父層 | 優先級 | 狀態 | 負責人 | 依賴 | 下一步 | 驗證方式 | 估時 | 標籤 | 備註 |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |\n",
            ),
            ("decisions.md", "# 決策\n\n-\n"),
            (
                "smoke-tests.md",
                "# 冒煙測試\n\n| 日期 | 對應任務 ID | 測試內容 | 測試方式 | 預期結果 | 實際結果 | 通過 / 失敗 | 類型 |\n| --- | --- | --- | --- | --- | --- | --- | --- |\n",
            ),
            (
                "notes.md",
                "# 筆記\n\n## 研究紀錄\n\n| 搜尋前構想 | 參考來源 | 採納內容 | 授權狀態 |\n| --- | --- | --- | --- |\n",
            ),
            (
                "snapshot.md",
                "# 快照\n\n- 建立時間：\n- 專案：\n- 週期：\n- 目標：\n- 進度：\n- 進行中任務：\n- 阻塞任務：\n- 最近決策：\n- 開放問題：\n",
            ),
            (
                "closeout.md",
                "# 收尾\n\n- 摘要：\n- 已完成：\n- 未完成：\n- 風險：\n- Smoke tests：\n- 回顧：\n",
            ),
            (
                "visual-hub.md",
                "# 視覺 HUB\n\n- 目前畫面：任務狀態、進度與阻塞項目\n- 小人規則：tasks.md 中一個小人代表一個任務\n",
            ),
        ],
    }
}

fn language_labels(language: WorkspaceLanguage) -> [&'static str; 9] {
    match language {
        WorkspaceLanguage::English => [
            "Project",
            "Goal",
            "Cycle",
            "Milestone",
            "Objective",
            "Current status",
            "Active tasks",
            "Blocked by",
            "Next update",
        ],
        WorkspaceLanguage::TraditionalChinese => [
            "專案",
            "目標",
            "週期",
            "里程碑",
            "目標",
            "目前狀態",
            "進行中任務",
            "阻塞原因",
            "下次更新",
        ],
    }
}

fn find_summary_value(text: &str, labels: &[&str]) -> Option<String> {
    text.lines().find_map(|line| {
        let value = line.trim();
        labels.iter().find_map(|label| {
            [format!("- {label}:"), format!("- {label}：")]
                .iter()
                .find_map(|prefix| {
                    value
                        .strip_prefix(prefix)
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                        .map(ToOwned::to_owned)
                })
        })
    })
}

fn summary_identity(
    existing: &str,
    options: &SyncOptions,
    language: WorkspaceLanguage,
) -> (String, String, String, String) {
    let labels = language_labels(language);
    let project_labels = [labels[0], "Project", "專案"];
    let goal_labels = [labels[1], "Goal", "目標"];
    let cycle_labels = [labels[2], "Cycle", "週期"];
    let labels_labels = ["Labels", "標籤"];
    (
        options
            .project
            .clone()
            .or_else(|| find_summary_value(existing, &project_labels))
            .unwrap_or_else(|| "MissionCenter".to_owned()),
        options
            .goal
            .clone()
            .or_else(|| find_summary_value(existing, &goal_labels))
            .unwrap_or_else(|| "MissionCenter workspace".to_owned()),
        options
            .cycle
            .clone()
            .or_else(|| find_summary_value(existing, &cycle_labels))
            .unwrap_or_else(|| "Unassigned".to_owned()),
        options
            .labels
            .clone()
            .or_else(|| find_summary_value(existing, &labels_labels))
            .unwrap_or_else(|| "mission-center".to_owned()),
    )
}

fn validate_sync_options(options: &SyncOptions) -> Result<(), WorkspaceError> {
    for value in [
        options.project.as_deref(),
        options.cycle.as_deref(),
        options.goal.as_deref(),
        options.labels.as_deref(),
        options.milestone.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if value.len() > 512
            || value
                .chars()
                .any(|c| c.is_control() || matches!(c, '\r' | '\n'))
        {
            return Err(WorkspaceError::ClaimRejected(
                "sync metadata is invalid".to_owned(),
            ));
        }
    }
    Ok(())
}

fn sync_date(timestamp: &str) -> Result<&str, WorkspaceError> {
    validate_timestamp(timestamp)?;
    let date = timestamp.get(..10).filter(|date| {
        let bytes = date.as_bytes();
        bytes.len() == 10
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes
                .iter()
                .enumerate()
                .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    });
    date.ok_or_else(|| {
        WorkspaceError::ClaimRejected("sync timestamp must be an RFC3339 date-time".to_owned())
    })
}

fn organize_daily_log(
    existing: Option<&[u8]>,
    language: WorkspaceLanguage,
    date: &str,
) -> Result<Vec<u8>, WorkspaceError> {
    let (title, preferred_marker) = match language {
        WorkspaceLanguage::English => ("# Daily Log", "- Last organized:"),
        WorkspaceLanguage::TraditionalChinese => ("# 每日紀錄", "- 最後整理："),
    };
    let text = existing
        .map(|bytes| String::from_utf8(bytes.to_vec()))
        .transpose()
        .map_err(|_| WorkspaceError::InvalidUtf8 {
            path: PathBuf::from("MissionCenter/daily-log.md"),
        })?
        .unwrap_or_else(|| format!("{title}\n\n{preferred_marker} 1970-01-01\n"));
    let markers = [
        "- 最後整理：",
        "- 最後整理:",
        "- Last organized:",
        "- Last organized：",
    ];
    let marker = text
        .split_inclusive('\n')
        .scan(0usize, |offset, line| {
            let line_start = *offset;
            *offset += line.len();
            Some((line_start, line))
        })
        .find_map(|(line_start, line)| {
            let content = line.trim_start_matches([' ', '\t']);
            let indentation = line.len() - content.len();
            markers.iter().find_map(|candidate| {
                content
                    .starts_with(candidate)
                    .then_some((*candidate, line_start + indentation))
            })
        });
    if let Some((marker, position)) = marker {
        let line_start = text[..position].rfind('\n').map_or(0, |index| index + 1);
        let line_end = text[position..]
            .find('\n')
            .map_or(text.len(), |index| position + index);
        let newline = if line_end > line_start && text.as_bytes()[line_end - 1] == b'\r' {
            "\r\n"
        } else if line_end < text.len() {
            "\n"
        } else {
            ""
        };
        let suffix_start = if line_end < text.len() {
            line_end + 1
        } else {
            line_end
        };
        let indentation = &text[line_start..position];
        return bounded_daily_log(
            format!(
                "{}{}{} {}{}{}",
                &text[..line_start],
                indentation,
                marker,
                date,
                newline,
                &text[suffix_start..]
            )
            .into_bytes(),
        );
    }

    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let separator = if text.is_empty() || text.ends_with('\n') {
        newline
    } else {
        match newline {
            "\r\n" => "\r\n\r\n",
            _ => "\n\n",
        }
    };
    bounded_daily_log(format!("{text}{separator}{preferred_marker} {date}{newline}").into_bytes())
}

fn bounded_daily_log(bytes: Vec<u8>) -> Result<Vec<u8>, WorkspaceError> {
    if bytes.len() as u64 > DAILY_LOG_MAX_BYTES {
        return Err(WorkspaceError::TooLarge {
            path: PathBuf::from("MissionCenter/daily-log.md"),
            limit: DAILY_LOG_MAX_BYTES,
        });
    }
    Ok(bytes)
}

fn compute_progress(tasks: &[Task]) -> (u32, String, Vec<String>, Vec<String>) {
    let ids: std::collections::HashSet<&str> = tasks.iter().map(|task| task.id.as_str()).collect();
    let parents: std::collections::HashSet<&str> = tasks
        .iter()
        .filter_map(|task| {
            (!task.parent.is_empty() && ids.contains(task.parent.as_str()))
                .then_some(task.parent.as_str())
        })
        .collect();
    let leaves = tasks.iter().filter(|task| {
        !parents.contains(task.id.as_str()) && !task.kind.eq_ignore_ascii_case("epic")
    });
    let mut total = 0u32;
    let mut done = 0u32;
    let mut total_est = 0u64;
    let mut done_est = 0u64;
    let mut estimated = 0u32;
    let mut active = Vec::new();
    let mut blocked = Vec::new();
    for task in leaves {
        if task.title.trim().is_empty() {
            continue;
        }
        total += 1;
        if task.status == mission_center_core::TaskStatus::Done {
            done += 1;
        }
        let estimate = task
            .estimate
            .split(|c: char| !c.is_ascii_digit())
            .find(|value| !value.is_empty())
            .and_then(|v| v.parse::<u32>().ok());
        if let Some(value) = estimate {
            estimated += 1;
            total_est = total_est.saturating_add(u64::from(value));
            if task.status == mission_center_core::TaskStatus::Done {
                done_est = done_est.saturating_add(u64::from(value));
            }
        }
        if matches!(
            task.status,
            mission_center_core::TaskStatus::Backlog
                | mission_center_core::TaskStatus::Ready
                | mission_center_core::TaskStatus::InProgress
                | mission_center_core::TaskStatus::Review
        ) && active.len() < 5
        {
            active.push(format!("{} {} ({})", task.id, task.title, task.status));
        }
        if task.status == mission_center_core::TaskStatus::Blocked && blocked.len() < 5 {
            blocked.push(format!("{} {}", task.id, task.title));
        }
    }
    let (percent, mode) = if total > 0 && estimated == total && total_est > 0 {
        (
            u32::try_from(
                (u128::from(done_est) * 100 + u128::from(total_est) / 2) / u128::from(total_est),
            )
            .unwrap_or(100)
            .min(100),
            format!("{done_est}/{total_est} estimated"),
        )
    } else if total > 0 {
        (
            (done * 100 + total / 2)
                .checked_div(total)
                .unwrap_or(0)
                .min(100),
            format!("{done}/{total} tasks"),
        )
    } else {
        (0, "0/0 tasks".to_owned())
    };
    (percent, mode, active, blocked)
}

#[allow(clippy::too_many_arguments)]
fn render_progress(
    language: WorkspaceLanguage,
    project: &str,
    goal: &str,
    milestone: &str,
    percent: u32,
    mode: &str,
    active: &[String],
    blocked: &[String],
) -> String {
    let (
        title,
        project_label,
        objective,
        status,
        milestone_label,
        bar_label,
        active_label,
        blocked_label,
        next_label,
        next_value,
        none,
    ) = match language {
        WorkspaceLanguage::English => (
            "Progress",
            "Project",
            "Objective",
            "Current status",
            "Milestone",
            "Progress bar",
            "Active tasks",
            "Blocked by",
            "Next update",
            "Re-run sync after any task or smoke-test change.",
            "None",
        ),
        WorkspaceLanguage::TraditionalChinese => (
            "進度",
            "專案",
            "目標",
            "目前狀態",
            "里程碑",
            "進度條",
            "進行中任務",
            "阻塞原因",
            "下次更新",
            "任務或冒煙測試有變動後請重新執行同步。",
            "無",
        ),
    };
    let filled = ((percent + 5) / 10).min(10) as usize;
    let bar = format!(
        "[{}{}] {percent}%",
        "#".repeat(filled),
        "-".repeat(10 - filled)
    );
    let active = if active.is_empty() {
        format!("  - {none}\n")
    } else {
        active.iter().map(|v| format!("  - {v}\n")).collect()
    };
    let blocked = if blocked.is_empty() {
        format!("  - {none}\n")
    } else {
        blocked.iter().map(|v| format!("  - {v}\n")).collect()
    };
    format!(
        "{MANAGED_SUMMARY_MARKER}\n# {title}\n\n- {project_label}: {project}\n- {objective}: {goal}\n- {status}: {mode}\n- {milestone_label}: {milestone}\n- {bar_label}: {bar}\n- {active_label}:\n{active}- {blocked_label}:\n{blocked}- {next_label}: {next_value}\n"
    )
}

fn render_project(
    language: WorkspaceLanguage,
    project: &str,
    cycle: &str,
    goal: &str,
    labels: &str,
) -> String {
    let (title, project_label, goal_label, cycle_label, labels_label, activity, comments) =
        match language {
            WorkspaceLanguage::English => (
                "Project",
                "Project",
                "Goal",
                "Cycle",
                "Labels",
                "Activity log",
                "Open comments",
            ),
            WorkspaceLanguage::TraditionalChinese => (
                "專案",
                "專案",
                "目標",
                "週期",
                "標籤",
                "活動紀錄",
                "開放問題",
            ),
        };
    format!(
        "{MANAGED_SUMMARY_MARKER}\n# {title}\n\n- {project_label}: {project}\n- {goal_label}: {goal}\n- {cycle_label}: {cycle}\n- {labels_label}: {labels}\n- {activity}:\n- {comments}:\n"
    )
}

impl MissionWorkspace {
    /// Create the canonical Markdown scaffold.  Existing files are preserved
    /// unless `force` is explicitly requested; tasks.md is never synthesized
    /// from any derived view.
    pub fn init(
        &self,
        operation_id: &str,
        timestamp: &str,
        language: &str,
        force: bool,
    ) -> Result<WriteOutcome, WorkspaceError> {
        validate_timestamp(timestamp)?;
        let language = WorkspaceLanguage::parse(language)?;
        ensure_directory(&self.root)?;
        ensure_directory(&self.mission_dir())?;
        ensure_directory(&self.mission_dir().join("incidents"))?;

        // Hold the workspace lock while taking the planning snapshot as well
        // as while applying it.  Otherwise a concurrent writer can create a
        // file after the snapshot and have it overwritten by this init.
        let lock = self.acquire_writer_lock(&format!("op:{operation_id}"))?;
        let files = init_templates(language);
        let mut planned = Vec::new();
        let mut writes = Vec::new();
        for (name, content) in files {
            let path = self.mission_dir().join(name);
            let existing = read_optional(path.clone(), init_limit(name))?;
            // tasks.md is the sole lifecycle source.  Even an explicit
            // scaffold force must never replace an existing canonical task
            // table with a template.
            let should_write = existing.is_none() || (force && name != TASKS_FILE);
            let selected = if should_write {
                content.as_bytes().to_vec()
            } else {
                existing.clone().unwrap_or_default()
            };
            planned.extend_from_slice(name.as_bytes());
            planned.push(0);
            planned.extend_from_slice(&canonicalize_hash_bytes(&selected));
            planned.push(0);
            if should_write {
                writes.push((path, content.as_bytes().to_vec(), existing));
            }
        }
        let digest = sha256_digest(&planned);
        let result = (|| {
            if self.begin_operation_locked(operation_id, &digest, timestamp)?
                == OperationOutcome::Replay
            {
                return Ok(WriteOutcome::Unchanged);
            }
            let mut applied: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
            for (path, bytes, previous) in &writes {
                if let Err(error) = self.atomic_write(path, bytes) {
                    for (applied_path, applied_previous) in applied.into_iter().rev() {
                        let _ = match applied_previous {
                            Some(previous) => {
                                self.atomic_write(&applied_path, &previous).map(|_| ())
                            }
                            None => fs::remove_file(&applied_path).map_err(WorkspaceError::Io),
                        };
                    }
                    let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                    return Err(error);
                }
                applied.push((path.clone(), (*previous).clone()));
            }
            if let Err(error) = self.commit_operation_locked(operation_id, &digest, timestamp) {
                for (applied_path, applied_previous) in applied.into_iter().rev() {
                    let _ = match applied_previous {
                        Some(previous) => self.atomic_write(&applied_path, &previous).map(|_| ()),
                        None => fs::remove_file(&applied_path).map_err(WorkspaceError::Io),
                    };
                }
                let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                return Err(error);
            }
            Ok(if writes.is_empty() {
                WriteOutcome::Unchanged
            } else {
                WriteOutcome::Changed
            })
        })();
        lock.release()?;
        result
    }

    /// Synchronize managed summaries from canonical tasks.md.  This method is
    /// deliberately narrower than the Python maintenance script: it updates
    /// only managed/missing summaries and derived views, and never writes
    /// tasks.md or claims HUD asset parity.
    pub fn sync(
        &self,
        operation_id: &str,
        timestamp: &str,
    ) -> Result<WriteOutcome, WorkspaceError> {
        self.sync_with_options(operation_id, timestamp, &SyncOptions::default())
    }

    pub fn sync_with_options(
        &self,
        operation_id: &str,
        timestamp: &str,
        options: &SyncOptions,
    ) -> Result<WriteOutcome, WorkspaceError> {
        let date = sync_date(timestamp)?;
        validate_sync_options(options)?;
        // Keep the canonical read, derivation and materialized-view writes in
        // one writer critical section. Reading tasks.md before locking could
        // publish summaries derived from a source changed by a concurrent
        // task mutation.
        let lock = self.acquire_writer_lock(&format!("op:{operation_id}"))?;
        let (tasks_text, tasks) = self.read_tasks()?;
        let language = self.detect_language()?;
        let project_path = self.mission_dir().join("project.md");
        let progress_path = self.mission_dir().join("progress.md");
        let brief_path = self.mission_dir().join("brief.md");
        let working_set_path = self.mission_dir().join("working-set.md");
        let focus_path = self.mission_dir().join("focus.md");
        let daily_log_path = self.mission_dir().join("daily-log.md");
        let existing_project = read_optional(project_path.clone(), PROJECT_MAX_BYTES)?;
        let existing_progress = read_optional(progress_path.clone(), PROJECT_MAX_BYTES)?;
        let existing_brief = read_optional(brief_path.clone(), BRIEF_MAX_BYTES)?;
        let existing_working_set = read_optional(working_set_path.clone(), WORKING_SET_MAX_BYTES)?;
        let existing_focus = read_optional(focus_path.clone(), FOCUS_MAX_BYTES)?;
        let project_text = existing_project
            .as_deref()
            .map(|bytes| String::from_utf8(bytes.to_vec()))
            .transpose()
            .map_err(|_| WorkspaceError::InvalidUtf8 {
                path: project_path.clone(),
            })?
            .unwrap_or_default();
        let progress_text = existing_progress
            .as_deref()
            .map(|bytes| String::from_utf8(bytes.to_vec()))
            .transpose()
            .map_err(|_| WorkspaceError::InvalidUtf8 {
                path: progress_path.clone(),
            })?
            .unwrap_or_default();
        let (project, goal, cycle, labels) = summary_identity(&project_text, options, language);
        let milestone = options
            .milestone
            .clone()
            .or_else(|| find_summary_value(&progress_text, &[language_labels(language)[3]]))
            .unwrap_or_else(|| "Next slice".to_owned());
        let (percent, mode, active, blocked) = compute_progress(&tasks);
        let progress = render_progress(
            language, &project, &goal, &milestone, percent, &mode, &active, &blocked,
        );
        let managed_project = project_text.contains(MANAGED_SUMMARY_MARKER);
        let managed_progress = progress_text.contains(MANAGED_SUMMARY_MARKER);
        let project_bytes =
            if options.rewrite_summaries || existing_project.is_none() || managed_project {
                Some(render_project(language, &project, &cycle, &goal, &labels).into_bytes())
            } else {
                None
            };
        let progress_bytes =
            if options.rewrite_summaries || existing_progress.is_none() || managed_progress {
                Some(progress.into_bytes())
            } else {
                None
            };
        let final_project = project_bytes.as_deref().or(existing_project.as_deref());
        let guardrails_bytes = read_optional(
            self.mission_dir().join("guardrails.md"),
            GUARDRAILS_MAX_BYTES,
        )?;
        let existing_daily_log = read_optional(daily_log_path.clone(), DAILY_LOG_MAX_BYTES)?;
        let daily_log_bytes = organize_daily_log(existing_daily_log.as_deref(), language, date)?;
        let daily_log_write = (existing_daily_log.as_deref() != Some(daily_log_bytes.as_slice()))
            .then_some(daily_log_bytes.clone());
        let workspace_fingerprint = mission_center_core::workspace_fingerprint(&[
            ("project.md", final_project),
            ("tasks.md", Some(tasks_text.as_bytes())),
            ("guardrails.md", guardrails_bytes.as_deref()),
            ("daily-log.md", Some(daily_log_bytes.as_slice())),
        ]);
        let tasks_fingerprint = mission_center_core::workspace_fingerprint(&[(
            "tasks.md",
            Some(tasks_text.as_bytes()),
        )]);
        let daily_log_text = String::from_utf8(daily_log_bytes.clone()).map_err(|_| {
            WorkspaceError::InvalidUtf8 {
                path: daily_log_path.clone(),
            }
        })?;
        let guardrails_text = guardrails_bytes
            .as_deref()
            .map(|bytes| String::from_utf8(bytes.to_vec()))
            .transpose()
            .map_err(|_| WorkspaceError::InvalidUtf8 {
                path: self.mission_dir().join("guardrails.md"),
            })?;
        let views = derived_views::render_views(
            &tasks,
            &derived_views::RenderInput {
                project: &project,
                goal: &goal,
                cycle: &cycle,
                workspace_fingerprint: &workspace_fingerprint,
                tasks_fingerprint: &tasks_fingerprint,
                language,
                timestamp,
                daily_log: Some(&daily_log_text),
                guardrails: guardrails_text.as_deref(),
            },
        )
        .map_err(WorkspaceError::Core)?;
        for (path, text, limit) in [
            (&brief_path, &views.brief, BRIEF_MAX_BYTES),
            (&working_set_path, &views.working_set, WORKING_SET_MAX_BYTES),
            (&focus_path, &views.focus, FOCUS_MAX_BYTES),
        ] {
            if text.len() as u64 > limit {
                return Err(WorkspaceError::TooLarge {
                    path: path.clone(),
                    limit,
                });
            }
        }
        let brief_text = existing_brief
            .as_deref()
            .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
            .unwrap_or_default();
        let working_set_text = existing_working_set
            .as_deref()
            .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
            .unwrap_or_default();
        let focus_text = existing_focus
            .as_deref()
            .and_then(|bytes| String::from_utf8(bytes.to_vec()).ok())
            .unwrap_or_default();
        let brief_bytes = if existing_brief.is_none() || derived_views::is_managed_view(&brief_text)
        {
            Some(views.brief.into_bytes())
        } else {
            None
        };
        let working_set_bytes = if existing_working_set.is_none()
            || derived_views::is_managed_view(&working_set_text)
        {
            Some(views.working_set.into_bytes())
        } else {
            None
        };
        let focus_bytes = if existing_focus.is_none() || derived_views::is_managed_view(&focus_text)
        {
            Some(views.focus.into_bytes())
        } else {
            None
        };
        let mut planned = canonicalize_hash_bytes(tasks_text.as_bytes());
        for (name, existing, generated) in [
            (
                "project.md",
                existing_project.as_ref(),
                project_bytes.as_ref(),
            ),
            (
                "progress.md",
                existing_progress.as_ref(),
                progress_bytes.as_ref(),
            ),
            ("brief.md", existing_brief.as_ref(), brief_bytes.as_ref()),
            (
                "working-set.md",
                existing_working_set.as_ref(),
                working_set_bytes.as_ref(),
            ),
            ("focus.md", existing_focus.as_ref(), focus_bytes.as_ref()),
            (
                "daily-log.md",
                existing_daily_log.as_ref(),
                daily_log_write.as_ref(),
            ),
        ] {
            planned.push(0);
            planned.extend_from_slice(name.as_bytes());
            planned.push(0);
            if let Some(bytes) = generated.or(existing) {
                planned.extend_from_slice(&canonicalize_hash_bytes(bytes));
            } else {
                planned.extend_from_slice(b"<missing>");
            }
        }
        let digest = sha256_digest(&planned);
        let result = (|| {
            if self.begin_operation_locked(operation_id, &digest, timestamp)?
                == OperationOutcome::Replay
            {
                return Ok(WriteOutcome::Unchanged);
            }
            let mut changed = false;
            let writes = [
                (
                    &project_path,
                    project_bytes.as_ref(),
                    existing_project.as_ref(),
                ),
                (
                    &progress_path,
                    progress_bytes.as_ref(),
                    existing_progress.as_ref(),
                ),
                (&brief_path, brief_bytes.as_ref(), existing_brief.as_ref()),
                (
                    &working_set_path,
                    working_set_bytes.as_ref(),
                    existing_working_set.as_ref(),
                ),
                (&focus_path, focus_bytes.as_ref(), existing_focus.as_ref()),
                (
                    &daily_log_path,
                    daily_log_write.as_ref(),
                    existing_daily_log.as_ref(),
                ),
            ];
            let mut applied: Vec<(&Path, Option<&Vec<u8>>)> = Vec::new();
            for (path, bytes, previous) in writes {
                if let Some(bytes) = bytes {
                    match self.atomic_write(path, bytes) {
                        Ok(outcome) => {
                            changed |= outcome == WriteOutcome::Changed;
                            if outcome == WriteOutcome::Changed {
                                applied.push((path, previous));
                            }
                        }
                        Err(error) => {
                            for (applied_path, applied_previous) in applied.into_iter().rev() {
                                let _ = match applied_previous {
                                    Some(old) => self.atomic_write(applied_path, old).map(|_| ()),
                                    None => {
                                        fs::remove_file(applied_path).map_err(WorkspaceError::Io)
                                    }
                                };
                            }
                            let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                            return Err(error);
                        }
                    }
                }
            }
            if let Err(error) = self.commit_operation_locked(operation_id, &digest, timestamp) {
                for (applied_path, applied_previous) in applied.into_iter().rev() {
                    let _ = match applied_previous {
                        Some(old) => self.atomic_write(applied_path, old).map(|_| ()),
                        None => fs::remove_file(applied_path).map_err(WorkspaceError::Io),
                    };
                }
                let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                return Err(error);
            }
            Ok(if changed {
                WriteOutcome::Changed
            } else {
                WriteOutcome::Unchanged
            })
        })();
        lock.release()?;
        result
    }

    fn detect_language(&self) -> Result<WorkspaceLanguage, WorkspaceError> {
        for (path, limit) in [
            (self.mission_dir().join("project.md"), PROJECT_MAX_BYTES),
            (self.mission_dir().join("progress.md"), PROJECT_MAX_BYTES),
            (self.tasks_path(), TASKS_MAX_BYTES),
        ] {
            if let Ok(text) = read_bounded_text(&path, limit)
                && ["# 專案", "# 進度", "# 任務", "- 目標:", "- 目標："]
                    .iter()
                    .any(|marker| text.contains(marker))
            {
                return Ok(WorkspaceLanguage::TraditionalChinese);
            }
        }
        Ok(WorkspaceLanguage::English)
    }
}

#[cfg(windows)]
fn is_mission_directory_name(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy()
        .eq_ignore_ascii_case(MISSION_DIRECTORY)
}

#[cfg(not(windows))]
fn is_mission_directory_name(name: &std::ffi::OsStr) -> bool {
    name == MISSION_DIRECTORY
}

fn read_bounded_text(path: &Path, limit: u64) -> Result<String, WorkspaceError> {
    let bytes = read_bounded(path, limit)?;
    String::from_utf8(bytes).map_err(|_| WorkspaceError::InvalidUtf8 {
        path: path.to_path_buf(),
    })
}
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, WorkspaceError> {
    ensure_no_reparse(path)?;
    let mut file = File::open(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            WorkspaceError::NotFound {
                path: path.to_path_buf(),
            }
        } else {
            WorkspaceError::Io(error)
        }
    })?;
    let mut bytes = Vec::new();
    let mut limited = Read::by_ref(&mut file).take(limit.saturating_add(1));
    limited.read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(WorkspaceError::TooLarge {
            path: path.to_path_buf(),
            limit,
        });
    }
    Ok(bytes)
}
fn read_optional(path: PathBuf, limit: u64) -> Result<Option<Vec<u8>>, WorkspaceError> {
    match read_bounded(&path, limit) {
        Ok(value) => Ok(Some(value)),
        Err(WorkspaceError::NotFound { .. }) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn read_bounded_file(path: &Path, limit: u64) -> Result<Vec<u8>, WorkspaceError> {
    read_bounded(path, limit)
}

pub fn read_bounded_utf8(path: &Path, limit: u64) -> Result<String, WorkspaceError> {
    read_bounded_text(path, limit)
}

fn ensure_no_reparse(path: &Path) -> Result<(), WorkspaceError> {
    // symlink_metadata checks every existing component before opening. Rust std does
    // not expose a portable openat/FILE_FLAG_OPEN_REPARSE_POINT API; the remaining
    // check-to-open race is therefore documented rather than claimed impossible.
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || is_reparse_point(&metadata) => {
                return Err(WorkspaceError::UnsafePath {
                    path: current.clone(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(WorkspaceError::NotFound {
                    path: path.to_path_buf(),
                });
            }
            Err(error) => return Err(WorkspaceError::Io(error)),
        }
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_metadata: &fs::Metadata) -> bool {
    false
}

const INTERNAL_MAX_BYTES: u64 = 64 * 1024;
const OPERATION_ID_MAX_BYTES: usize = 128;
const OWNER_TOKEN_MAX_BYTES: usize = 256;
const LOCK_TOMBSTONE_PREFIX: &str = "writer.lock.releasing-";

#[derive(Debug, Clone, PartialEq, Eq)]
struct OperationReceipt {
    operation_id: String,
    digest: String,
    status: String,
    timestamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PulseRecord {
    pulse_id: String,
    task_id: String,
    phase: String,
    outcome: String,
    next_action: String,
    evidence_ref: String,
    budget_remaining: u64,
    causal_parent: Option<String>,
    recorded_at: String,
}

struct SnapshotFacts {
    state: String,
    active: String,
    status: String,
    revision: String,
    fingerprint: String,
    dependencies: String,
    verification: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    Changed,
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransitionResult {
    pub outcome: WriteOutcome,
    pub from: TaskStatus,
    pub to: TaskStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationOutcome {
    Started,
    Replay,
    Committed,
}

/// The small, deterministic part of the Python bootstrap contract that the
/// Rust companion owns.  HUD asset copying and the richer maintenance log are
/// intentionally outside this bounded native slice.
pub const REQUIRED_INIT_FILES: &[&str] = &[
    "brief.md",
    "working-set.md",
    "critical-lessons.md",
    "guardrails.md",
    "daily-log.md",
    "project.md",
    "progress.md",
    "tasks.md",
    "decisions.md",
    "smoke-tests.md",
    "notes.md",
    "snapshot.md",
    "closeout.md",
    "visual-hub.md",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceLanguage {
    English,
    TraditionalChinese,
}

impl WorkspaceLanguage {
    pub fn parse(value: &str) -> Result<Self, WorkspaceError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "en" | "english" | "auto" => Ok(Self::English),
            "zh-tw" | "zh_tw" | "traditional-chinese" => Ok(Self::TraditionalChinese),
            _ => Err(WorkspaceError::InvalidLocator(format!(
                "unsupported language: {value}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOptions {
    pub project: Option<String>,
    pub cycle: Option<String>,
    pub goal: Option<String>,
    pub labels: Option<String>,
    pub milestone: Option<String>,
    pub rewrite_summaries: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotOptions {
    pub note: Option<String>,
    pub attempts: Vec<Value>,
    pub hypotheses: Vec<String>,
    pub evidences: Vec<String>,
    pub verification_result: Option<String>,
    pub verification_action: Option<String>,
    pub verification_evidence: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRecord {
    pub task_id: String,
    pub owner: String,
    pub fence: u64,
    pub expires_at: String,
    pub operation_id: String,
    pub digest: String,
}

#[derive(Debug)]
pub struct WriterLock {
    path: PathBuf,
    token: String,
    released: bool,
}

impl WriterLock {
    pub fn token(&self) -> &str {
        &self.token
    }

    fn tombstone_path(&self) -> PathBuf {
        self.path
            .with_file_name(format!("{LOCK_TOMBSTONE_PREFIX}{}", unique_nonce()))
    }

    pub fn release(mut self) -> Result<(), WorkspaceError> {
        if self.released {
            return Ok(());
        }
        ensure_no_reparse(&self.path)?;
        let tombstone = self.tombstone_path();
        fs::rename(&self.path, &tombstone).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                WorkspaceError::Conflict("writer lock owner changed".to_owned())
            } else {
                WorkspaceError::Io(error)
            }
        })?;
        let current = read_bounded(&tombstone, OWNER_TOKEN_MAX_BYTES as u64)?;
        if current != self.token.as_bytes() {
            return Err(WorkspaceError::Conflict(
                "writer lock owner changed; recovery tombstone retained".to_owned(),
            ));
        }
        fs::remove_file(&tombstone)?;
        self.released = true;
        Ok(())
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // Detach first so a replaced lock can never be removed by this owner.
        // A mismatched tombstone is intentionally retained as a fail-closed
        // recovery artifact for an explicit operator reconciliation.
        if !self.released {
            let tombstone = self.tombstone_path();
            if fs::rename(&self.path, &tombstone).is_ok()
                && let Ok(current) = read_bounded(&tombstone, OWNER_TOKEN_MAX_BYTES as u64)
                && current == self.token.as_bytes()
            {
                let _ = fs::remove_file(&tombstone);
            }
        }
    }
}

impl MissionWorkspace {
    pub fn writer_lock_path(&self) -> PathBuf {
        self.mission_dir()
            .join(".mission-center")
            .join("writer.lock")
    }

    /// Return a retained lock-release tombstone, if one needs explicit
    /// reconciliation before another writer may proceed.
    pub fn writer_lock_recovery_artifact(&self) -> Result<Option<PathBuf>, WorkspaceError> {
        let dir = self.mission_dir().join(".mission-center");
        match fs::symlink_metadata(&dir) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                    return Err(WorkspaceError::UnsafePath { path: dir });
                }
                find_lock_tombstone(&dir)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(WorkspaceError::Io(error)),
        }
    }

    /// Acquire the single writer lock. Existing locks are never treated as stale.
    pub fn acquire_writer_lock(&self, owner_token: &str) -> Result<WriterLock, WorkspaceError> {
        if owner_token.is_empty() || owner_token.len() > OWNER_TOKEN_MAX_BYTES {
            return Err(WorkspaceError::InvalidLocator(
                "invalid owner token".to_owned(),
            ));
        }
        let token = format!("{owner_token}:{}", unique_nonce());
        if token.len() > OWNER_TOKEN_MAX_BYTES {
            return Err(WorkspaceError::InvalidLocator(
                "owner token is too long".to_owned(),
            ));
        }
        let dir = self.mission_dir().join(".mission-center");
        ensure_directory(&dir)?;
        if let Some(recovery) = find_lock_tombstone(&dir)? {
            return Err(WorkspaceError::Contended(recovery));
        }
        let path = dir.join("writer.lock");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        let mut file = options.open(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                WorkspaceError::Contended(path.clone())
            } else {
                WorkspaceError::Io(error)
            }
        })?;
        if let Err(error) = (|| {
            file.write_all(token.as_bytes())?;
            file.flush()?;
            sync_all_portable(&file)
        })() {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(WorkspaceError::Io(error));
        }
        Ok(WriterLock {
            path,
            token,
            released: false,
        })
    }

    /// Write bytes using a same-directory create-new temporary and atomic rename.
    pub fn atomic_write(
        &self,
        target: &Path,
        bytes: &[u8],
    ) -> Result<WriteOutcome, WorkspaceError> {
        let mission = self.mission_dir();
        if !target.starts_with(&mission)
            || target.strip_prefix(&mission).is_ok_and(|path| {
                path.components()
                    .any(|part| matches!(part, Component::ParentDir))
            })
        {
            return Err(WorkspaceError::UnsafePath {
                path: target.to_path_buf(),
            });
        }
        let parent = target.parent().ok_or_else(|| WorkspaceError::UnsafePath {
            path: target.to_path_buf(),
        })?;
        ensure_no_reparse(parent)?;
        match fs::symlink_metadata(target) {
            Ok(_) => {
                ensure_no_reparse(target)?;
                let existing_bytes = fs::metadata(target)?.len();
                let replacement_bytes = bytes.len() as u64;
                if existing_bytes == replacement_bytes && existing_bytes <= TASKS_MAX_BYTES {
                    let old = read_bounded(target, existing_bytes.max(INTERNAL_MAX_BYTES))?;
                    if old == bytes {
                        return Ok(WriteOutcome::Unchanged);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(WorkspaceError::Io(error)),
        }
        let nonce = unique_nonce();
        let name = target
            .file_name()
            .ok_or_else(|| WorkspaceError::UnsafePath {
                path: target.to_path_buf(),
            })?
            .to_string_lossy();
        let mut temporary = parent.join(format!(".{name}.tmp-{nonce}"));
        let result = (|| {
            let mut file = loop {
                match OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)
                {
                    Ok(file) => break file,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        temporary = parent.join(format!(".{name}.tmp-{}", unique_nonce()));
                    }
                    Err(error) => return Err(error),
                }
            };
            file.write_all(bytes)?;
            file.flush()?;
            sync_all_portable(&file)?;
            fs::rename(&temporary, target)?;
            #[cfg(unix)]
            {
                let directory = File::open(parent)?;
                sync_all_portable(&directory)?;
            }
            Ok::<(), io::Error>(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
            .map(|()| WriteOutcome::Changed)
            .map_err(WorkspaceError::Io)
    }

    pub fn atomic_write_if_changed(
        &self,
        target: &Path,
        bytes: &[u8],
    ) -> Result<WriteOutcome, WorkspaceError> {
        self.atomic_write(target, bytes)
    }

    pub fn operation_path(&self, operation_id: &str) -> Result<PathBuf, WorkspaceError> {
        let id = safe_id(operation_id)?;
        Ok(self
            .mission_dir()
            .join(".mission-center")
            .join("operations")
            .join(format!("{id}.json")))
    }

    pub fn operation_status(&self, operation_id: &str) -> Result<String, WorkspaceError> {
        let receipt = read_bounded_text(&self.operation_path(operation_id)?, INTERNAL_MAX_BYTES)?;
        Ok(parse_operation_receipt(&receipt)?.status)
    }

    pub fn begin_operation(
        &self,
        operation_id: &str,
        digest: &str,
        timestamp: &str,
    ) -> Result<OperationOutcome, WorkspaceError> {
        validate_timestamp(timestamp)?;
        let lock = self.acquire_writer_lock(&format!("op:{operation_id}"))?;
        let result = self.begin_operation_locked(operation_id, digest, timestamp);
        lock.release()?;
        result
    }

    fn begin_operation_locked(
        &self,
        operation_id: &str,
        digest: &str,
        timestamp: &str,
    ) -> Result<OperationOutcome, WorkspaceError> {
        let path = self.operation_path(operation_id)?;
        if fs::symlink_metadata(&path).is_ok() {
            let receipt = read_bounded_text(&path, INTERNAL_MAX_BYTES)?;
            let prior = parse_operation_receipt(&receipt)?;
            if prior.operation_id != operation_id || validate_timestamp(&prior.timestamp).is_err() {
                return Err(WorkspaceError::InvalidReceipt(
                    "receipt identity/timestamp mismatch".to_owned(),
                ));
            }
            if prior.status == "committed" {
                if prior.digest == digest {
                    return Ok(OperationOutcome::Replay);
                }
                return Err(WorkspaceError::Conflict(operation_id.to_owned()));
            }
            if prior.status == "aborted" {
                if prior.digest != digest {
                    return Err(WorkspaceError::Conflict(operation_id.to_owned()));
                }
                let body = format!(
                    "{{\"schemaVersion\":\"1.0\",\"operationId\":{},\"digest\":{},\"status\":\"started\",\"timestamp\":{}}}",
                    json_quote(operation_id),
                    json_quote(digest),
                    json_quote(timestamp)
                );
                self.atomic_write(&path, body.as_bytes())?;
                return Ok(OperationOutcome::Started);
            }
            if prior.status != "started" {
                return Err(WorkspaceError::InvalidReceipt("unknown status".to_owned()));
            }
            return Err(WorkspaceError::AlreadyStarted(operation_id.to_owned()));
        }
        ensure_directory(path.parent().expect("operation parent"))?;
        let body = format!(
            "{{\"schemaVersion\":\"1.0\",\"operationId\":{},\"digest\":{},\"status\":\"started\",\"timestamp\":{}}}",
            json_quote(operation_id),
            json_quote(digest),
            json_quote(timestamp)
        );
        self.atomic_write(&path, body.as_bytes())?;
        Ok(OperationOutcome::Started)
    }

    pub fn commit_operation(
        &self,
        operation_id: &str,
        digest: &str,
        timestamp: &str,
    ) -> Result<OperationOutcome, WorkspaceError> {
        validate_timestamp(timestamp)?;
        let lock = self.acquire_writer_lock(&format!("op:{operation_id}"))?;
        let result = self.commit_operation_locked(operation_id, digest, timestamp);
        lock.release()?;
        result
    }

    fn commit_operation_locked(
        &self,
        operation_id: &str,
        digest: &str,
        timestamp: &str,
    ) -> Result<OperationOutcome, WorkspaceError> {
        let path = self.operation_path(operation_id)?;
        let prior = read_bounded_text(&path, INTERNAL_MAX_BYTES)?;
        let prior = parse_operation_receipt(&prior)?;
        if prior.operation_id != operation_id || prior.digest != digest {
            return Err(WorkspaceError::Conflict(operation_id.to_owned()));
        }
        if prior.status == "committed" {
            return Ok(OperationOutcome::Replay);
        }
        if prior.status != "started" {
            return Err(WorkspaceError::InvalidReceipt("unknown status".to_owned()));
        }
        let body = format!(
            "{{\"schemaVersion\":\"1.0\",\"operationId\":{},\"digest\":{},\"status\":\"committed\",\"timestamp\":{}}}",
            json_quote(operation_id),
            json_quote(digest),
            json_quote(timestamp)
        );
        self.atomic_write(&path, body.as_bytes())?;
        Ok(OperationOutcome::Committed)
    }

    fn abort_operation_locked(
        &self,
        operation_id: &str,
        digest: &str,
        timestamp: &str,
    ) -> Result<(), WorkspaceError> {
        let path = self.operation_path(operation_id)?;
        let prior = read_bounded_text(&path, INTERNAL_MAX_BYTES)?;
        let prior = parse_operation_receipt(&prior)?;
        if prior.operation_id != operation_id || prior.digest != digest {
            return Err(WorkspaceError::Conflict(operation_id.to_owned()));
        }
        if prior.status != "started" {
            return Err(WorkspaceError::InvalidReceipt(
                "abort requires a started receipt".to_owned(),
            ));
        }
        let body = format!(
            "{{\"schemaVersion\":\"1.0\",\"operationId\":{},\"digest\":{},\"status\":\"aborted\",\"timestamp\":{}}}",
            json_quote(operation_id),
            json_quote(digest),
            json_quote(timestamp)
        );
        self.atomic_write(&path, body.as_bytes())?;
        Ok(())
    }

    pub fn abort_operation(
        &self,
        operation_id: &str,
        digest: &str,
        timestamp: &str,
    ) -> Result<(), WorkspaceError> {
        validate_timestamp(timestamp)?;
        let lock = self.acquire_writer_lock(&format!("op:{operation_id}"))?;
        let result = self.abort_operation_locked(operation_id, digest, timestamp);
        lock.release()?;
        result
    }

    pub fn claim_path(&self, task_id: &str) -> PathBuf {
        self.mission_dir()
            .join(".mission-center")
            .join("claims")
            .join(format!(
                "{}.json",
                mission_center_core::sha256_digest(task_id.as_bytes())
            ))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn claim(
        &self,
        task_id: &str,
        owner: &str,
        fence: u64,
        expires_at: &str,
        now: &str,
        operation_id: &str,
        committed_at: &str,
    ) -> Result<ClaimRecord, WorkspaceError> {
        if owner.is_empty() || owner.len() > OWNER_TOKEN_MAX_BYTES {
            return Err(WorkspaceError::ClaimRejected(
                "owner is empty or too long".to_owned(),
            ));
        }
        validate_timestamp(expires_at).map_err(|_| {
            WorkspaceError::ClaimRejected("invalid expires_at timestamp".to_owned())
        })?;
        validate_timestamp(now)
            .map_err(|_| WorkspaceError::ClaimRejected("invalid now timestamp".to_owned()))?;
        validate_timestamp(committed_at).map_err(|_| {
            WorkspaceError::ClaimRejected("invalid committed_at timestamp".to_owned())
        })?;
        if timestamp_value(expires_at)
            .map_err(|_| WorkspaceError::ClaimRejected("invalid expires_at timestamp".to_owned()))?
            <= timestamp_value(now)
                .map_err(|_| WorkspaceError::ClaimRejected("invalid now timestamp".to_owned()))?
        {
            return Err(WorkspaceError::ClaimRejected(
                "expires_at must be after now".to_owned(),
            ));
        }
        let lock = self.acquire_writer_lock(&format!("op:{operation_id}"))?;
        let result = (|| {
            let (_, tasks) = self.read_tasks()?;
            if !tasks.iter().any(|task| task.id == task_id) {
                return Err(WorkspaceError::ClaimRejected(format!(
                    "unknown task: {task_id}"
                )));
            }
            let path = self.claim_path(task_id);
            let digest = mission_center_core::sha256_digest(
                format!("claim\0{task_id}\0{owner}\0{fence}\0{expires_at}").as_bytes(),
            );
            let old = match fs::symlink_metadata(&path) {
                Ok(_) => Some(self.read_claim(task_id)?),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(WorkspaceError::Io(error)),
            };
            if let Some(old) = &old {
                if old.digest == digest && old.operation_id == operation_id {
                    let receipt =
                        read_bounded_text(&self.operation_path(operation_id)?, INTERNAL_MAX_BYTES)?;
                    let prior = parse_operation_receipt(&receipt)?;
                    if prior.operation_id != operation_id || prior.digest != digest {
                        return Err(WorkspaceError::Conflict(operation_id.to_owned()));
                    }
                    if prior.status == "started" {
                        self.commit_operation_locked(operation_id, &digest, committed_at)?;
                    } else if prior.status == "aborted" {
                        self.begin_operation_locked(operation_id, &digest, committed_at)?;
                        self.commit_operation_locked(operation_id, &digest, committed_at)?;
                    }
                    return Ok(old.clone());
                }
                if fence <= old.fence {
                    return Err(WorkspaceError::ClaimRejected(
                        "fence must increase monotonically".to_owned(),
                    ));
                }
                if !is_expired(now, &old.expires_at) {
                    return Err(WorkspaceError::ClaimRejected(
                        "active claim exists".to_owned(),
                    ));
                }
            }
            let operation = self.begin_operation_locked(operation_id, &digest, committed_at)?;
            if operation == OperationOutcome::Replay {
                return self.read_claim(task_id);
            }
            ensure_directory(path.parent().expect("claims parent"))?;
            let record = ClaimRecord {
                task_id: task_id.to_owned(),
                owner: owner.to_owned(),
                fence,
                expires_at: expires_at.to_owned(),
                operation_id: operation_id.to_owned(),
                digest: digest.clone(),
            };
            let body = claim_json(&record, committed_at);
            if let Err(error) = self.atomic_write(&path, body.as_bytes()) {
                let _ = self.abort_operation_locked(operation_id, &digest, committed_at);
                return Err(error);
            }
            if let Err(error) = self.commit_operation_locked(operation_id, &digest, committed_at) {
                let _ = self.abort_operation_locked(operation_id, &digest, committed_at);
                return Err(error);
            }
            Ok(record)
        })();
        lock.release()?;
        result
    }

    pub fn read_claim(&self, task_id: &str) -> Result<ClaimRecord, WorkspaceError> {
        let path = self.claim_path(task_id);
        let text = read_bounded_text(&path, INTERNAL_MAX_BYTES)?;
        let record = parse_claim(&text)?;
        if record.task_id != task_id {
            return Err(WorkspaceError::InvalidReceipt(
                "claim taskId mismatch".to_owned(),
            ));
        }
        let expected = mission_center_core::sha256_digest(
            format!(
                "claim\0{}\0{}\0{}\0{}",
                record.task_id, record.owner, record.fence, record.expires_at
            )
            .as_bytes(),
        );
        if record.digest != expected {
            return Err(WorkspaceError::InvalidReceipt(
                "claim digest mismatch".to_owned(),
            ));
        }
        Ok(record)
    }

    pub fn release_claim(
        &self,
        task_id: &str,
        owner: &str,
        fence: u64,
        operation_id: &str,
        committed_at: &str,
    ) -> Result<OperationOutcome, WorkspaceError> {
        if owner.is_empty() || owner.len() > OWNER_TOKEN_MAX_BYTES {
            return Err(WorkspaceError::ClaimRejected(
                "owner or timestamp is empty or too long".to_owned(),
            ));
        }
        validate_timestamp(committed_at).map_err(|_| {
            WorkspaceError::ClaimRejected("invalid committed_at timestamp".to_owned())
        })?;
        let digest = mission_center_core::sha256_digest(
            format!("release\0{task_id}\0{owner}\0{fence}").as_bytes(),
        );
        let lock = self.acquire_writer_lock(&format!("op:{operation_id}"))?;
        let result = (|| {
            let (_, tasks) = self.read_tasks()?;
            if !tasks.iter().any(|task| task.id == task_id) {
                return Err(WorkspaceError::ClaimRejected(format!(
                    "unknown task: {task_id}"
                )));
            }
            let receipt_path = self.operation_path(operation_id)?;
            let prior = if fs::symlink_metadata(&receipt_path).is_ok() {
                let receipt = read_bounded_text(&receipt_path, INTERNAL_MAX_BYTES)?;
                let prior = parse_operation_receipt(&receipt)?;
                if prior.operation_id != operation_id {
                    return Err(WorkspaceError::InvalidReceipt(
                        "receipt identity mismatch".to_owned(),
                    ));
                }
                if prior.status == "committed" {
                    if prior.digest == digest {
                        return Ok(OperationOutcome::Replay);
                    }
                    return Err(WorkspaceError::Conflict(operation_id.to_owned()));
                }
                Some(prior)
            } else {
                None
            };
            let current = match self.read_claim(task_id) {
                Ok(current) => current,
                Err(WorkspaceError::NotFound { .. })
                    if prior
                        .as_ref()
                        .is_some_and(|receipt| receipt.digest == digest) =>
                {
                    if prior
                        .as_ref()
                        .is_some_and(|receipt| receipt.status == "started")
                    {
                        self.commit_operation_locked(operation_id, &digest, committed_at)?;
                        return Ok(OperationOutcome::Committed);
                    }
                    if prior
                        .as_ref()
                        .is_some_and(|receipt| receipt.status == "aborted")
                    {
                        return Ok(OperationOutcome::Replay);
                    }
                    return Err(WorkspaceError::InvalidReceipt(
                        "unsupported release recovery state".to_owned(),
                    ));
                }
                Err(error) => return Err(error),
            };
            if current.owner != owner || current.fence != fence {
                return Err(WorkspaceError::ClaimRejected(
                    "owner/fence does not match".to_owned(),
                ));
            }
            if self.begin_operation_locked(operation_id, &digest, committed_at)?
                == OperationOutcome::Replay
            {
                return Ok(OperationOutcome::Replay);
            }
            let claim_path = self.claim_path(task_id);
            let claim_bytes = read_bounded(&claim_path, INTERNAL_MAX_BYTES)?;
            if let Err(error) = fs::remove_file(&claim_path) {
                if let Err(abort_error) =
                    self.abort_operation_locked(operation_id, &digest, committed_at)
                {
                    return Err(WorkspaceError::RecoveryUnknown(format!(
                        "claim removal failed: {error}; abort receipt failed: {abort_error}"
                    )));
                }
                return Err(WorkspaceError::Io(error));
            }
            if let Err(error) = self.commit_operation_locked(operation_id, &digest, committed_at) {
                // Keep release replayable if the receipt commit fails after
                // deleting the claim.  Restoring the exact bytes lets a
                // later retry resume from the aborted receipt safely.
                let restore = self.atomic_write(&claim_path, &claim_bytes);
                let abort = self.abort_operation_locked(operation_id, &digest, committed_at);
                if let Err(restore_error) = restore {
                    return Err(WorkspaceError::RecoveryUnknown(format!(
                        "receipt commit failed: {error}; claim restore failed: {restore_error}"
                    )));
                }
                if let Err(abort_error) = abort {
                    return Err(WorkspaceError::RecoveryUnknown(format!(
                        "receipt commit failed: {error}; abort receipt failed: {abort_error}"
                    )));
                }
                return Err(error);
            }
            Ok(OperationOutcome::Committed)
        })();
        lock.release()?;
        result
    }

    pub fn normalize_tasks(
        &self,
        operation_id: &str,
        timestamp: &str,
    ) -> Result<WriteOutcome, WorkspaceError> {
        validate_timestamp(timestamp)?;
        let lock = self.acquire_writer_lock(&format!("op:{operation_id}"))?;
        let result = (|| {
            let source = read_bounded(&self.tasks_path(), TASKS_MAX_BYTES)?;
            let digest = sha256_digest(&canonicalize_hash_bytes(&source));
            if self.begin_operation_locked(operation_id, &digest, timestamp)?
                == OperationOutcome::Replay
            {
                return Ok(WriteOutcome::Unchanged);
            }
            let text =
                String::from_utf8(source.clone()).map_err(|_| WorkspaceError::InvalidUtf8 {
                    path: self.tasks_path(),
                })?;
            let normalized = normalize_markdown_tasks(&text)?;
            let outcome = self.atomic_write(&self.tasks_path(), normalized.as_bytes());
            match outcome {
                Ok(outcome) => {
                    if let Err(error) =
                        self.commit_operation_locked(operation_id, &digest, timestamp)
                    {
                        let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                        return Err(error);
                    }
                    Ok(outcome)
                }
                Err(error) => {
                    let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                    Err(error)
                }
            }
        })();
        lock.release()?;
        result
    }

    /// Apply exactly one lifecycle transition to the canonical tasks table.
    ///
    /// The canonical task ID and target status are part of the operation
    /// identity, so replaying the same operation is deterministic. Only the
    /// status cell of the selected row is rewritten; all other rows and
    /// surrounding document content remain untouched.
    pub fn transition_task(
        &self,
        operation_id: &str,
        task_id: &str,
        target: TaskStatus,
        timestamp: &str,
    ) -> Result<WriteOutcome, WorkspaceError> {
        self.transition_task_with_status(operation_id, task_id, target, timestamp)
            .map(|result| result.outcome)
    }

    pub fn transition_task_with_status(
        &self,
        operation_id: &str,
        task_id: &str,
        target: TaskStatus,
        timestamp: &str,
    ) -> Result<TransitionResult, WorkspaceError> {
        validate_timestamp(timestamp)?;
        if task_id.trim().is_empty() || task_id.len() > 128 {
            return Err(WorkspaceError::ClaimRejected(
                "task id is empty or too long".to_owned(),
            ));
        }
        let lock = self.acquire_writer_lock(&format!("op:{operation_id}"))?;
        let result = (|| {
            let source = read_bounded(&self.tasks_path(), TASKS_MAX_BYTES)?;
            let text =
                String::from_utf8(source.clone()).map_err(|_| WorkspaceError::InvalidUtf8 {
                    path: self.tasks_path(),
                })?;
            let tasks = parse_tasks_markdown(&text)?;
            let matches = tasks
                .iter()
                .enumerate()
                .filter(|(_, task)| task.id.eq_ignore_ascii_case(task_id))
                .collect::<Vec<_>>();
            let Some((_, selected)) = matches.first().copied() else {
                return Err(WorkspaceError::ClaimRejected(format!(
                    "unknown task: {task_id}"
                )));
            };
            if matches.len() > 1 {
                return Err(WorkspaceError::ClaimRejected(
                    "task id is ambiguous".to_owned(),
                ));
            }
            let mut proposed = selected.clone();
            transition_status(&mut proposed, target)?;
            let passport = if target == TaskStatus::Done {
                let passport = self.read_completion_passport(&selected.id)?;
                self.validate_completion_passport(&passport, selected)?;
                None
            } else if selected.status == TaskStatus::Done && target == TaskStatus::InProgress {
                match self.read_completion_passport(&selected.id) {
                    Ok(passport) => {
                        if passport.get("status").and_then(Value::as_str) == Some("superseded") {
                            None
                        } else {
                            self.validate_completion_passport(&passport, selected)?;
                            Some((self.completion_passport_path(&selected.id)?, passport))
                        }
                    }
                    Err(WorkspaceError::NotFound { .. }) => None,
                    Err(error) => return Err(error),
                }
            } else {
                None
            };
            let digest = sha256_digest(
                format!("transition\0{}\0{}", selected.id, target.as_str()).as_bytes(),
            );
            if self.begin_operation_locked(operation_id, &digest, timestamp)?
                == OperationOutcome::Replay
            {
                return Ok(TransitionResult {
                    outcome: WriteOutcome::Unchanged,
                    from: selected.status,
                    to: target,
                });
            }
            let updated = match transition_markdown_tasks(&text, task_id, target) {
                Ok(updated) => updated,
                Err(error) => {
                    let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                    return Err(error);
                }
            };
            let outcome = self.atomic_write(&self.tasks_path(), updated.as_bytes());
            match outcome {
                Ok(outcome) => {
                    if let Some((path, mut passport)) = passport {
                        let Some(object) = passport.as_object_mut() else {
                            let _ = self.atomic_write(&self.tasks_path(), source.as_slice());
                            let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                            return Err(WorkspaceError::ClaimRejected(
                                "completion passport must be an object".to_owned(),
                            ));
                        };
                        object.insert("status".to_owned(), Value::String("superseded".to_owned()));
                        let superseded = serde_json::to_vec(&passport).map_err(|error| {
                            WorkspaceError::ClaimRejected(format!(
                                "cannot serialize superseded completion passport: {error}"
                            ))
                        })?;
                        if let Err(error) = atomic_write_scoped(&path, &self.root, &superseded) {
                            let _ = self.atomic_write(&self.tasks_path(), source.as_slice());
                            let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                            return Err(error);
                        }
                    }
                    match self.commit_operation_locked(operation_id, &digest, timestamp) {
                        Ok(_) => Ok(TransitionResult {
                            outcome,
                            from: selected.status,
                            to: target,
                        }),
                        Err(error) => {
                            let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                            Err(error)
                        }
                    }
                }
                Err(error) => {
                    let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                    Err(error)
                }
            }
        })();
        lock.release()?;
        result
    }

    pub fn write_snapshot(
        &self,
        operation_id: &str,
        timestamp: &str,
        note: Option<&str>,
    ) -> Result<WriteOutcome, WorkspaceError> {
        self.write_snapshot_with_options(
            operation_id,
            timestamp,
            SnapshotOptions {
                note: note.map(ToOwned::to_owned),
                ..SnapshotOptions::default()
            },
        )
    }

    pub fn write_snapshot_with_options(
        &self,
        operation_id: &str,
        timestamp: &str,
        options: SnapshotOptions,
    ) -> Result<WriteOutcome, WorkspaceError> {
        validate_timestamp(timestamp)?;
        if options.note.as_deref().is_some_and(|value| {
            value.len() > 280
                || value.contains(['\r', '\n'])
                || value.chars().any(char::is_control)
                || secret_like(value)
        }) {
            return Err(WorkspaceError::ClaimRejected(
                "snapshot contains secret-like data".to_owned(),
            ));
        }
        if options.hypotheses.len() != options.evidences.len() {
            return Err(WorkspaceError::ClaimRejected(
                "hypothesis and evidence must be supplied in matching pairs".to_owned(),
            ));
        }
        let supplied_attempts = options
            .attempts
            .iter()
            .map(sanitize_snapshot_attempt)
            .collect::<Result<Vec<_>, _>>()?;
        let new_diagnosis = options
            .hypotheses
            .iter()
            .zip(&options.evidences)
            .map(|(hypothesis, evidence)| sanitize_diagnosis_pair(hypothesis, evidence))
            .collect::<Result<Vec<_>, _>>()?;
        let verification_result = options.verification_result.as_deref();
        if verification_result.is_some_and(|value| !matches!(value, "pass" | "fail")) {
            return Err(WorkspaceError::ClaimRejected(
                "verification result must be pass or fail".to_owned(),
            ));
        }
        if options
            .verification_action
            .as_deref()
            .is_some_and(|value| !valid_verification_action(value))
        {
            return Err(WorkspaceError::ClaimRejected(
                "verification action is not a supported low-cost verification".to_owned(),
            ));
        }
        let lock = self.acquire_writer_lock(&format!("op:{operation_id}"))?;
        let result = (|| {
            let (_, tasks) = self.read_tasks()?;
            let snapshot_facts = self.canonical_snapshot_facts(&tasks)?;
            let chinese = self.snapshot_is_chinese()?;
            let labels = if chinese {
                (
                    "執行檢查點",
                    "建立時間",
                    "進行中任務",
                    "狀態",
                    "版本",
                    "指紋",
                    "依賴",
                    "驗證",
                    "恢復",
                    "目前沒有進行中任務；請從 canonical 任務清單重新選取。",
                )
            } else {
                (
                    "Execution Checkpoint",
                    "Captured at",
                    "Active task",
                    "Status",
                    "Revision",
                    "Fingerprint",
                    "Dependencies",
                    "Verification",
                    "Resume",
                    "No active task; resume from canonical task selection.",
                )
            };
            let prior = self
                .read_path_text(&self.snapshot_path(), SNAPSHOT_MAX_BYTES)?
                .unwrap_or_default();
            let mut attempts = read_recent_snapshot_attempts(&prior);
            attempts.extend(supplied_attempts.clone());
            let prior_gate = read_snapshot_gate(&prior);
            let mut diagnosis = read_diagnosis_evidence(&prior);
            let unseen = new_diagnosis
                .iter()
                .filter(|pair| !diagnosis.contains(pair))
                .cloned()
                .collect::<Vec<_>>();
            let mut gate_mode = retry_gate_mode(&attempts);
            if prior_gate == Some("verification_required") {
                gate_mode = "verification_required";
            }
            if verification_result.is_some() && prior_gate != Some("verification_required") {
                return Err(WorkspaceError::ClaimRejected(
                    "verification result is only valid after verification_required".to_owned(),
                ));
            }
            let verification_record = match verification_result {
                None => None,
                Some(result) => {
                    let action = options.verification_action.as_deref().ok_or_else(|| {
                        WorkspaceError::ClaimRejected(
                            "verification result requires a low-cost action and bounded evidence"
                                .to_owned(),
                        )
                    })?;
                    let evidence = options.verification_evidence.as_deref().ok_or_else(|| {
                        WorkspaceError::ClaimRejected(
                            "verification result requires a low-cost action and bounded evidence"
                                .to_owned(),
                        )
                    })?;
                    validate_snapshot_text(evidence, "verification evidence", true)?;
                    Some(json!({"action":action,"result":result,"evidence":evidence.trim()}))
                }
            };
            if verification_result == Some("pass") {
                gate_mode = "retry";
                attempts.clear();
            } else if verification_result == Some("fail") {
                gate_mode = "diagnosis";
            } else if gate_mode == "diagnosis" && !unseen.is_empty() {
                gate_mode = "verification_required";
                attempts.clear();
                diagnosis.extend(unseen);
                if diagnosis.len() > MAX_RECENT_ATTEMPTS {
                    diagnosis = diagnosis[diagnosis.len() - MAX_RECENT_ATTEMPTS..].to_vec();
                }
            }
            if attempts.len() > MAX_RECENT_ATTEMPTS {
                attempts = attempts[attempts.len() - MAX_RECENT_ATTEMPTS..].to_vec();
            }
            let metadata = format!(
                "- Retry gate: {gate_mode}\n- Recent attempts JSON: {}\n- Diagnosis evidence JSON: {}",
                serde_json::to_string(&attempts)
                    .map_err(|error| WorkspaceError::ClaimRejected(error.to_string()))?,
                serde_json::to_string(&diagnosis)
                    .map_err(|error| WorkspaceError::ClaimRejected(error.to_string()))?,
            );
            let resume = if snapshot_facts.state == "active" {
                if chinese {
                    format!("讀取 {} 的 canonical 任務與下一步", snapshot_facts.active)
                } else {
                    format!(
                        "Read canonical task and next action for {}",
                        snapshot_facts.active
                    )
                }
            } else {
                labels.9.to_owned()
            };
            let body = format!(
                "# {}\n\n- State: {}\n- {}: {timestamp}\n- {}: {}\n- {}: {}\n- {}: {}\n- {}: {}\n- {}: {}\n- {}: {}\n- {}: {}\n{metadata}\n- {}:\n{}{}",
                labels.0,
                snapshot_facts.state,
                labels.1,
                labels.2,
                snapshot_facts.active,
                labels.3,
                snapshot_facts.status,
                labels.4,
                snapshot_facts.revision,
                labels.5,
                snapshot_facts.fingerprint,
                labels.6,
                snapshot_facts.dependencies,
                labels.7,
                snapshot_facts.verification,
                labels.8,
                resume,
                if chinese {
                    "近期嘗試"
                } else {
                    "Recent attempts"
                },
                if attempts.is_empty() {
                    format!("  - {}\n", if chinese { "無" } else { "None" })
                } else {
                    attempts
                        .iter()
                        .filter_map(|attempt| {
                            Some(format!(
                                "  - {} | {}\n",
                                attempt.get("phase")?.as_str()?,
                                attempt.get("errorSignature")?.as_str()?
                            ))
                        })
                        .collect()
                },
                options
                    .note
                    .as_deref()
                    .map_or(String::new(), |value| format!("- Notes: {value}\n")),
            );
            let body = if let Some(record) = verification_record {
                format!("{body}- Verification evidence JSON: {}\n", record)
            } else {
                body
            };
            if body.len() > SNAPSHOT_MAX_BYTES as usize {
                return Err(WorkspaceError::TooLarge {
                    path: self.snapshot_path(),
                    limit: SNAPSHOT_MAX_BYTES,
                });
            }
            let digest = sha256_digest(body.as_bytes());
            if self.begin_operation_locked(operation_id, &digest, timestamp)?
                == OperationOutcome::Replay
            {
                return Ok(WriteOutcome::Unchanged);
            }
            let outcome = self.atomic_write(&self.snapshot_path(), body.as_bytes());
            match outcome {
                Ok(outcome) => {
                    match self.commit_operation_locked(operation_id, &digest, timestamp) {
                        Ok(_) => Ok(outcome),
                        Err(error) => {
                            let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                            Err(error)
                        }
                    }
                }
                Err(error) => {
                    let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                    Err(error)
                }
            }
        })();
        lock.release()?;
        result
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_pulse_full(
        &self,
        operation_id: &str,
        pulse_id: &str,
        task_id: &str,
        phase: &str,
        outcome: &str,
        next_action: &str,
        evidence_ref: &str,
        recorded_at: &str,
        budget_remaining: u64,
        causal_parent: Option<&str>,
    ) -> Result<OperationOutcome, WorkspaceError> {
        validate_rfc3339(recorded_at)?;
        validate_pulse_text(pulse_id, "pulseId", true, 128)?;
        validate_pulse_text(task_id, "taskId", true, 128)?;
        validate_pulse_text(phase, "phase", true, 128)?;
        validate_pulse_text(outcome, "outcome", true, 1024)?;
        validate_pulse_text(next_action, "nextAction", true, 1024)?;
        validate_pulse_text(evidence_ref, "evidenceRef", false, 512)?;
        if let Some(parent) = causal_parent {
            validate_pulse_text(parent, "causalParent", true, 128)?;
        }
        if secret_like(&format!("{phase} {outcome} {next_action} {evidence_ref}")) {
            return Err(WorkspaceError::ClaimRejected(
                "pulse contains secret-like data".to_owned(),
            ));
        }
        let lock = self.acquire_writer_lock(&format!("op:{operation_id}"))?;
        let result = (|| {
            let (_, tasks) = self.read_tasks()?;
            let selected = tasks
                .iter()
                .find(|task| task.id.eq_ignore_ascii_case(task_id));
            if selected.is_none() {
                return Err(WorkspaceError::ClaimRejected(format!(
                    "unknown task: {task_id}"
                )));
            }
            let path = self.mission_dir().join("execution-ledger.jsonl");
            let prior = match read_bounded(&path, 256 * 1024) {
                Ok(bytes) => bytes,
                Err(WorkspaceError::NotFound { .. }) => Vec::new(),
                Err(error) => return Err(error),
            };
            let prior_records = parse_pulse_ledger(&prior, &path)?;
            let record = PulseRecord {
                pulse_id: pulse_id.trim().to_owned(),
                task_id: task_id.trim().to_owned(),
                phase: phase.trim().to_owned(),
                outcome: outcome.trim().to_owned(),
                next_action: next_action.trim().to_owned(),
                evidence_ref: evidence_ref.trim().to_owned(),
                budget_remaining,
                causal_parent: causal_parent.map(str::trim).map(ToOwned::to_owned),
                recorded_at: recorded_at.to_owned(),
            };
            if let Some(existing) = prior_records.iter().find(|item| item.pulse_id == pulse_id) {
                if pulse_payload_equal(existing, &record) {
                    let digest = pulse_digest(&record);
                    if let Ok(receipt_text) =
                        read_bounded_text(&self.operation_path(operation_id)?, INTERNAL_MAX_BYTES)
                    {
                        let receipt = parse_operation_receipt(&receipt_text)?;
                        if receipt.digest != digest {
                            return Err(WorkspaceError::Conflict(operation_id.to_owned()));
                        }
                        if receipt.status == "started" {
                            self.commit_operation_locked(operation_id, &digest, recorded_at)?;
                        }
                    }
                    return Ok(OperationOutcome::Replay);
                }
                return Err(WorkspaceError::Conflict(
                    "pulseId already exists with different content".to_owned(),
                ));
            }
            if let Some(parent) = causal_parent {
                let parent_record = prior_records
                    .iter()
                    .find(|item| item.pulse_id == parent)
                    .ok_or_else(|| {
                        WorkspaceError::ClaimRejected("unknown causal parent".to_owned())
                    })?;
                if !parent_record.task_id.eq_ignore_ascii_case(task_id) {
                    return Err(WorkspaceError::ClaimRejected(
                        "causal parent must belong to the same task".to_owned(),
                    ));
                }
                if timestamp_value(&parent_record.recorded_at)? > timestamp_value(recorded_at)? {
                    return Err(WorkspaceError::ClaimRejected(
                        "causal parent must not be later than child".to_owned(),
                    ));
                }
            }
            let digest = pulse_digest(&record);
            if self.begin_operation_locked(operation_id, &digest, recorded_at)?
                == OperationOutcome::Replay
            {
                return Ok(OperationOutcome::Replay);
            }
            let line = format!(
                "{{\"schemaVersion\":\"1.0\",\"kind\":\"execution-pulse\",\"pulseId\":{},\"taskId\":{},\"phase\":{},\"outcome\":{},\"nextAction\":{},\"evidenceRef\":{},\"budgetRemaining\":{},\"causalParent\":{},\"recordedAt\":{}}}\n",
                json_quote(&record.pulse_id),
                json_quote(&record.task_id),
                json_quote(&record.phase),
                json_quote(&record.outcome),
                json_quote(&record.next_action),
                json_quote(&record.evidence_ref),
                budget_remaining,
                record
                    .causal_parent
                    .as_deref()
                    .map_or_else(|| "null".to_owned(), json_quote),
                json_quote(&record.recorded_at)
            );
            if prior.len() + line.len() > 256 * 1024 || line.len() > 4096 {
                let _ = self.abort_operation_locked(operation_id, &digest, recorded_at);
                return Err(WorkspaceError::TooLarge {
                    path,
                    limit: 256 * 1024,
                });
            }
            let mut next = prior;
            if !next.is_empty() && !next.ends_with(b"\n") {
                next.push(b'\n');
            }
            next.extend_from_slice(line.as_bytes());
            if let Err(error) = self.atomic_write(&path, &next) {
                let _ = self.abort_operation_locked(operation_id, &digest, recorded_at);
                return Err(error);
            }
            if let Err(error) = self.commit_operation_locked(operation_id, &digest, recorded_at) {
                let _ = self.abort_operation_locked(operation_id, &digest, recorded_at);
                return Err(error);
            }
            Ok(OperationOutcome::Committed)
        })();
        lock.release()?;
        result
    }

    pub fn handoff_json(&self, task_id: Option<&str>) -> Result<String, WorkspaceError> {
        let (_, tasks) = self.read_tasks()?;
        let ledger = self.mission_dir().join("execution-ledger.jsonl");
        let bytes = match read_bounded(&ledger, 256 * 1024) {
            Ok(bytes) => bytes,
            Err(WorkspaceError::NotFound { .. }) => Vec::new(),
            Err(error) => return Err(error),
        };
        let text = String::from_utf8(bytes).map_err(|_| WorkspaceError::InvalidUtf8 {
            path: ledger.clone(),
        })?;
        let all = parse_pulse_ledger(text.as_bytes(), &ledger)?;
        let requested_id = task_id.or_else(|| all.last().map(|record| record.task_id.as_str()));
        let selected =
            requested_id.and_then(|id| tasks.iter().find(|task| task.id.eq_ignore_ascii_case(id)));
        if requested_id.is_some() && selected.is_none() {
            return Err(WorkspaceError::ClaimRejected(
                "handoff task is absent from canonical tasks.md".to_owned(),
            ));
        }
        let records: Vec<&PulseRecord> = all
            .iter()
            .filter(|record| requested_id.is_some_and(|id| record.task_id.eq_ignore_ascii_case(id)))
            .collect();
        let latest = records.last();
        let action = selected
            .filter(|task| task.status != mission_center_core::TaskStatus::Done)
            .and_then(|_| latest.map(|value| value.next_action.as_str()));
        let found = selected.is_some() && latest.is_some();
        let chain = latest.map_or_else(Vec::new, |latest| {
            let mut chain = Vec::new();
            let mut current = Some(*latest);
            while let Some(record) = current {
                chain.push(record);
                current = record
                    .causal_parent
                    .as_deref()
                    .and_then(|parent| all.iter().find(|item| item.pulse_id == parent));
            }
            chain.reverse();
            chain
        });
        let canonical = selected.map_or_else(|| "null".to_owned(), canonical_task_json);
        let latest_json = latest.map_or_else(|| "null".to_owned(), |record| pulse_json(record));
        let task_json = selected.map_or_else(|| "null".to_owned(), |task| json_quote(&task.id));
        if !found {
            return Ok(format!(
                "{{\"schemaVersion\":\"1.0\",\"route\":\"handoff\",\"taskId\":{},\"found\":false,\"pulses\":[],\"bytes\":0,\"maxBytes\":8192,\"truncated\":false,\"content\":null}}",
                task_json
            ));
        }
        let make_body = |latest_json: &str, chain_json: &str, truncated: bool| {
            let history_field = if found {
                format!("\"causalChain\":[{}]", chain_json)
            } else {
                "\"pulses\":[]".to_owned()
            };
            format!(
                "{{\"schemaVersion\":\"1.0\",\"route\":\"handoff\",\"taskId\":{},\"found\":{},\"lifecycleSource\":\"tasks.md\",\"canonicalTask\":{},\"latestPulse\":{},\"nextAction\":{},\"executionNextAction\":{},\"nextActionSource\":{},\"executionOnly\":{},\"budgetRemaining\":{},\"evidenceRef\":{},\"causalParent\":{},{},\"truncated\":{}}}",
                task_json,
                if found { "true" } else { "false" },
                canonical,
                latest_json,
                action.map_or_else(|| "null".to_owned(), json_quote),
                action.map_or_else(|| "null".to_owned(), json_quote),
                action.map_or_else(|| "null".to_owned(), |_| json_quote("execution-pulse")),
                if action.is_some() { "true" } else { "false" },
                latest.map_or_else(
                    || "null".to_owned(),
                    |record| record.budget_remaining.to_string()
                ),
                latest.map_or_else(
                    || "null".to_owned(),
                    |record| json_quote(&record.evidence_ref)
                ),
                latest.map_or_else(
                    || "null".to_owned(),
                    |record| record
                        .causal_parent
                        .as_deref()
                        .map_or_else(|| "null".to_owned(), json_quote)
                ),
                history_field,
                if truncated { "true" } else { "false" }
            )
        };
        let mut chain_start = 0usize;
        let body = loop {
            let chain_json = chain[chain_start..]
                .iter()
                .map(|record| pulse_json(record))
                .collect::<Vec<_>>()
                .join(",");
            let candidate = make_body(&latest_json, &chain_json, chain_start > 0);
            if candidate.len() <= 8 * 1024 || chain_start + 1 >= chain.len() {
                break candidate;
            }
            chain_start += 1;
        };
        let base = body;
        let encoded_content = json_quote(&base);
        let mut body = format!(
            "{{{},\"bytes\":{},\"maxBytes\":8192,\"content\":{}}}",
            &base[1..base.len() - 1],
            base.len(),
            encoded_content
        );
        if body.len() > 8 * 1024 {
            let compact = latest.map_or_else(|| "null".to_owned(), |record| {
                format!(
                    "{{\"pulseId\":{},\"taskId\":{},\"nextAction\":{},\"budgetRemaining\":{},\"causalParent\":{}}}",
                    json_quote(&record.pulse_id), json_quote(&record.task_id),
                    json_quote(&record.next_action), record.budget_remaining,
                    record.causal_parent.as_deref().map_or_else(|| "null".to_owned(), json_quote))
            });
            let compact_base = make_body(&compact, "", true);
            body = format!(
                "{{{},\"bytes\":{},\"maxBytes\":8192,\"content\":{}}}",
                &compact_base[1..compact_base.len() - 1],
                compact_base.len(),
                json_quote(&compact_base)
            );
        }
        if body.len() > 8 * 1024 {
            return Err(WorkspaceError::TooLarge {
                path: ledger,
                limit: 8 * 1024,
            });
        }
        Ok(body)
    }

    pub fn closeout(
        &self,
        operation_id: &str,
        timestamp: &str,
        cycle: &str,
        archive: bool,
    ) -> Result<OperationOutcome, WorkspaceError> {
        self.closeout_with_details(operation_id, timestamp, cycle, archive, &[])
    }

    pub fn closeout_with_details(
        &self,
        operation_id: &str,
        timestamp: &str,
        cycle: &str,
        archive: bool,
        details: &[(&str, &str)],
    ) -> Result<OperationOutcome, WorkspaceError> {
        validate_timestamp(timestamp)?;
        if !details
            .iter()
            .any(|(key, value)| key.eq_ignore_ascii_case("summary") && !value.trim().is_empty())
        {
            return Err(WorkspaceError::ClaimRejected(
                "closeout requires a non-empty summary".to_owned(),
            ));
        }
        if cycle.is_empty()
            || cycle.len() > 64
            || !cycle
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
        {
            return Err(WorkspaceError::InvalidLocator(cycle.to_owned()));
        }
        let lock = self.acquire_writer_lock(&format!("closeout:{operation_id}"))?;
        let result = (|| {
            let (_, tasks) = self.read_tasks()?;
            let fingerprint = self.canonical_snapshot_facts(&tasks)?.fingerprint;
            if details.iter().any(|(key, value)| {
                key.is_empty()
                    || value.len() > 4096
                    || key.chars().any(char::is_control)
                    || value.chars().any(char::is_control)
                    || secret_like(key)
                    || secret_like(value)
            }) {
                return Err(WorkspaceError::ClaimRejected(
                    "closeout detail is invalid or secret-like".to_owned(),
                ));
            }
            let chinese = self.snapshot_is_chinese()?;
            let detail_lines = details
                .iter()
                .map(|(key, value)| {
                    let label = if chinese {
                        match key.to_ascii_lowercase().as_str() {
                            "summary" => "摘要",
                            "completed" => "已完成",
                            "unfinished" => "未完成",
                            "risks" => "風險",
                            "smoke tests" => "冒煙測試",
                            "retro" => "回顧",
                            _ => key,
                        }
                    } else {
                        key
                    };
                    format!("- {}: {}\n", label, value)
                })
                .collect::<String>();
            let body = format!(
                "# Closeout\n\n- Schema: 1.0\n- Cycle: {}\n- Closed at: {}\n- Source fingerprint: {}\n- Tasks: {}\n",
                cycle,
                timestamp,
                fingerprint,
                tasks.len()
            ) + &detail_lines;
            let digest = sha256_digest(format!("closeout\0{cycle}\0{body}").as_bytes());
            if self.begin_operation_locked(operation_id, &digest, timestamp)?
                == OperationOutcome::Replay
            {
                let current = self.mission_dir().join("closeout.md");
                if read_bounded(&current, SNAPSHOT_MAX_BYTES)? != body.as_bytes() {
                    return Err(WorkspaceError::Conflict(
                        "closeout artifact tampered".to_owned(),
                    ));
                }
                if archive {
                    let archived = self
                        .mission_dir()
                        .join("closeouts")
                        .join(format!("{cycle}.md"));
                    if read_bounded(&archived, SNAPSHOT_MAX_BYTES)? != body.as_bytes() {
                        return Err(WorkspaceError::Conflict(
                            "closeout archive tampered".to_owned(),
                        ));
                    }
                }
                return Ok(OperationOutcome::Replay);
            }
            let current = self.mission_dir().join("closeout.md");
            let archive_path = self
                .mission_dir()
                .join("closeouts")
                .join(format!("{cycle}.md"));
            let old_current = read_optional(current.clone(), SNAPSHOT_MAX_BYTES)?;
            let old_archive = if archive {
                match read_optional(archive_path.clone(), SNAPSHOT_MAX_BYTES)? {
                    Some(bytes) if bytes != body.as_bytes() => {
                        let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                        return Err(WorkspaceError::Conflict(
                            "immutable closeout archive differs".to_owned(),
                        ));
                    }
                    value => value,
                }
            } else {
                None
            };
            let mut archive_created = false;
            let write_result = (|| {
                self.atomic_write(&current, body.as_bytes())?;
                if archive && old_archive.is_none() {
                    ensure_directory(archive_path.parent().expect("closeouts parent"))?;
                    create_exclusive_file(&archive_path, body.as_bytes())?;
                    archive_created = true;
                }
                self.commit_operation_locked(operation_id, &digest, timestamp)?;
                Ok::<(), WorkspaceError>(())
            })();
            if let Err(error) = write_result {
                if let Some(bytes) = old_current {
                    let _ = self.atomic_write(&current, &bytes);
                } else {
                    let _ = fs::remove_file(&current);
                }
                if archive_created {
                    let _ = fs::remove_file(&archive_path);
                }
                let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                return Err(error);
            }
            Ok(OperationOutcome::Committed)
        })();
        lock.release()?;
        result
    }

    pub fn project_map(
        &self,
        operation_id: Option<&str>,
        timestamp: Option<&str>,
        dry_run: bool,
    ) -> Result<String, WorkspaceError> {
        if let Some(value) = timestamp {
            validate_timestamp(value)?;
        }
        let project = read_bounded(&self.mission_dir().join("project.md"), PROJECT_MAX_BYTES)?;
        let task_source = read_bounded(&self.tasks_path(), TASKS_MAX_BYTES)?;
        let task_text =
            String::from_utf8(task_source.clone()).map_err(|_| WorkspaceError::InvalidUtf8 {
                path: self.tasks_path(),
            })?;
        let tasks = parse_tasks_markdown(&task_text)?;
        let project_lf = canonicalize_hash_bytes(&project);
        let tasks_lf = canonicalize_hash_bytes(&task_source);
        let source_fingerprint = project_map_source_fingerprint(&project_lf, &tasks_lf);
        let language = if String::from_utf8_lossy(&tasks_lf).contains("標題")
            || String::from_utf8_lossy(&tasks_lf).contains("狀態")
            || String::from_utf8_lossy(&project_lf).contains("# 任務")
        {
            "zh-TW"
        } else {
            "en"
        };
        let nodes = tasks
            .iter()
            .map(|task| {
                format!(
                    "{{\"id\":{},\"title\":{},\"type\":{},\"parentId\":{},\"priority\":{},\"status\":{},\"owner\":{},\"dependsOn\":[{}]}}",
                    json_quote(&task.id),
                    json_quote(&task.title),
                    json_quote(&task.kind),
                    if task.parent.is_empty() { "null".to_owned() } else { json_quote(&task.parent) },
                    json_quote(&task.priority),
                    json_quote(task.status.as_str()),
                    json_quote(&task.assignee),
                    task.dependencies
                        .iter()
                        .map(|id| json_quote(id))
                        .collect::<Vec<_>>()
                        .join(",")
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let mut by_status = Vec::new();
        for status in [
            "Backlog",
            "Ready",
            "In Progress",
            "Blocked",
            "Review",
            "Done",
        ] {
            let count = tasks
                .iter()
                .filter(|task| task.status.as_str() == status)
                .count();
            if count > 0 {
                by_status.push(format!("{}:{}", json_quote(status), count));
            }
        }
        let edges = tasks
            .iter()
            .flat_map(|task| {
                let mut values = Vec::new();
                if !task.parent.is_empty() {
                    values.push(format!(
                        "{{\"from\":{},\"to\":{},\"kind\":\"parent\"}}",
                        json_quote(&task.parent),
                        json_quote(&task.id)
                    ));
                }
                values.extend(task.dependencies.iter().map(|dependency| {
                    format!(
                        "{{\"from\":{},\"to\":{},\"kind\":\"dependsOn\"}}",
                        json_quote(dependency),
                        json_quote(&task.id)
                    )
                }));
                values
            })
            .collect::<Vec<_>>()
            .join(",");
        let generated_at = timestamp.unwrap_or("1970-01-01T00:00:00Z");
        let generation = sha256_digest(format!("{source_fingerprint}\0{generated_at}").as_bytes());
        let project_text =
            String::from_utf8(project_lf.clone()).map_err(|_| WorkspaceError::InvalidUtf8 {
                path: self.mission_dir().join("project.md"),
            })?;
        let project_name = project_text
            .lines()
            .find_map(|line| line.strip_prefix("# "))
            .unwrap_or("MissionCenter");
        let project_goal = project_text
            .lines()
            .find_map(|line| {
                line.strip_prefix("- Goal: ")
                    .or_else(|| line.strip_prefix("- 目標: "))
            })
            .unwrap_or("");
        let json = format!(
            "{{\"schemaVersion\":\"1.0\",\"sourceFingerprint\":{},\"sources\":[\"MissionCenter/project.md\",\"MissionCenter/tasks.md\"],\"generatedAt\":{},\"language\":{},\"project\":{{\"name\":{},\"goal\":{}}},\"counts\":{{\"total\":{},\"byStatus\":{{{}}}}},\"nodes\":[{}],\"edges\":[{}],\"generation\":{}}}",
            json_quote(&source_fingerprint),
            json_quote(generated_at),
            json_quote(language),
            json_quote(project_name.trim()),
            json_quote(project_goal.trim()),
            tasks.len(),
            by_status.join(","),
            nodes,
            edges,
            json_quote(&generation)
        );
        if dry_run {
            return Ok(json);
        }
        let operation_id = operation_id.ok_or_else(|| {
            WorkspaceError::InvalidReceipt("project-map requires operation-id".to_owned())
        })?;
        let timestamp = timestamp.ok_or_else(|| {
            WorkspaceError::InvalidReceipt("project-map requires timestamp".to_owned())
        })?;
        let lock = self.acquire_writer_lock(&format!("project-map:{operation_id}"))?;
        let result = (|| {
            let digest = sha256_digest(json.as_bytes());
            if self.begin_operation_locked(operation_id, &digest, timestamp)?
                == OperationOutcome::Replay
            {
                self.validate_project_map_artifacts(&source_fingerprint, &generation)?;
                return Ok(json.clone());
            }
            let output_dir = self.root.join("output").join("mission-center-project-map");
            ensure_directory(&output_dir)?;
            let json_path = output_dir.join("project-map.json");
            let html_path = output_dir.join("project-map.html");
            let manifest_path = output_dir.join("project-map.manifest.json");
            let html = format!(
                "<!doctype html><meta charset=\"utf-8\"><title>Project Map</title><pre>{}</pre>\n",
                html_escape(&json)
            );
            let manifest = format!(
                "{{\"schemaVersion\":\"1.0\",\"status\":\"committed\",\"sourceFingerprint\":{},\"generation\":{},\"files\":{{\"project-map.json\":{},\"project-map.html\":{}}}}}",
                json_quote(&source_fingerprint),
                json_quote(&generation),
                json_quote(&sha256_digest(json.as_bytes())),
                json_quote(&sha256_digest(html.as_bytes()))
            );
            let old_json = read_optional(json_path.clone(), INTERNAL_MAX_BYTES)?;
            let old_html = read_optional(html_path.clone(), INTERNAL_MAX_BYTES)?;
            let old_manifest = read_optional(manifest_path.clone(), INTERNAL_MAX_BYTES)?;
            let current_project =
                read_bounded(&self.mission_dir().join("project.md"), PROJECT_MAX_BYTES)?;
            let current_tasks = read_bounded(&self.tasks_path(), TASKS_MAX_BYTES)?;
            let current_source = project_map_source_fingerprint(
                &canonicalize_hash_bytes(&current_project),
                &canonicalize_hash_bytes(&current_tasks),
            );
            if current_source != source_fingerprint {
                let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                return Err(WorkspaceError::Conflict("concurrent_mutation".to_owned()));
            }
            let write_result = (|| {
                atomic_write_scoped(&json_path, &output_dir, json.as_bytes())?;
                atomic_write_scoped(&html_path, &output_dir, html.as_bytes())?;
                let after_project =
                    read_bounded(&self.mission_dir().join("project.md"), PROJECT_MAX_BYTES)?;
                let after_tasks = read_bounded(&self.tasks_path(), TASKS_MAX_BYTES)?;
                let after_source = project_map_source_fingerprint(
                    &canonicalize_hash_bytes(&after_project),
                    &canonicalize_hash_bytes(&after_tasks),
                );
                if after_source != source_fingerprint {
                    return Err(WorkspaceError::Conflict("concurrent_mutation".to_owned()));
                }
                atomic_write_scoped(&manifest_path, &output_dir, manifest.as_bytes())?;
                self.commit_operation_locked(operation_id, &digest, timestamp)?;
                Ok::<(), WorkspaceError>(())
            })();
            if let Err(error) = write_result {
                for (path, bytes) in [
                    (json_path, old_json),
                    (html_path, old_html),
                    (manifest_path, old_manifest),
                ] {
                    if let Some(bytes) = bytes {
                        let _ = atomic_write_scoped(&path, &output_dir, &bytes);
                    } else {
                        let _ = fs::remove_file(path);
                    }
                }
                let _ = self.abort_operation_locked(operation_id, &digest, timestamp);
                return Err(error);
            }
            Ok(json)
        })();
        lock.release()?;
        result
    }

    pub fn verify_project_map(&self) -> Result<(), WorkspaceError> {
        let project = read_bounded(&self.mission_dir().join("project.md"), PROJECT_MAX_BYTES)?;
        let tasks = read_bounded(&self.tasks_path(), TASKS_MAX_BYTES)?;
        let fingerprint = project_map_source_fingerprint(
            &canonicalize_hash_bytes(&project),
            &canonicalize_hash_bytes(&tasks),
        );
        let manifest_path = self
            .root
            .join("output")
            .join("mission-center-project-map")
            .join("project-map.manifest.json");
        let manifest = read_bounded_text(&manifest_path, INTERNAL_MAX_BYTES)?;
        let generation = extract_json_string(&manifest, "generation").ok_or_else(|| {
            WorkspaceError::InvalidReceipt("manifest missing generation".to_owned())
        })?;
        let json_path = manifest_path
            .parent()
            .expect("project-map output parent")
            .join("project-map.json");
        let json = read_bounded_text(&json_path, INTERNAL_MAX_BYTES)?;
        let generated_at = extract_json_string(&json, "generatedAt").ok_or_else(|| {
            WorkspaceError::InvalidReceipt("project map missing generatedAt".to_owned())
        })?;
        let expected = self.project_map(None, Some(&generated_at), true)?;
        if json != expected {
            return Err(WorkspaceError::Conflict(
                "project map schema or canonical content mismatch".to_owned(),
            ));
        }
        self.validate_project_map_artifacts(&fingerprint, &generation)
    }

    fn validate_project_map_artifacts(
        &self,
        source_fingerprint: &str,
        generation: &str,
    ) -> Result<(), WorkspaceError> {
        let output_dir = self.root.join("output").join("mission-center-project-map");
        ensure_no_reparse(&output_dir)?;
        let json_path = output_dir.join("project-map.json");
        let html_path = output_dir.join("project-map.html");
        let manifest_path = output_dir.join("project-map.manifest.json");
        let json = read_bounded_text(&json_path, INTERNAL_MAX_BYTES)?;
        let html = read_bounded_text(&html_path, INTERNAL_MAX_BYTES)?;
        let manifest = read_bounded_text(&manifest_path, INTERNAL_MAX_BYTES)?;
        if !manifest.contains("\"schemaVersion\":\"1.0\"")
            || !manifest.contains("\"status\":\"committed\"")
            || !manifest.contains(&format!("\"sourceFingerprint\":\"{source_fingerprint}\""))
            || !manifest.contains(&format!("\"generation\":\"{generation}\""))
            || !json.contains(&format!("\"sourceFingerprint\":\"{source_fingerprint}\""))
            || !json.contains(&format!("\"generation\":\"{generation}\""))
        {
            return Err(WorkspaceError::Conflict(
                "project-map manifest is corrupt".to_owned(),
            ));
        }
        let json_hash = sha256_digest(json.as_bytes());
        let html_hash = sha256_digest(html.as_bytes());
        if !manifest.contains(&format!("\"project-map.json\":\"{json_hash}\""))
            || !manifest.contains(&format!("\"project-map.html\":\"{html_hash}\""))
        {
            return Err(WorkspaceError::Conflict(
                "project-map artifact hash mismatch".to_owned(),
            ));
        }
        Ok(())
    }
}

fn validate_pulse_text(
    value: &str,
    field: &str,
    required: bool,
    limit: usize,
) -> Result<(), WorkspaceError> {
    if (required && value.trim().is_empty()) || value.len() > limit {
        return Err(WorkspaceError::ClaimRejected(format!(
            "pulse {field} is empty or exceeds its bound"
        )));
    }
    if value
        .chars()
        .any(|character| character.is_control() && character != '\t')
        || secret_like(value)
    {
        return Err(WorkspaceError::ClaimRejected(format!(
            "pulse {field} contains forbidden content"
        )));
    }
    Ok(())
}

fn project_map_source_fingerprint(project: &[u8], tasks: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(project.len() + tasks.len() + 32);
    bytes.extend_from_slice(b"project.md\0");
    bytes.extend_from_slice(project);
    bytes.push(0);
    bytes.extend_from_slice(b"tasks.md\0");
    bytes.extend_from_slice(tasks);
    bytes.push(0);
    sha256_digest(&bytes)
}

fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

fn extract_json_string(text: &str, key: &str) -> Option<String> {
    let marker = format!("\"{key}\":\"");
    let start = text.find(&marker)? + marker.len();
    let rest = &text[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

fn pulse_digest(record: &PulseRecord) -> String {
    sha256_digest(
        format!(
            "pulse\0{}\0{}\0{}\0{}\0{}\0{}\0{}\0{}",
            record.pulse_id,
            record.task_id,
            record.phase,
            record.outcome,
            record.next_action,
            record.evidence_ref,
            record.budget_remaining,
            record.causal_parent.as_deref().unwrap_or("")
        )
        .as_bytes(),
    )
}

fn pulse_payload_equal(left: &PulseRecord, right: &PulseRecord) -> bool {
    left.pulse_id == right.pulse_id
        && left.task_id.eq_ignore_ascii_case(&right.task_id)
        && left.phase == right.phase
        && left.outcome == right.outcome
        && left.next_action == right.next_action
        && left.evidence_ref == right.evidence_ref
        && left.budget_remaining == right.budget_remaining
        && left.causal_parent == right.causal_parent
}

fn validate_snapshot_text(value: &str, field: &str, required: bool) -> Result<(), WorkspaceError> {
    if (required && value.trim().is_empty())
        || value.chars().count() > 280
        || value.chars().any(char::is_control)
        || secret_like(value)
    {
        return Err(WorkspaceError::ClaimRejected(format!(
            "{field} is empty, oversized, or secret-like"
        )));
    }
    Ok(())
}

fn sanitize_snapshot_attempt(value: &Value) -> Result<Value, WorkspaceError> {
    let Some(object) = value.as_object() else {
        return Err(WorkspaceError::ClaimRejected(
            "attempt must be an object".to_owned(),
        ));
    };
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "phase" | "errorSignature" | "hypothesis" | "evidence"
        )
    }) || !object.contains_key("phase")
        || !object.contains_key("errorSignature")
    {
        return Err(WorkspaceError::ClaimRejected(
            "attempt needs only phase, errorSignature, optional hypothesis/evidence".to_owned(),
        ));
    }
    let mut clean = serde_json::Map::new();
    for key in ["phase", "errorSignature", "hypothesis", "evidence"] {
        if let Some(raw) = object.get(key) {
            let text = raw.as_str().ok_or_else(|| {
                WorkspaceError::ClaimRejected(format!("attempt {key} must be a string"))
            })?;
            validate_snapshot_text(text, &format!("attempt {key}"), true)?;
            clean.insert(key.to_owned(), Value::String(text.trim().to_owned()));
        }
    }
    Ok(Value::Object(clean))
}

fn sanitize_diagnosis_pair(hypothesis: &str, evidence: &str) -> Result<Value, WorkspaceError> {
    validate_snapshot_text(hypothesis, "diagnosis hypothesis", true)?;
    validate_snapshot_text(evidence, "diagnosis evidence", true)?;
    Ok(json!({"hypothesis": hypothesis.trim(), "evidence": evidence.trim()}))
}

fn metadata_json_line<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .find_map(|line| line.strip_prefix(prefix).map(str::trim))
}

fn read_recent_snapshot_attempts(text: &str) -> Vec<Value> {
    let Some(raw) = metadata_json_line(text, "- Recent attempts JSON:") else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str(raw) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| sanitize_snapshot_attempt(item).ok())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .take(MAX_RECENT_ATTEMPTS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn read_diagnosis_evidence(text: &str) -> Vec<Value> {
    let Some(raw) = metadata_json_line(text, "- Diagnosis evidence JSON:") else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str(raw) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let object = item.as_object()?;
            let hypothesis = object.get("hypothesis")?.as_str()?.trim();
            let evidence = object.get("evidence")?.as_str()?.trim();
            sanitize_diagnosis_pair(hypothesis, evidence).ok()
        })
        .rev()
        .take(MAX_RECENT_ATTEMPTS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn read_snapshot_gate(text: &str) -> Option<&str> {
    metadata_json_line(text, "- Retry gate:")
        .filter(|value| matches!(*value, "retry" | "diagnosis" | "verification_required"))
}

fn retry_gate_mode(attempts: &[Value]) -> &'static str {
    let mut signatures = std::collections::HashMap::<&str, usize>::new();
    let mut phases = std::collections::HashMap::<&str, usize>::new();
    for attempt in attempts {
        if let Some(object) = attempt.as_object() {
            if let Some(value) = object.get("errorSignature").and_then(Value::as_str) {
                *signatures.entry(value).or_default() += 1;
            }
            if let Some(value) = object.get("phase").and_then(Value::as_str) {
                *phases.entry(value).or_default() += 1;
            }
        }
    }
    if signatures.values().any(|count| *count >= 2) || phases.values().any(|count| *count >= 3) {
        "diagnosis"
    } else {
        "retry"
    }
}

fn valid_verification_action(value: &str) -> bool {
    matches!(
        value,
        "unit_test"
            | "integration_test"
            | "config_validation"
            | "dry_run"
            | "local_reproduction"
            | "staging_smoke"
            | "read_only_query"
    )
}

fn pulse_json(record: &PulseRecord) -> String {
    format!(
        "{{\"schemaVersion\":\"1.0\",\"kind\":\"execution-pulse\",\"pulseId\":{},\"taskId\":{},\"phase\":{},\"outcome\":{},\"nextAction\":{},\"evidenceRef\":{},\"budgetRemaining\":{},\"causalParent\":{},\"recordedAt\":{}}}",
        json_quote(&record.pulse_id),
        json_quote(&record.task_id),
        json_quote(&record.phase),
        json_quote(&record.outcome),
        json_quote(&record.next_action),
        json_quote(&record.evidence_ref),
        record.budget_remaining,
        record
            .causal_parent
            .as_deref()
            .map_or_else(|| "null".to_owned(), json_quote),
        json_quote(&record.recorded_at),
    )
}

fn canonical_task_json(task: &Task) -> String {
    format!(
        "{{\"ID\":{},\"Title\":{},\"Priority\":{},\"Status\":{},\"Depends on\":{},\"Next action\":{},\"Verification\":{}}}",
        json_quote(&task.id),
        json_quote(&task.title),
        json_quote(&task.priority),
        json_quote(task.status.as_str()),
        json_quote(&task.dependencies.join(", ")),
        json_quote(&task.next_action),
        json_quote(&task.verification),
    )
}

fn parse_pulse_ledger(bytes: &[u8], path: &Path) -> Result<Vec<PulseRecord>, WorkspaceError> {
    let text = String::from_utf8(bytes.to_vec()).map_err(|_| WorkspaceError::InvalidUtf8 {
        path: path.to_path_buf(),
    })?;
    let mut records = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > 4096 {
            return Err(WorkspaceError::TooLarge {
                path: path.to_path_buf(),
                limit: 4096,
            });
        }
        records.push(parse_pulse_record(line)?);
    }
    let mut seen = Vec::new();
    for (index, record) in records.iter().enumerate() {
        if seen.iter().any(|id| id == &record.pulse_id) {
            return Err(WorkspaceError::InvalidReceipt(
                "duplicate pulseId".to_owned(),
            ));
        }
        if let Some(parent) = &record.causal_parent {
            let position = records
                .iter()
                .position(|item| &item.pulse_id == parent)
                .ok_or_else(|| WorkspaceError::ClaimRejected("unknown causal parent".to_owned()))?;
            if position >= index {
                return Err(WorkspaceError::ClaimRejected(
                    "causal parent must precede child".to_owned(),
                ));
            }
            let parent_record = &records[position];
            if !parent_record.task_id.eq_ignore_ascii_case(&record.task_id) {
                return Err(WorkspaceError::ClaimRejected(
                    "causal parent task mismatch".to_owned(),
                ));
            }
            if timestamp_value(&parent_record.recorded_at)? > timestamp_value(&record.recorded_at)?
            {
                return Err(WorkspaceError::ClaimRejected(
                    "causal parent timestamp is later".to_owned(),
                ));
            }
        }
        seen.push(record.pulse_id.clone());
    }
    Ok(records)
}

fn parse_pulse_record(text: &str) -> Result<PulseRecord, WorkspaceError> {
    let characters: Vec<char> = text.chars().collect();
    let mut index = 0;
    skip_json_whitespace(&characters, &mut index);
    expect_json_char(&characters, &mut index, '{')?;
    let mut fields: Vec<(String, String, bool)> = Vec::new();
    loop {
        skip_json_whitespace(&characters, &mut index);
        if characters.get(index) == Some(&'}') {
            if fields.is_empty() {
                return Err(WorkspaceError::InvalidReceipt(
                    "empty pulse object".to_owned(),
                ));
            }
            index += 1;
            break;
        }
        let key = parse_json_string(&characters, &mut index)?;
        if !matches!(
            key.as_str(),
            "schemaVersion"
                | "kind"
                | "pulseId"
                | "taskId"
                | "phase"
                | "outcome"
                | "nextAction"
                | "evidenceRef"
                | "budgetRemaining"
                | "causalParent"
                | "recordedAt"
        ) || fields.iter().any(|(known, _, _)| known == &key)
        {
            return Err(WorkspaceError::InvalidReceipt(
                "unknown or duplicate pulse field".to_owned(),
            ));
        }
        skip_json_whitespace(&characters, &mut index);
        expect_json_char(&characters, &mut index, ':')?;
        if key == "budgetRemaining" {
            fields.push((
                key,
                parse_json_number(&characters, &mut index)?.to_string(),
                false,
            ));
        } else if key == "causalParent" && characters.get(index) == Some(&'n') {
            for expected in ['n', 'u', 'l', 'l'] {
                expect_json_char(&characters, &mut index, expected)?;
            }
            fields.push((key, String::new(), true));
        } else {
            fields.push((key, parse_json_string(&characters, &mut index)?, false));
        }
        skip_json_whitespace(&characters, &mut index);
        match characters.get(index) {
            Some(',') => {
                index += 1;
                skip_json_whitespace(&characters, &mut index);
                if characters.get(index) == Some(&'}') {
                    return Err(WorkspaceError::InvalidReceipt(
                        "trailing comma in pulse".to_owned(),
                    ));
                }
            }
            Some('}') => {
                index += 1;
                break;
            }
            _ => {
                return Err(WorkspaceError::InvalidReceipt(
                    "invalid pulse separator".to_owned(),
                ));
            }
        }
    }
    skip_json_whitespace(&characters, &mut index);
    if index != characters.len() || fields.len() != 11 {
        return Err(WorkspaceError::InvalidReceipt(
            "missing pulse field".to_owned(),
        ));
    }
    let value = |name: &str| {
        fields
            .iter()
            .find_map(|(key, value, null)| (key == name).then_some((value.clone(), *null)))
    };
    let get = |name: &str| {
        value(name)
            .map(|(v, _)| v)
            .ok_or_else(|| WorkspaceError::InvalidReceipt(format!("missing {name}")))
    };
    if get("schemaVersion")?.as_str() != "1.0" || get("kind")?.as_str() != "execution-pulse" {
        return Err(WorkspaceError::InvalidReceipt(
            "invalid pulse envelope".to_owned(),
        ));
    }
    let pulse_id = get("pulseId")?.trim().to_owned();
    let task_id = get("taskId")?.trim().to_owned();
    let phase = get("phase")?.trim().to_owned();
    let outcome = get("outcome")?.trim().to_owned();
    let next_action = get("nextAction")?.trim().to_owned();
    let evidence_ref = get("evidenceRef")?.trim().to_owned();
    let recorded_at = get("recordedAt")?.trim().to_owned();
    let budget_remaining = get("budgetRemaining")?
        .parse::<u64>()
        .map_err(|_| WorkspaceError::InvalidReceipt("invalid budgetRemaining".to_owned()))?;
    let parent =
        value("causalParent").and_then(|(value, null)| (!null).then_some(value.trim().to_owned()));
    validate_rfc3339(&recorded_at)?;
    validate_pulse_text(&pulse_id, "pulseId", true, 128)?;
    validate_pulse_text(&task_id, "taskId", true, 128)?;
    validate_pulse_text(&phase, "phase", true, 128)?;
    validate_pulse_text(&outcome, "outcome", true, 1024)?;
    validate_pulse_text(&next_action, "nextAction", true, 1024)?;
    validate_pulse_text(&evidence_ref, "evidenceRef", false, 512)?;
    if let Some(parent) = &parent {
        validate_pulse_text(parent, "causalParent", true, 128)?;
    }
    Ok(PulseRecord {
        pulse_id,
        task_id,
        phase,
        outcome,
        next_action,
        evidence_ref,
        budget_remaining,
        causal_parent: parent,
        recorded_at,
    })
}

fn secret_like(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    for marker in [
        "password",
        "secret",
        "token",
        "authorization",
        "api key",
        "api_key",
        "api-key",
    ] {
        let mut rest = lower.as_str();
        while let Some(index) = rest.find(marker) {
            let boundary = index == 0
                || rest[..index]
                    .chars()
                    .next_back()
                    .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_');
            let suffix = &rest[index + marker.len()..];
            let trimmed =
                suffix.trim_start_matches(|character: char| character.is_ascii_whitespace());
            if boundary
                && (trimmed.starts_with(':') || trimmed.starts_with('='))
                && trimmed[1..].trim_start().chars().next().is_some()
            {
                return true;
            }
            rest = suffix;
        }
    }
    if lower.contains("-----begin ") {
        return true;
    }
    for (index, _) in lower.match_indices("bearer") {
        let boundary = index == 0
            || lower[..index]
                .chars()
                .next_back()
                .is_none_or(|character| !character.is_ascii_alphanumeric() && character != '_');
        if boundary {
            let token = lower[index + "bearer".len()..]
                .trim_start_matches(|character: char| character.is_ascii_whitespace());
            if token.len() >= 8
                && token
                    .chars()
                    .take_while(|character| {
                        character.is_ascii_alphanumeric() || ".~+/=-".contains(*character)
                    })
                    .count()
                    >= 8
            {
                return true;
            }
        }
    }
    lower.split_whitespace().any(|word| {
        let mut segments = word.split('.');
        let jwt = word.starts_with("eyj")
            && segments.clone().count() == 3
            && segments.all(|segment| {
                segment.len() >= 5
                    && segment.chars().all(|character| {
                        character.is_ascii_alphanumeric() || "_-".contains(character)
                    })
            });
        jwt || word.starts_with("sk-")
            || word.starts_with("ghp-")
            || word.starts_with("xoxb-")
            || word.starts_with("xoxp-")
    })
}

fn transition_markdown_tasks(
    text: &str,
    task_id: &str,
    target: TaskStatus,
) -> Result<String, WorkspaceError> {
    let mut lines = text
        .split_inclusive('\n')
        .map(ToOwned::to_owned)
        .collect::<Vec<String>>();
    if lines.is_empty() {
        lines.push(text.to_owned());
    }
    let content_lines = lines
        .iter()
        .map(|line| strip_line_ending(line))
        .collect::<Vec<_>>();
    let tables = locate_task_table_rows(&content_lines)?;
    let mut matched = 0usize;
    for table in tables {
        for &line_index in &table.row_lines {
            let line = &mut lines[line_index];
            let content = strip_line_ending(line);
            let cells = split_cells(content)?;
            if cells.len() != table.headers.len()
                || !cells[table.id_index].eq_ignore_ascii_case(task_id)
            {
                continue;
            }
            matched += 1;
            let prefix_len = content.len() - content.trim_start().len();
            let prefix = &content[..prefix_len];
            let ending = line_ending(line);
            let mut updated = cells;
            updated[table.status_index] = target.as_str().to_owned();
            let row = updated
                .iter()
                .map(|cell| escape_md_cell(cell))
                .collect::<Vec<_>>()
                .join(" | ");
            *line = format!("{prefix}| {row} |{ending}");
        }
    }
    if matched == 0 {
        return Err(WorkspaceError::ClaimRejected(format!(
            "unknown task: {task_id}"
        )));
    }
    if matched > 1 {
        return Err(WorkspaceError::ClaimRejected(
            "task id is ambiguous".to_owned(),
        ));
    }
    Ok(lines.concat())
}

fn strip_line_ending(line: &str) -> &str {
    line.strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .or_else(|| line.strip_suffix('\r'))
        .unwrap_or(line)
}

fn line_ending(line: &str) -> &str {
    if line.ends_with("\r\n") {
        "\r\n"
    } else if line.ends_with('\n') {
        "\n"
    } else if line.ends_with('\r') {
        "\r"
    } else {
        ""
    }
}

fn normalize_markdown_tasks(text: &str) -> Result<String, WorkspaceError> {
    let mut lines: Vec<String> = text.lines().map(ToOwned::to_owned).collect();
    let start = lines
        .iter()
        .position(|line| line.starts_with('|'))
        .ok_or(WorkspaceError::Core(CoreError::MissingTable))?;
    if start + 2 > lines.len() {
        return Err(WorkspaceError::Core(CoreError::MissingTable));
    }
    let headers = split_cells(&lines[start])?;
    let separator = split_cells(&lines[start + 1])?;
    if headers.len() != separator.len() {
        return Err(WorkspaceError::Core(CoreError::InvalidHeader));
    }
    let canonical = |header: &str| match header.trim() {
        "Priority" | "優先級" => Some("priority"),
        "Status" | "狀態" => Some("status"),
        "Labels" | "標籤" => Some("labels"),
        _ => None,
    };
    let indexes: Vec<Option<&str>> = headers.iter().map(|header| canonical(header)).collect();
    let mut changed = false;
    for line in lines.iter_mut().skip(start + 2) {
        if !line.starts_with('|') {
            break;
        }
        let cells = split_cells(line)?;
        if cells.len() != headers.len() {
            continue;
        }
        let before = cells.clone();
        let mut cells = cells;
        for (index, kind) in indexes.iter().enumerate() {
            cells[index] = match kind {
                Some("status") => normalize_status(&cells[index]),
                Some("priority") => normalize_priority(&cells[index]),
                Some("labels") => normalize_labels(&cells[index]),
                _ => cells[index].clone(),
            };
        }
        if cells != before {
            *line = format!(
                "| {} |",
                cells
                    .iter()
                    .map(|cell| escape_md_cell(cell))
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
            changed = true;
        }
    }
    let mut result = lines.join("\n");
    if text.ends_with('\n') {
        result.push('\n');
    }
    if changed {
        Ok(result)
    } else {
        Ok(text.to_owned())
    }
}

fn escape_md_cell(value: &str) -> String {
    value.replace('\\', "\\\\").replace('|', "\\|")
}

fn normalize_status(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "todo" | "backlog" => "Backlog".to_owned(),
        "ready" => "Ready".to_owned(),
        "doing" | "in progress" => "In Progress".to_owned(),
        "blocked" => "Blocked".to_owned(),
        "review" => "Review".to_owned(),
        "done" => "Done".to_owned(),
        "" => "Backlog".to_owned(),
        _ => value.trim().to_owned(),
    }
}

fn normalize_priority(value: &str) -> String {
    match value.trim().to_ascii_lowercase().as_str() {
        "urgent" | "critical" | "p0" => "P0".to_owned(),
        "high" | "p1" => "P1".to_owned(),
        "medium" | "normal" | "p2" => "P2".to_owned(),
        "low" | "p3" => "P3".to_owned(),
        "" => "P2".to_owned(),
        _ => value.trim().to_owned(),
    }
}

fn normalize_labels(value: &str) -> String {
    let mut labels = Vec::new();
    for raw in value.split([',', ';']) {
        let label = raw.trim().to_ascii_lowercase();
        if !label.is_empty() && !labels.contains(&label) {
            labels.push(label);
        }
    }
    labels.join(", ")
}

fn ensure_directory(path: &Path) -> Result<(), WorkspaceError> {
    if fs::symlink_metadata(path).is_ok() {
        ensure_no_reparse(path)?;
        if !path.is_dir() {
            return Err(WorkspaceError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("not a directory: {}", path.display()),
            )));
        }
        return Ok(());
    }
    if let Some(parent) = path.parent()
        && parent != path
    {
        ensure_directory(parent)?;
    }
    fs::create_dir(path).map_err(WorkspaceError::Io)?;
    ensure_no_reparse(path)
}

fn atomic_write_scoped(target: &Path, scope: &Path, bytes: &[u8]) -> Result<(), WorkspaceError> {
    if !target.starts_with(scope) {
        return Err(WorkspaceError::UnsafePath {
            path: target.to_path_buf(),
        });
    }
    ensure_no_reparse(scope)?;
    let parent = target.parent().ok_or_else(|| WorkspaceError::UnsafePath {
        path: target.to_path_buf(),
    })?;
    ensure_no_reparse(parent)?;
    if let Ok(metadata) = fs::symlink_metadata(target) {
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(WorkspaceError::UnsafePath {
                path: target.to_path_buf(),
            });
        }
        if read_bounded(target, (bytes.len() as u64).max(INTERNAL_MAX_BYTES))? == bytes {
            return Ok(());
        }
    }
    let name = target
        .file_name()
        .ok_or_else(|| WorkspaceError::UnsafePath {
            path: target.to_path_buf(),
        })?;
    let temporary = target.with_file_name(format!(
        ".{}.tmp-{}",
        name.to_string_lossy(),
        unique_nonce()
    ));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.flush()?;
        sync_all_portable(&file)?;
        fs::rename(&temporary, target)?;
        #[cfg(unix)]
        {
            sync_all_portable(&File::open(scope)?)?;
        }
        Ok::<(), io::Error>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(WorkspaceError::Io)
}

fn create_exclusive_file(path: &Path, bytes: &[u8]) -> Result<(), WorkspaceError> {
    let mut file = match OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(WorkspaceError::Conflict(
                "immutable archive collision".to_owned(),
            ));
        }
        Err(error) => return Err(WorkspaceError::Io(error)),
    };
    if let Err(error) = (|| {
        file.write_all(bytes)?;
        file.flush()?;
        sync_all_portable(&file)?;
        Ok::<(), io::Error>(())
    })() {
        let _ = fs::remove_file(path);
        return Err(WorkspaceError::Io(error));
    }
    Ok(())
}

fn sync_all_portable(file: &File) -> io::Result<()> {
    match file.sync_all() {
        Err(error) if is_unsupported_sync_error(&error) => Ok(()),
        result => result,
    }
}

fn is_unsupported_sync_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported
    ) || (cfg!(target_os = "macos") && error.raw_os_error() == Some(45))
}

fn safe_id(value: &str) -> Result<&str, WorkspaceError> {
    if value.is_empty()
        || value.len() > OPERATION_ID_MAX_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(WorkspaceError::InvalidLocator(value.to_owned()));
    }
    Ok(value)
}

fn find_lock_tombstone(dir: &Path) -> Result<Option<PathBuf>, WorkspaceError> {
    let entries = fs::read_dir(dir).map_err(WorkspaceError::Io)?;
    for entry in entries {
        let entry = entry.map_err(WorkspaceError::Io)?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(LOCK_TOMBSTONE_PREFIX)
        {
            return Ok(Some(entry.path()));
        }
    }
    Ok(None)
}

fn unique_nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    format!(
        "{timestamp}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn json_quote(value: &str) -> String {
    let mut result = String::with_capacity(value.len() + 2);
    result.push('\"');
    for character in value.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '\"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\u{08}' => result.push_str("\\b"),
            '\u{0c}' => result.push_str("\\f"),
            character if character <= '\u{1f}' => {
                result.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => result.push(character),
        }
    }
    result.push('\"');
    result
}

fn parse_operation_receipt(text: &str) -> Result<OperationReceipt, WorkspaceError> {
    let characters: Vec<char> = text.chars().collect();
    let mut index = 0;
    skip_json_whitespace(&characters, &mut index);
    expect_json_char(&characters, &mut index, '{')?;
    let mut fields: Vec<(String, String)> = Vec::new();
    loop {
        skip_json_whitespace(&characters, &mut index);
        if characters.get(index) == Some(&'}') {
            index += 1;
            break;
        }
        let key = parse_json_string(&characters, &mut index)?;
        if !matches!(
            key.as_str(),
            "schemaVersion" | "operationId" | "digest" | "status" | "timestamp"
        ) || fields.iter().any(|(known, _)| known == &key)
        {
            return Err(WorkspaceError::InvalidReceipt(
                "unknown or duplicate field".to_owned(),
            ));
        }
        skip_json_whitespace(&characters, &mut index);
        expect_json_char(&characters, &mut index, ':')?;
        let value = parse_json_string(&characters, &mut index)?;
        fields.push((key, value));
        skip_json_whitespace(&characters, &mut index);
        match characters.get(index) {
            Some(',') => {
                index += 1;
                skip_json_whitespace(&characters, &mut index);
                if characters.get(index) == Some(&'}') {
                    return Err(WorkspaceError::InvalidReceipt(
                        "trailing comma in receipt".to_owned(),
                    ));
                }
            }
            Some('}') => {
                index += 1;
                break;
            }
            _ => {
                return Err(WorkspaceError::InvalidReceipt(
                    "invalid object separator".to_owned(),
                ));
            }
        }
    }
    skip_json_whitespace(&characters, &mut index);
    if index != characters.len() || fields.len() != 5 {
        return Err(WorkspaceError::InvalidReceipt(
            "missing receipt field".to_owned(),
        ));
    }
    let value = |name: &str| {
        fields
            .iter()
            .find_map(|(key, value)| (key == name).then_some(value.clone()))
    };
    if value("schemaVersion").as_deref() != Some("1.0") {
        return Err(WorkspaceError::InvalidReceipt(
            "unsupported schemaVersion".to_owned(),
        ));
    }
    let receipt = OperationReceipt {
        operation_id: value("operationId")
            .ok_or_else(|| WorkspaceError::InvalidReceipt("missing operationId".to_owned()))?,
        digest: value("digest")
            .ok_or_else(|| WorkspaceError::InvalidReceipt("missing digest".to_owned()))?,
        status: value("status")
            .ok_or_else(|| WorkspaceError::InvalidReceipt("missing status".to_owned()))?,
        timestamp: value("timestamp")
            .ok_or_else(|| WorkspaceError::InvalidReceipt("missing timestamp".to_owned()))?,
    };
    if !matches!(receipt.status.as_str(), "started" | "committed" | "aborted")
        || receipt.digest.is_empty()
        || safe_id(&receipt.operation_id).is_err()
        || validate_timestamp(&receipt.timestamp).is_err()
    {
        return Err(WorkspaceError::InvalidReceipt(
            "invalid receipt value".to_owned(),
        ));
    }
    Ok(receipt)
}

fn skip_json_whitespace(characters: &[char], index: &mut usize) {
    while characters
        .get(*index)
        .is_some_and(|character| character.is_ascii_whitespace())
    {
        *index += 1;
    }
}

fn expect_json_char(
    characters: &[char],
    index: &mut usize,
    expected: char,
) -> Result<(), WorkspaceError> {
    if characters.get(*index) == Some(&expected) {
        *index += 1;
        Ok(())
    } else {
        Err(WorkspaceError::InvalidReceipt(
            "invalid JSON syntax".to_owned(),
        ))
    }
}

fn parse_json_string(characters: &[char], index: &mut usize) -> Result<String, WorkspaceError> {
    expect_json_char(characters, index, '"')?;
    let mut result = String::new();
    while let Some(character) = characters.get(*index).copied() {
        *index += 1;
        match character {
            '"' => return Ok(result),
            '\\' => {
                let escaped = characters.get(*index).copied().ok_or_else(|| {
                    WorkspaceError::InvalidReceipt("truncated JSON escape".to_owned())
                })?;
                *index += 1;
                match escaped {
                    '"' | '\\' | '/' => result.push(escaped),
                    'b' => result.push('\u{08}'),
                    'f' => result.push('\u{0c}'),
                    'n' => result.push('\n'),
                    'r' => result.push('\r'),
                    't' => result.push('\t'),
                    'u' => {
                        let mut code = 0u32;
                        for _ in 0..4 {
                            let digit = characters.get(*index).copied().ok_or_else(|| {
                                WorkspaceError::InvalidReceipt(
                                    "truncated unicode escape".to_owned(),
                                )
                            })?;
                            *index += 1;
                            code = code * 16
                                + digit.to_digit(16).ok_or_else(|| {
                                    WorkspaceError::InvalidReceipt(
                                        "invalid unicode escape".to_owned(),
                                    )
                                })?;
                        }
                        result.push(char::from_u32(code).ok_or_else(|| {
                            WorkspaceError::InvalidReceipt("invalid unicode scalar".to_owned())
                        })?);
                    }
                    _ => {
                        return Err(WorkspaceError::InvalidReceipt(
                            "invalid JSON escape".to_owned(),
                        ));
                    }
                }
            }
            character if character.is_control() => {
                return Err(WorkspaceError::InvalidReceipt(
                    "control character in JSON string".to_owned(),
                ));
            }
            character => result.push(character),
        }
    }
    Err(WorkspaceError::InvalidReceipt(
        "unterminated JSON string".to_owned(),
    ))
}

fn claim_json(record: &ClaimRecord, timestamp: &str) -> String {
    format!(
        "{{\"schemaVersion\":\"1.0\",\"taskId\":{},\"owner\":{},\"fence\":{},\"expiresAt\":{},\"operationId\":{},\"digest\":{},\"timestamp\":{}}}",
        json_quote(&record.task_id),
        json_quote(&record.owner),
        record.fence,
        json_quote(&record.expires_at),
        json_quote(&record.operation_id),
        json_quote(&record.digest),
        json_quote(timestamp)
    )
}

fn parse_claim(text: &str) -> Result<ClaimRecord, WorkspaceError> {
    let characters: Vec<char> = text.chars().collect();
    let mut index = 0;
    skip_json_whitespace(&characters, &mut index);
    expect_json_char(&characters, &mut index, '{')?;
    let mut fields: Vec<(String, String)> = Vec::new();
    loop {
        skip_json_whitespace(&characters, &mut index);
        if characters.get(index) == Some(&'}') {
            index += 1;
            break;
        }
        let key = parse_json_string(&characters, &mut index)?;
        if !matches!(
            key.as_str(),
            "schemaVersion"
                | "taskId"
                | "owner"
                | "fence"
                | "expiresAt"
                | "operationId"
                | "digest"
                | "timestamp"
        ) || fields.iter().any(|(known, _)| known == &key)
        {
            return Err(WorkspaceError::InvalidReceipt(
                "unknown or duplicate claim field".to_owned(),
            ));
        }
        skip_json_whitespace(&characters, &mut index);
        expect_json_char(&characters, &mut index, ':')?;
        let value = if key == "fence" {
            parse_json_number(&characters, &mut index)?.to_string()
        } else {
            parse_json_string(&characters, &mut index)?
        };
        fields.push((key, value));
        skip_json_whitespace(&characters, &mut index);
        match characters.get(index) {
            Some(',') => {
                index += 1;
                skip_json_whitespace(&characters, &mut index);
                if characters.get(index) == Some(&'}') {
                    return Err(WorkspaceError::InvalidReceipt(
                        "trailing comma in claim".to_owned(),
                    ));
                }
            }
            Some('}') => {
                index += 1;
                break;
            }
            _ => {
                return Err(WorkspaceError::InvalidReceipt(
                    "invalid claim separator".to_owned(),
                ));
            }
        }
    }
    skip_json_whitespace(&characters, &mut index);
    if index != characters.len() || fields.len() != 8 {
        return Err(WorkspaceError::InvalidReceipt(
            "missing claim field".to_owned(),
        ));
    }
    let value = |name: &str| {
        fields
            .iter()
            .find_map(|(key, value)| (key == name).then_some(value.clone()))
    };
    if value("schemaVersion").as_deref() != Some("1.0") {
        return Err(WorkspaceError::InvalidReceipt(
            "unsupported claim schemaVersion".to_owned(),
        ));
    }
    let task_id = value("taskId")
        .ok_or_else(|| WorkspaceError::InvalidReceipt("missing taskId".to_owned()))?;
    let owner =
        value("owner").ok_or_else(|| WorkspaceError::InvalidReceipt("missing owner".to_owned()))?;
    let fence = value("fence")
        .ok_or_else(|| WorkspaceError::InvalidReceipt("missing fence".to_owned()))?
        .parse::<u64>()
        .map_err(|_| WorkspaceError::InvalidReceipt("invalid fence".to_owned()))?;
    let expires_at = value("expiresAt")
        .ok_or_else(|| WorkspaceError::InvalidReceipt("missing expiresAt".to_owned()))?;
    let operation_id = value("operationId")
        .ok_or_else(|| WorkspaceError::InvalidReceipt("missing operationId".to_owned()))?;
    let digest = value("digest")
        .ok_or_else(|| WorkspaceError::InvalidReceipt("missing digest".to_owned()))?;
    let timestamp = value("timestamp")
        .ok_or_else(|| WorkspaceError::InvalidReceipt("missing timestamp".to_owned()))?;
    if task_id.is_empty()
        || owner.is_empty()
        || owner.len() > OWNER_TOKEN_MAX_BYTES
        || safe_id(&operation_id).is_err()
        || digest.is_empty()
        || validate_timestamp(&expires_at).is_err()
        || validate_timestamp(&timestamp).is_err()
    {
        return Err(WorkspaceError::InvalidReceipt(
            "invalid claim value".to_owned(),
        ));
    }
    Ok(ClaimRecord {
        task_id,
        owner,
        fence,
        expires_at,
        operation_id,
        digest,
    })
}

fn parse_json_number(characters: &[char], index: &mut usize) -> Result<u64, WorkspaceError> {
    let start = *index;
    while characters
        .get(*index)
        .is_some_and(|character| character.is_ascii_digit())
    {
        *index += 1;
    }
    if start == *index || (*index - start > 1 && characters[start] == '0') {
        return Err(WorkspaceError::InvalidReceipt(
            "invalid JSON number".to_owned(),
        ));
    }
    characters[start..*index]
        .iter()
        .collect::<String>()
        .parse()
        .map_err(|_| WorkspaceError::InvalidReceipt("number out of range".to_owned()))
}

fn is_expired(now: &str, expires_at: &str) -> bool {
    match (timestamp_value(now), timestamp_value(expires_at)) {
        (Ok(now), Ok(expires)) => now >= expires,
        _ => false,
    }
}

fn validate_timestamp(value: &str) -> Result<(), WorkspaceError> {
    timestamp_value(value).map(|_| ())
}

fn validate_rfc3339(value: &str) -> Result<(), WorkspaceError> {
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(WorkspaceError::ClaimRejected(
            "pulse recordedAt must be RFC3339 date-time".to_owned(),
        ));
    }
    timestamp_value(value).map(|_| ())
}

fn timestamp_value(value: &str) -> Result<u128, WorkspaceError> {
    if value.is_empty() || value.len() > 64 {
        return Err(WorkspaceError::ClaimRejected(
            "timestamp must be epoch seconds or RFC3339".to_owned(),
        ));
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        let seconds = value.parse::<u64>().map_err(|_| {
            WorkspaceError::ClaimRejected("epoch timestamp out of range".to_owned())
        })?;
        return Ok(u128::from(seconds) * 1_000_000_000);
    }
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || !matches!(bytes[10], b'T' | b't')
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return Err(WorkspaceError::ClaimRejected(
            "invalid RFC3339 timestamp".to_owned(),
        ));
    }
    let year = decimal(bytes, 0, 4)?;
    let month = decimal(bytes, 5, 7)?;
    let day = decimal(bytes, 8, 10)?;
    let hour = decimal(bytes, 11, 13)?;
    let minute = decimal(bytes, 14, 16)?;
    let second = decimal(bytes, 17, 19)?;
    if !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return Err(WorkspaceError::ClaimRejected(
            "invalid RFC3339 timestamp".to_owned(),
        ));
    }
    let mut index = 19;
    let mut nanos = 0u32;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let digits = index - start;
        if digits == 0 || digits > 9 {
            return Err(WorkspaceError::ClaimRejected(
                "invalid RFC3339 fraction".to_owned(),
            ));
        }
        nanos = decimal(bytes, start, index)? as u32;
        for _ in digits..9 {
            nanos *= 10;
        }
    }
    let (offset_seconds, consumed) = match bytes.get(index) {
        Some(b'Z') | Some(b'z') => (0i64, index + 1),
        Some(b'+' | b'-') if bytes.len() >= index + 6 && bytes[index + 3] == b':' => {
            let offset_hour = decimal(bytes, index + 1, index + 3)?;
            let offset_minute = decimal(bytes, index + 4, index + 6)?;
            if offset_hour > 23 || offset_minute > 59 {
                return Err(WorkspaceError::ClaimRejected(
                    "invalid RFC3339 offset".to_owned(),
                ));
            }
            let sign = if bytes[index] == b'+' { 1 } else { -1 };
            (
                sign * (offset_hour as i64 * 3600 + offset_minute as i64 * 60),
                index + 6,
            )
        }
        _ => {
            return Err(WorkspaceError::ClaimRejected(
                "missing RFC3339 timezone".to_owned(),
            ));
        }
    };
    if consumed != bytes.len() {
        return Err(WorkspaceError::ClaimRejected(
            "invalid RFC3339 suffix".to_owned(),
        ));
    }
    let days = days_from_civil(year as i64, month as i64, day as i64);
    let seconds = days
        .checked_mul(86_400)
        .and_then(|value| {
            value.checked_add(hour as i64 * 3600 + minute as i64 * 60 + second as i64)
        })
        .and_then(|value| value.checked_sub(offset_seconds))
        .ok_or_else(|| WorkspaceError::ClaimRejected("timestamp out of range".to_owned()))?;
    if seconds < 0 {
        return Err(WorkspaceError::ClaimRejected(
            "timestamp before Unix epoch".to_owned(),
        ));
    }
    Ok(seconds as u128 * 1_000_000_000 + nanos as u128)
}

fn decimal(bytes: &[u8], start: usize, end: usize) -> Result<u64, WorkspaceError> {
    if end > bytes.len() || start >= end || !bytes[start..end].iter().all(u8::is_ascii_digit) {
        return Err(WorkspaceError::ClaimRejected(
            "invalid timestamp digits".to_owned(),
        ));
    }
    Ok(bytes[start..end]
        .iter()
        .fold(0u64, |value, digit| value * 10 + u64::from(digit - b'0')))
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = (if year >= 0 { year } else { year - 399 }) / 400;
    let yoe = year - era * 400;
    let month = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_temp_root(label: &str) -> PathBuf {
        let temp = std::env::temp_dir();
        #[cfg(target_os = "macos")]
        let temp = temp.canonicalize().expect("canonical temporary directory");
        temp.join(format!("mission-center-{label}-{}", unique_nonce()))
    }

    struct Fixture {
        workspace: MissionWorkspace,
        root: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fixture() -> Fixture {
        let root = test_temp_root("workspace");
        fs::create_dir_all(root.join(MISSION_DIRECTORY)).expect("fixture directory");
        fs::write(
            root.join(MISSION_DIRECTORY).join(TASKS_FILE),
            "| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | 測試 | Ready |\n",
        )
        .expect("fixture tasks");
        Fixture {
            workspace: MissionWorkspace::new(&root),
            root,
        }
    }

    #[test]
    fn layout_is_canonical() {
        let ws = MissionWorkspace::new("demo");
        assert_eq!(
            ws.tasks_path(),
            PathBuf::from("demo/MissionCenter/tasks.md")
        );
    }

    #[test]
    fn init_creates_contract_and_replays_without_rewriting() {
        let root = test_temp_root("init");
        let workspace = MissionWorkspace::new(&root);
        let first = workspace
            .init("init-test", "2026-08-29T00:00:00Z", "en", false)
            .expect("init");
        assert_eq!(first, WriteOutcome::Changed);
        for name in REQUIRED_INIT_FILES {
            assert!(
                root.join(MISSION_DIRECTORY).join(name).is_file(),
                "missing {name}"
            );
        }
        let tasks = fs::read(workspace.tasks_path()).expect("tasks");
        assert_eq!(
            workspace.operation_status("init-test").unwrap(),
            "committed"
        );
        assert_eq!(
            workspace
                .init("init-test", "2026-08-29T00:00:00Z", "en", false)
                .unwrap(),
            WriteOutcome::Unchanged
        );
        assert_eq!(fs::read(workspace.tasks_path()).unwrap(), tasks);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn init_force_preserves_existing_canonical_tasks() {
        let fixture = fixture();
        let before = fs::read(fixture.workspace.tasks_path()).unwrap();
        fixture
            .workspace
            .init("force-tasks", "2026-08-29T00:00:00Z", "en", true)
            .expect("forced scaffold");
        assert_eq!(fs::read(fixture.workspace.tasks_path()).unwrap(), before);
    }

    #[test]
    fn transition_writes_one_status_cell_and_enforces_review_gate() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        let timestamp = "2026-08-29T13:00:00Z";
        assert_eq!(
            workspace
                .transition_task("transition-1", "mc-1", TaskStatus::InProgress, timestamp)
                .unwrap(),
            WriteOutcome::Changed
        );
        let after_progress = fs::read_to_string(workspace.tasks_path()).unwrap();
        assert!(after_progress.contains("| MC-1 | 測試 | In Progress |"));
        assert_eq!(
            workspace
                .transition_task("transition-1", "MC-1", TaskStatus::InProgress, timestamp)
                .unwrap(),
            WriteOutcome::Unchanged
        );
        assert!(matches!(
            workspace.transition_task(
                "transition-skip",
                "MC-1",
                TaskStatus::Done,
                "2026-08-29T13:01:00Z",
            ),
            Err(WorkspaceError::Core(CoreError::InvalidTransition { .. }))
        ));
        assert_eq!(
            workspace
                .transition_task(
                    "transition-2",
                    "MC-1",
                    TaskStatus::Review,
                    "2026-08-29T13:02:00Z",
                )
                .unwrap(),
            WriteOutcome::Changed
        );
        let (_, review_tasks) = workspace.read_tasks().unwrap();
        let review_task = review_tasks.first().unwrap();
        let evidence_dir = fixture.root.join("output/mission-center-evidence");
        fs::create_dir_all(&evidence_dir).unwrap();
        fs::write(evidence_dir.join("smoke.md"), "pass").unwrap();
        let passport_dir = fixture.root.join("output/mission-center-passports");
        fs::create_dir_all(&passport_dir).unwrap();
        fs::write(
            passport_dir.join("MC-1.json"),
            serde_json::json!({
                "schemaVersion":"1.0",
                "artifactType":"completion-passport",
                "taskId":"MC-1",
                "taskDigest":mission_center_core::canonical_task_digest(review_task),
                "status":"current",
                "verification":{"result":"pass","evidenceRefs":["output/mission-center-evidence/smoke.md"]},
                "findings":[]
            }).to_string(),
        ).unwrap();
        assert_eq!(
            workspace
                .transition_task(
                    "transition-3",
                    "MC-1",
                    TaskStatus::Done,
                    "2026-08-29T13:03:00Z",
                )
                .unwrap(),
            WriteOutcome::Changed
        );
        assert!(
            fs::read_to_string(workspace.tasks_path())
                .unwrap()
                .contains("| MC-1 | 測試 | Done |")
        );
        assert_eq!(
            workspace
                .transition_task(
                    "transition-3",
                    "MC-1",
                    TaskStatus::Done,
                    "2026-08-29T13:03:00Z",
                )
                .unwrap(),
            WriteOutcome::Unchanged
        );
        assert_eq!(
            workspace
                .transition_task(
                    "transition-reopen",
                    "MC-1",
                    TaskStatus::InProgress,
                    "2026-08-29T13:04:00Z",
                )
                .unwrap(),
            WriteOutcome::Changed
        );
        assert!(
            fs::read_to_string(workspace.tasks_path())
                .unwrap()
                .contains("| MC-1 | 測試 | In Progress |")
        );
        assert!(
            fs::read_to_string(passport_dir.join("MC-1.json"))
                .unwrap()
                .contains("\"status\":\"superseded\"")
        );
    }

    #[test]
    fn done_gate_fails_closed_without_receipt_or_task_write() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        workspace
            .transition_task("gate-1", "MC-1", TaskStatus::InProgress, "1")
            .unwrap();
        workspace
            .transition_task("gate-2", "MC-1", TaskStatus::Review, "2")
            .unwrap();
        let before = fs::read(workspace.tasks_path()).unwrap();
        let result = workspace.transition_task("gate-3", "MC-1", TaskStatus::Done, "3");
        assert!(matches!(result, Err(WorkspaceError::NotFound { .. })));
        assert_eq!(fs::read(workspace.tasks_path()).unwrap(), before);
        assert!(!workspace.operation_path("gate-3").unwrap().exists());
    }

    #[test]
    fn done_gate_rejects_stale_critical_and_unsafe_passports() {
        for (digest, evidence, findings) in [
            (
                "0000000000000000000000000000000000000000000000000000000000000000",
                "output/mission-center-evidence/smoke.md",
                serde_json::json!([]),
            ),
            ("valid", "../secret.txt", serde_json::json!([])),
            (
                "valid",
                "output/mission-center-evidence/smoke.md",
                serde_json::json!([{"id":"F-1","severity":"Critical","disposition":"accepted"}]),
            ),
        ] {
            let fixture = fixture();
            let workspace = &fixture.workspace;
            workspace
                .transition_task("bad-1", "MC-1", TaskStatus::InProgress, "1")
                .unwrap();
            workspace
                .transition_task("bad-2", "MC-1", TaskStatus::Review, "2")
                .unwrap();
            let (_, tasks) = workspace.read_tasks().unwrap();
            let task = tasks.first().unwrap();
            let digest = if digest == "valid" {
                mission_center_core::canonical_task_digest(task)
            } else {
                digest.to_owned()
            };
            let evidence_path = fixture.root.join("output/mission-center-evidence");
            fs::create_dir_all(&evidence_path).unwrap();
            fs::write(evidence_path.join("smoke.md"), "pass").unwrap();
            let passport_path = fixture.root.join("output/mission-center-passports");
            fs::create_dir_all(&passport_path).unwrap();
            fs::write(
                passport_path.join("MC-1.json"),
                serde_json::json!({
                    "schemaVersion":"1.0","artifactType":"completion-passport",
                    "taskId":"MC-1","taskDigest":digest,"status":"current",
                    "verification":{"result":"pass","evidenceRefs":[evidence]},
                    "findings":findings
                })
                .to_string(),
            )
            .unwrap();
            let before = fs::read(workspace.tasks_path()).unwrap();
            assert!(
                workspace
                    .transition_task("bad-3", "MC-1", TaskStatus::Done, "3")
                    .is_err()
            );
            assert_eq!(fs::read(workspace.tasks_path()).unwrap(), before);
            assert!(!workspace.operation_path("bad-3").unwrap().exists());
        }
    }

    #[test]
    fn transition_preserves_crlf_and_escaped_cells() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        fs::write(
            workspace.tasks_path(),
            b"| ID | Title | Status | Notes |\r\n| --- | --- | --- | --- |\r\n| MC-1 | pipe \\| slash \\\\ | Ready | keep |\r\n",
        )
        .unwrap();
        assert_eq!(
            workspace
                .transition_task(
                    "transition-crlf",
                    "MC-1",
                    TaskStatus::InProgress,
                    "2026-08-29T13:05:00Z",
                )
                .unwrap(),
            WriteOutcome::Changed
        );
        let bytes = fs::read(workspace.tasks_path()).unwrap();
        assert!(bytes.windows(2).any(|pair| pair == b"\r\n"));
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("pipe \\| slash \\\\"));
        assert!(text.contains("| In Progress |"));
    }

    #[test]
    fn transition_updates_only_status_in_indented_multitable_continuation() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        let source = "  | ID | Title | Status | Notes |\n  | --- | --- | --- | --- |\n  | MC-1 | First | Ready | keep-1 |\n\n  | ID | Title | Status | Notes |\n  | --- | --- | --- | --- |\n  | MC-2 | Second | Ready | keep-2 |\n\n## Appendix\n| Note | Value |\n| --- | --- |\n| Keep | this |\n\n## Continuation\n  | MC-3 | Third | Ready | keep-3 |\n";
        fs::write(workspace.tasks_path(), source).unwrap();

        assert_eq!(
            workspace
                .transition_task(
                    "transition-multitable-continuation",
                    "MC-3",
                    TaskStatus::InProgress,
                    "2026-08-29T13:06:00Z",
                )
                .unwrap(),
            WriteOutcome::Changed
        );

        let expected = source.replace(
            "  | MC-3 | Third | Ready | keep-3 |",
            "  | MC-3 | Third | In Progress | keep-3 |",
        );
        assert_eq!(
            fs::read_to_string(workspace.tasks_path()).unwrap(),
            expected
        );
    }

    #[test]
    fn sync_derives_managed_progress_and_never_changes_tasks() {
        let fixture = fixture();
        let before = fs::read(fixture.workspace.tasks_path()).unwrap();
        assert_eq!(
            fixture
                .workspace
                .sync("sync-test", "2026-08-29T00:00:00Z")
                .unwrap(),
            WriteOutcome::Changed
        );
        let progress =
            fs::read_to_string(fixture.workspace.mission_dir().join("progress.md")).unwrap();
        assert!(progress.contains("0/1 tasks"));
        let brief = fs::read_to_string(fixture.workspace.mission_dir().join("brief.md")).unwrap();
        let working_set =
            fs::read_to_string(fixture.workspace.mission_dir().join("working-set.md")).unwrap();
        let focus = fs::read_to_string(fixture.workspace.mission_dir().join("focus.md")).unwrap();
        let workspace_fingerprint = fixture.workspace.fingerprint().unwrap();
        let task_fingerprint =
            mission_center_core::workspace_fingerprint(&[("tasks.md", Some(before.as_slice()))]);
        assert!(brief.contains(&format!("source-fingerprint={workspace_fingerprint}")));
        assert!(working_set.contains(&format!("source-fingerprint={task_fingerprint}")));
        assert!(focus.contains(&format!("source-fingerprint={task_fingerprint}")));
        assert!(working_set.contains("| MC-1 | 測試 |  | Ready |"));
        let daily_log =
            fs::read_to_string(fixture.workspace.mission_dir().join("daily-log.md")).unwrap();
        assert!(daily_log.contains("2026-08-29"));
        assert_eq!(fs::read(fixture.workspace.tasks_path()).unwrap(), before);
        assert_eq!(
            fixture
                .workspace
                .sync("sync-test", "2026-08-29T00:00:00Z")
                .unwrap(),
            WriteOutcome::Unchanged
        );
    }

    #[test]
    fn sync_advances_daily_date_without_losing_existing_events_or_crlf() {
        let fixture = fixture();
        let path = fixture.workspace.mission_dir().join("daily-log.md");
        fs::write(
            &path,
            "# 每日紀錄\r\n\r\n- 最後整理： 2026-08-28\r\n\r\n## 2026-08-28\r\n- 保留事件\r\n",
        )
        .unwrap();
        fixture
            .workspace
            .sync("sync-daily-date", "2026-08-29T08:00:00+08:00")
            .unwrap();
        let bytes = fs::read(&path).unwrap();
        assert!(bytes.windows(2).any(|pair| pair == b"\r\n"));
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("- 最後整理： 2026-08-29"));
        assert!(text.contains("- 保留事件"));
    }

    #[test]
    fn sync_does_not_replace_marker_text_embedded_in_daily_log_content() {
        let fixture = fixture();
        let path = fixture.workspace.mission_dir().join("daily-log.md");
        fs::write(
            &path,
            "# 每日紀錄\n\n- 事件文字提到 - Last organized: yesterday\n",
        )
        .unwrap();
        fixture
            .workspace
            .sync("sync-daily-embedded-marker", "2026-08-29T08:00:00+08:00")
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("- 事件文字提到 - Last organized: yesterday"));
        assert!(
            text.lines()
                .any(|line| line == "- Last organized: 2026-08-29")
        );
    }

    #[test]
    fn sync_rejects_post_transform_daily_log_overflow_without_overwriting() {
        let fixture = fixture();
        let path = fixture.workspace.mission_dir().join("daily-log.md");
        let mut original = "# 每日紀錄\n".as_bytes().to_vec();
        original.resize(DAILY_LOG_MAX_BYTES as usize, b'x');
        fs::write(&path, &original).unwrap();

        let error = fixture
            .workspace
            .sync("sync-daily-overflow", "2026-08-29T08:00:00+08:00")
            .unwrap_err();

        assert!(matches!(
            error,
            WorkspaceError::TooLarge { limit, .. } if limit == DAILY_LOG_MAX_BYTES
        ));
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn sync_all_done_refreshes_views_without_active_rows() {
        let fixture = fixture();
        fs::write(
            fixture.workspace.tasks_path(),
            "| ID | Title | Status | Priority |\n| --- | --- | --- | --- |\n| MC-1 | 測試 | Done | P0 |\n",
        )
        .unwrap();
        let before = fs::read(fixture.workspace.tasks_path()).unwrap();
        assert_eq!(
            fixture
                .workspace
                .sync("sync-all-done", "2026-08-29T00:00:00Z")
                .unwrap(),
            WriteOutcome::Changed
        );
        let working_set =
            fs::read_to_string(fixture.workspace.mission_dir().join("working-set.md")).unwrap();
        let focus = fs::read_to_string(fixture.workspace.mission_dir().join("focus.md")).unwrap();
        assert!(working_set.contains("all work complete"));
        assert!(!working_set.contains("| MC-1 |"));
        assert!(!focus.contains("| MC-1 |"));
        assert!(focus.contains("- Unfinished P0: 0"));
        assert_eq!(fs::read(fixture.workspace.tasks_path()).unwrap(), before);
        fixture
            .workspace
            .write_snapshot("snapshot-all-done", "2", None)
            .unwrap();
        let snapshot = fs::read_to_string(fixture.workspace.snapshot_path()).unwrap();
        assert!(snapshot.contains("- Resume:"));
        assert!(snapshot.contains("No active task; resume from canonical task selection."));
        assert!(!snapshot.ends_with("\n\n"));
    }

    #[test]
    fn sync_preserves_custom_unmanaged_derived_view() {
        let fixture = fixture();
        let custom = b"# Hand-authored brief\n\nKeep this note.\n";
        fs::write(fixture.workspace.mission_dir().join("brief.md"), custom).unwrap();
        let before = fs::read(fixture.workspace.tasks_path()).unwrap();
        fixture
            .workspace
            .sync("sync-custom-view", "2026-08-29T00:00:00Z")
            .unwrap();
        assert_eq!(
            fs::read(fixture.workspace.mission_dir().join("brief.md")).unwrap(),
            custom
        );
        assert_eq!(fs::read(fixture.workspace.tasks_path()).unwrap(), before);
    }

    #[test]
    fn sync_missing_or_malformed_tasks_fails_closed() {
        let root = test_temp_root("sync-error");
        fs::create_dir_all(root.join(MISSION_DIRECTORY)).unwrap();
        let workspace = MissionWorkspace::new(&root);
        assert!(matches!(
            workspace.sync("missing", "2026-08-29T00:00:00Z"),
            Err(WorkspaceError::NotFound { .. })
        ));
        fs::write(workspace.tasks_path(), b"# Tasks\n").unwrap();
        assert!(workspace.sync("malformed", "2026-08-29T00:00:00Z").is_err());
        assert!(!workspace.mission_dir().join("progress.md").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sync_claims_writer_lock_before_reading_tasks() {
        let fixture = fixture();
        fs::write(fixture.workspace.tasks_path(), b"# malformed\n").unwrap();
        let held = fixture
            .workspace
            .acquire_writer_lock("concurrency-test")
            .expect("hold writer lock");
        let result = fixture
            .workspace
            .sync("blocked-sync", "2026-08-29T00:00:00Z");
        drop(held);
        assert!(matches!(result, Err(WorkspaceError::Contended(_))));
        assert!(!fixture.workspace.mission_dir().join("progress.md").exists());
    }

    #[test]
    fn bounded_reads_distinguish_oversize_and_invalid_utf8() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        fs::write(workspace.mission_dir().join("large.bin"), b"12345").expect("large");
        assert!(matches!(
            workspace.read_artifact("large.bin", 4),
            Err(WorkspaceError::TooLarge { .. })
        ));
        fs::write(workspace.mission_dir().join("bad.txt"), [0xff, 0xfe]).expect("bad");
        assert!(matches!(
            workspace.read_artifact_text("bad.txt", 16),
            Err(WorkspaceError::InvalidUtf8 { .. })
        ));
    }

    #[test]
    fn atomic_write_reports_unchanged_and_lock_is_single_writer() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        let target = workspace.mission_dir().join("atomic.txt");
        assert_eq!(
            workspace.atomic_write(&target, b"same").unwrap(),
            WriteOutcome::Changed
        );
        assert_eq!(
            workspace.atomic_write(&target, b"same").unwrap(),
            WriteOutcome::Unchanged
        );
        assert_eq!(
            workspace.atomic_write(&target, b"replacement").unwrap(),
            WriteOutcome::Changed
        );
        let first = workspace.acquire_writer_lock("first").unwrap();
        assert!(matches!(
            workspace.acquire_writer_lock("second"),
            Err(WorkspaceError::Contended(_))
        ));
        first.release().unwrap();
        assert!(workspace.acquire_writer_lock("second").is_ok());
        let _ = fs::remove_file(workspace.writer_lock_path());
    }

    #[test]
    fn atomic_write_can_shrink_a_bounded_large_file() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        let target = workspace.mission_dir().join("large-atomic.txt");
        let original = vec![b'a'; TASKS_MAX_BYTES as usize + 128];
        let replacement = vec![b'b'; INTERNAL_MAX_BYTES as usize + 64];
        fs::write(&target, &original).expect("write large original");
        assert_eq!(
            workspace.atomic_write(&target, &replacement).unwrap(),
            WriteOutcome::Changed
        );
        assert_eq!(fs::read(&target).unwrap(), replacement);
    }

    #[test]
    fn atomic_write_replaces_same_length_file_above_read_ceiling() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        let target = workspace.mission_dir().join("oversized-atomic.txt");
        let original = vec![b'a'; TASKS_MAX_BYTES as usize + 128];
        let replacement = vec![b'b'; original.len()];
        fs::write(&target, &original).expect("write oversized original");
        assert_eq!(
            workspace.atomic_write(&target, &replacement).unwrap(),
            WriteOutcome::Changed
        );
        assert_eq!(fs::read(&target).unwrap(), replacement);
    }

    #[test]
    fn durability_sync_fallback_is_limited_to_unsupported_errors() {
        assert!(is_unsupported_sync_error(&io::Error::from(
            io::ErrorKind::InvalidInput
        )));
        assert!(is_unsupported_sync_error(&io::Error::from(
            io::ErrorKind::Unsupported
        )));
        assert!(!is_unsupported_sync_error(&io::Error::from(
            io::ErrorKind::PermissionDenied
        )));
        #[cfg(target_os = "macos")]
        assert!(is_unsupported_sync_error(&io::Error::from_raw_os_error(45)));
    }

    #[test]
    fn lock_owner_swap_is_not_removed_by_original_owner() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        let first = workspace.acquire_writer_lock("first").unwrap();
        fs::write(workspace.writer_lock_path(), b"replacement").unwrap();
        assert!(matches!(
            first.release(),
            Err(WorkspaceError::Conflict(message)) if message.contains("tombstone retained")
        ));
        assert!(!workspace.writer_lock_path().exists());
        let tombstone = find_lock_tombstone(&workspace.mission_dir().join(".mission-center"))
            .unwrap()
            .expect("replacement lock tombstone");
        assert_eq!(
            workspace.writer_lock_recovery_artifact().unwrap(),
            Some(tombstone.clone())
        );
        assert_eq!(fs::read(&tombstone).unwrap(), b"replacement");
        assert!(matches!(
            workspace.acquire_writer_lock("second"),
            Err(WorkspaceError::Contended(path)) if path == tombstone
        ));
        fs::remove_file(tombstone).unwrap();
        assert!(workspace.writer_lock_recovery_artifact().unwrap().is_none());
    }

    #[test]
    fn atomic_write_rejects_targets_outside_mission_directory() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        let outside = fixture.root.join("outside.txt");
        assert!(matches!(
            workspace.atomic_write(&outside, b"nope"),
            Err(WorkspaceError::UnsafePath { .. })
        ));
        let escaped = workspace.mission_dir().join("..").join("outside.txt");
        assert!(matches!(
            workspace.atomic_write(&escaped, b"nope"),
            Err(WorkspaceError::UnsafePath { .. })
        ));
    }

    #[test]
    fn operation_receipts_replay_conflict_and_fail_closed() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        assert_eq!(
            workspace.begin_operation("op", "digest", "1").unwrap(),
            OperationOutcome::Started
        );
        assert!(matches!(
            workspace.begin_operation("op", "digest", "1"),
            Err(WorkspaceError::AlreadyStarted(_))
        ));
        assert_eq!(
            workspace.commit_operation("op", "digest", "1").unwrap(),
            OperationOutcome::Committed
        );
        assert_eq!(
            workspace.begin_operation("op", "digest", "2").unwrap(),
            OperationOutcome::Replay
        );
        assert!(workspace.abort_operation("op", "digest", "3").is_err());
        assert_eq!(workspace.operation_status("op").unwrap(), "committed");
        assert!(matches!(
            workspace.begin_operation("op", "other", "1"),
            Err(WorkspaceError::Conflict(_))
        ));

        let malformed = workspace.operation_path("bad").unwrap();
        ensure_directory(malformed.parent().unwrap()).unwrap();
        fs::write(
            malformed,
            r#"{"schemaVersion":"1.0","operationId":"bad","digest":"d","status":"committed","timestamp":"1","unknown":true}"#,
        )
        .unwrap();
        assert!(matches!(
            workspace.begin_operation("bad", "d", "1"),
            Err(WorkspaceError::InvalidReceipt(_))
        ));
    }

    #[test]
    fn concurrent_begin_has_one_starter() {
        let fixture = fixture();
        let workspace = fixture.workspace.clone();
        let results = std::thread::scope(|scope| {
            (0..8)
                .map(|_| {
                    let workspace = workspace.clone();
                    scope.spawn(move || workspace.begin_operation("parallel", "digest", "1"))
                })
                .map(|thread| thread.join().expect("thread"))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Ok(OperationOutcome::Started)))
                .count(),
            1
        );
    }

    #[test]
    fn normalize_snapshot_pulse_and_handoff_are_real_operations() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        let task_path = workspace.tasks_path();
        fs::write(
            &task_path,
            "| ID | Title | Priority | Status | Labels |\n| --- | --- | --- | --- | --- |\n| MC-1 | 測試 | high | doing | Alpha; alpha |\n",
        )
        .unwrap();
        assert_eq!(
            workspace.normalize_tasks("normalize-1", "1").unwrap(),
            WriteOutcome::Changed
        );
        let normalized = fs::read_to_string(&task_path).unwrap();
        assert!(normalized.contains("| P1 | In Progress | alpha |"));
        workspace
            .write_snapshot("snapshot-1", "2", Some("safe note"))
            .unwrap();
        let snapshot = fs::read_to_string(workspace.snapshot_path()).unwrap();
        assert!(snapshot.contains("- Resume:"));
        assert!(snapshot.contains("Read canonical task and next action for MC-1"));
        assert!(!snapshot.ends_with("\n\n"));
        workspace
            .append_pulse_full(
                "pulse-1",
                "pulse-a",
                "MC-1",
                "phase",
                "outcome",
                "next",
                "evidence",
                "1970-01-01T00:00:03Z",
                5,
                None,
            )
            .unwrap();
        assert!(
            workspace
                .append_pulse_full(
                    "pulse-2",
                    "pulse-b",
                    "MC-1",
                    "phase",
                    "outcome",
                    "next two",
                    "evidence",
                    "1970-01-01T00:00:04Z",
                    4,
                    Some("pulse-a")
                )
                .is_ok()
        );
        assert!(
            workspace
                .handoff_json(Some("MC-1"))
                .unwrap()
                .contains("pulse-b")
        );
    }

    #[test]
    fn secret_scanner_matches_python_whitespace_forms() {
        for value in [
            "password : hidden",
            "password\t=hidden",
            "api_key = hidden",
            "api-key\t: hidden",
            "api key = hidden",
            "Bearer eyJheader.payload.signature",
        ] {
            assert!(secret_like(value), "secret form was accepted: {value}");
        }
        for value in ["password", "tokenized label", "public api key"] {
            assert!(!secret_like(value), "non-secret text was rejected: {value}");
        }
    }

    #[test]
    fn snapshot_retry_metadata_is_sanitized_and_verification_gated() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        workspace
            .transition_task("snapshot-in-progress", "MC-1", TaskStatus::InProgress, "1")
            .unwrap();
        let options = SnapshotOptions {
            attempts: vec![
                json!({"phase":"build","errorSignature":"E1"}),
                json!({"phase":"build","errorSignature":"E1","hypothesis":"retry"}),
            ],
            ..SnapshotOptions::default()
        };
        workspace
            .write_snapshot_with_options("snapshot-metadata-1", "2", options)
            .unwrap();
        let first = fs::read_to_string(workspace.snapshot_path()).unwrap();
        assert!(first.contains("- Retry gate: diagnosis"));
        assert!(first.contains("\"errorSignature\":\"E1\""));

        workspace
            .write_snapshot_with_options(
                "snapshot-metadata-2",
                "3",
                SnapshotOptions {
                    hypotheses: vec!["new hypothesis".to_owned()],
                    evidences: vec!["new evidence".to_owned()],
                    ..SnapshotOptions::default()
                },
            )
            .unwrap();
        let required = fs::read_to_string(workspace.snapshot_path()).unwrap();
        assert!(required.contains("- Retry gate: verification_required"));
        assert!(required.contains("new hypothesis"));

        workspace
            .write_snapshot_with_options(
                "snapshot-metadata-3",
                "4",
                SnapshotOptions {
                    verification_result: Some("pass".to_owned()),
                    verification_action: Some("unit_test".to_owned()),
                    verification_evidence: Some("local check passed".to_owned()),
                    ..SnapshotOptions::default()
                },
            )
            .unwrap();
        let verified = fs::read_to_string(workspace.snapshot_path()).unwrap();
        assert!(verified.contains("- Retry gate: retry"));
        assert!(verified.contains("\"result\":\"pass\""));

        fs::write(
            workspace.snapshot_path(),
            "# old\n- Recent attempts JSON: {malformed}\n- Diagnosis evidence JSON: [{\"bad\":true}]\n",
        )
        .unwrap();
        workspace
            .write_snapshot_with_options("snapshot-metadata-4", "5", SnapshotOptions::default())
            .unwrap();
        let sanitized = fs::read_to_string(workspace.snapshot_path()).unwrap();
        assert!(sanitized.contains("- Recent attempts JSON: []"));
        assert!(sanitized.contains("- Diagnosis evidence JSON: []"));
        assert!(!sanitized.contains("{malformed}"));
    }

    #[test]
    fn claims_expire_fence_and_leave_tasks_unchanged() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        let before = fs::read(workspace.tasks_path()).unwrap();
        workspace
            .claim("MC-1", "alice", 1, "200", "100", "claim-1", "1")
            .unwrap();
        assert!(
            workspace
                .claim("MC-1", "bob", 2, "300", "100", "claim-2", "2")
                .is_err()
        );
        workspace
            .claim("MC-1", "bob", 2, "300", "200", "claim-3", "3")
            .unwrap();
        assert!(
            workspace
                .claim("MC-1", "old", 1, "400", "300", "claim-4", "4")
                .is_err()
        );
        assert_eq!(fs::read(workspace.tasks_path()).unwrap(), before);
        workspace
            .release_claim("MC-1", "bob", 2, "release-1", "5")
            .unwrap();
        assert_eq!(
            workspace
                .release_claim("MC-1", "bob", 2, "release-1", "6")
                .unwrap(),
            OperationOutcome::Replay
        );
        let malformed = workspace.claim_path("MC-1");
        ensure_directory(malformed.parent().unwrap()).unwrap();
        fs::write(
            malformed,
            r#"{"schemaVersion":"1.0","taskId":"MC-1","owner":"bob","fence":2,"expiresAt":"300","operationId":"bad","digest":"bad","timestamp":"6","unknown":"reject"}"#,
        )
        .unwrap();
        assert!(matches!(
            workspace.read_claim("MC-1"),
            Err(WorkspaceError::InvalidReceipt(_))
        ));
    }

    #[test]
    fn claim_rejects_expiry_at_or_before_now_and_release_recovers_missing_claim() {
        let fixture = fixture();
        let workspace = &fixture.workspace;
        assert!(
            workspace
                .claim("MC-1", "alice", 1, "100", "100", "bad-expiry", "1")
                .is_err()
        );
        let digest = mission_center_core::sha256_digest(
            format!("release\0{}\0{}\0{}", "MC-1", "alice", 1).as_bytes(),
        );
        workspace.begin_operation("recover", &digest, "1").unwrap();
        assert_eq!(
            workspace
                .release_claim("MC-1", "alice", 1, "recover", "2")
                .unwrap(),
            OperationOutcome::Committed
        );
        assert_eq!(workspace.operation_status("recover").unwrap(), "committed");
    }

    #[test]
    fn progress_percentage_widens_saturated_estimates_before_multiplication() {
        let tasks = mission_center_core::parse_tasks_markdown(
            "| ID | Title | Status | Estimate |\n| --- | --- | --- | --- |\n| A | done | Done | 4294967295 |\n| B | ready | Ready | 4294967295 |\n",
        )
        .unwrap();
        let (percent, mode, _, _) = compute_progress(&tasks);
        assert_eq!(percent, 50);
        assert_eq!(mode, "4294967295/8589934590 estimated");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_component_is_rejected() {
        use std::os::unix::fs::symlink;
        let fixture = fixture();
        let workspace = &fixture.workspace;
        let outside = fixture.root.join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret"), b"secret").unwrap();
        symlink(&outside, workspace.mission_dir().join("link")).unwrap();
        assert!(matches!(
            workspace.read_artifact("link/secret", 64),
            Err(WorkspaceError::UnsafePath { .. })
        ));
    }
}
