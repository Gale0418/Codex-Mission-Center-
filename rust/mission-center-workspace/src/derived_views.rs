use crate::WorkspaceLanguage;
use mission_center_core::{CoreError, Task, TaskStatus, split_cells};
use std::collections::HashSet;

pub(crate) const BRIEF_MAX_BYTES: u64 = 16 * 1024;
pub(crate) const WORKING_SET_MAX_BYTES: u64 = 4 * 1024;
pub(crate) const FOCUS_MAX_BYTES: u64 = 16 * 1024;
const DERIVED_WARNING: &str = "Generated materialized view. Do not edit directly; rebuild from canonical MissionCenter files.";
const FOCUS_DEPRECATION: &str = "Deprecated compatibility view: focus.md is generated from tasks.md only and must never be edited or treated as a second lifecycle source.";

pub(crate) struct DerivedViews {
    pub(crate) brief: String,
    pub(crate) working_set: String,
    pub(crate) focus: String,
}

pub(crate) struct RenderInput<'a> {
    pub(crate) project: &'a str,
    pub(crate) goal: &'a str,
    pub(crate) cycle: &'a str,
    pub(crate) workspace_fingerprint: &'a str,
    pub(crate) tasks_fingerprint: &'a str,
    pub(crate) language: WorkspaceLanguage,
    pub(crate) timestamp: &'a str,
    pub(crate) daily_log: Option<&'a str>,
    pub(crate) guardrails: Option<&'a str>,
}

pub(crate) fn render_views(
    tasks: &[Task],
    input: &RenderInput<'_>,
) -> Result<DerivedViews, CoreError> {
    let working_set_count = working_set_tasks(tasks).len();
    Ok(DerivedViews {
        brief: render_brief(input, working_set_count)?,
        working_set: render_working_set(tasks, input.tasks_fingerprint, input.language),
        focus: render_focus(tasks, input.tasks_fingerprint, input.language),
    })
}

pub(crate) fn is_managed_view(text: &str) -> bool {
    text.contains("mission-center-derived")
        || text.contains("Generated materialized view.")
        || text.contains("Generated after bootstrap.")
        || text.contains("Bootstrap 後產生。")
}

fn derived_marker(fingerprint: &str) -> String {
    format!(
        "<!-- mission-center-derived schema=1.0 fingerprint-format=sha256-v2-lf source-fingerprint={fingerprint} -->"
    )
}

fn escape_derived_cell(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace(['\r', '\n'], " ")
}

fn working_set_tasks(tasks: &[Task]) -> Vec<&Task> {
    let unfinished: Vec<&Task> = tasks
        .iter()
        .filter(|task| task.status != TaskStatus::Done)
        .collect();
    let mut selected = Vec::new();
    let mut seen = HashSet::new();
    for status in [
        TaskStatus::Blocked,
        TaskStatus::InProgress,
        TaskStatus::Review,
    ] {
        for task in unfinished.iter().filter(|task| task.status == status) {
            if seen.insert(task.id.as_str()) {
                selected.push(*task);
            }
            if selected.len() >= 6 {
                return selected;
            }
        }
    }
    for task in unfinished.iter().filter(|task| {
        task.priority.eq_ignore_ascii_case("P0") && task.status != TaskStatus::Backlog
    }) {
        if seen.insert(task.id.as_str()) {
            selected.push(*task);
        }
        if selected.len() >= 6 {
            return selected;
        }
    }
    let mut ready: Vec<&Task> = unfinished
        .iter()
        .filter(|task| task.status == TaskStatus::Ready)
        .copied()
        .collect();
    ready.sort_by(|left, right| {
        task_priority_key(left)
            .cmp(&task_priority_key(right))
            .then_with(|| left.id.cmp(&right.id))
    });
    for task in ready {
        if seen.insert(task.id.as_str()) {
            selected.push(task);
        }
        if selected.len() >= 6 {
            break;
        }
    }
    selected
}

/// Return the bounded working-set IDs used by both sync views and the CLI.
/// `tasks.md` remains the only lifecycle/order source.
pub fn working_set_ids(tasks: &[Task]) -> Vec<String> {
    working_set_tasks(tasks)
        .into_iter()
        .map(|task| task.id.clone())
        .collect()
}

fn task_priority_key(task: &Task) -> (u32, &str) {
    let priority = task.priority.trim();
    let value = priority
        .strip_prefix('P')
        .or_else(|| priority.strip_prefix('p'))
        .and_then(|value| value.parse::<u32>().ok())
        .unwrap_or(99);
    (value, task.id.as_str())
}

fn next_candidates(tasks: &[Task]) -> Vec<&Task> {
    let done: HashSet<&str> = tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Done)
        .map(|task| task.id.as_str())
        .collect();
    let mut candidates: Vec<&Task> = tasks
        .iter()
        .filter(|task| {
            task.status == TaskStatus::Backlog
                && task.dependencies.iter().all(|dependency| {
                    dependency.trim().is_empty() || done.contains(dependency.trim())
                })
        })
        .collect();
    candidates.sort_by(|left, right| {
        task_priority_key(left)
            .cmp(&task_priority_key(right))
            .then_with(|| left.id.cmp(&right.id))
    });
    candidates.truncate(2);
    candidates
}

fn render_working_set(tasks: &[Task], fingerprint: &str, language: WorkspaceLanguage) -> String {
    let items = working_set_tasks(tasks);
    let candidates = next_candidates(tasks);
    let (title, source, count, headers, candidate_title, candidate_note) = match language {
        WorkspaceLanguage::English => (
            "Active Working Set",
            "Source of truth",
            "Unfinished working set count",
            "| ID | Title | Priority | Status | Next action | Depends on | Verification | Blocker reason |",
            "Next Candidates",
            "Candidates only; promote to Ready in `tasks.md` before starting.",
        ),
        WorkspaceLanguage::TraditionalChinese => (
            "當前工作集",
            "唯一真實來源",
            "可執行項目數",
            "| ID | 標題 | 優先級 | 狀態 | 下一步 | 依賴 | 驗證方式 | 阻塞原因 |",
            "下一步候選",
            "以上僅為候選，開始前仍須在 `tasks.md` 升格為 Ready。",
        ),
    };
    let mut lines = vec![
        format!("<!-- {DERIVED_WARNING} -->"),
        derived_marker(fingerprint),
        format!("# {title}"),
        String::new(),
        format!("- {source}: `tasks.md`"),
        format!("- {count}: {}", items.len()),
    ];
    let mut candidate_lines = Vec::new();
    if !candidates.is_empty() {
        candidate_lines.push(String::new());
        candidate_lines.push(format!("## {candidate_title}"));
        candidate_lines.push(String::new());
        candidate_lines.extend(
            candidates
                .iter()
                .map(|task| format!("- {} — {}", task.id, task.title)),
        );
        candidate_lines.push(format!("- {candidate_note}"));
    }
    if items.is_empty() {
        let reason = if tasks.iter().all(|task| task.status == TaskStatus::Done) {
            "all work complete"
        } else if tasks.iter().any(|task| task.status == TaskStatus::Blocked) {
            "blocked"
        } else {
            "dependency unresolved"
        };
        lines.push(format!("- Status: {reason}"));
        lines.extend(candidate_lines);
        return lines.join("\n") + "\n";
    }
    lines.extend([
        String::new(),
        headers.to_owned(),
        "| --- | --- | --- | --- | --- | --- | --- | --- |".to_owned(),
    ]);
    for task in items {
        let blocker = if task.status == TaskStatus::Blocked {
            task.notes.as_str()
        } else {
            ""
        };
        let dependencies = task.dependencies.join(", ");
        let values = [
            task.id.as_str(),
            task.title.as_str(),
            task.priority.as_str(),
            task.status.as_str(),
            task.next_action.as_str(),
            dependencies.as_str(),
            task.verification.as_str(),
            blocker,
        ];
        lines.push(format!(
            "| {} |",
            values
                .iter()
                .map(|value| escape_derived_cell(value))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    lines.extend(candidate_lines);
    lines.join("\n") + "\n"
}

fn render_focus(tasks: &[Task], fingerprint: &str, language: WorkspaceLanguage) -> String {
    let focus: Vec<&Task> = tasks
        .iter()
        .filter(|task| task.priority.eq_ignore_ascii_case("P0") && task.status != TaskStatus::Done)
        .collect();
    let (title, source, count, headers) = match language {
        WorkspaceLanguage::English => (
            "P0 Focus",
            "Source of truth",
            "Unfinished P0",
            "| ID | Title | Status | Next action | Depends on | Verification |",
        ),
        WorkspaceLanguage::TraditionalChinese => (
            "P0 焦點",
            "唯一真實來源",
            "未完成 P0",
            "| ID | 標題 | 狀態 | 下一步 | 依賴 | 驗證方式 |",
        ),
    };
    let mut lines = vec![
        format!("<!-- {DERIVED_WARNING} -->"),
        format!("<!-- {FOCUS_DEPRECATION} -->"),
        derived_marker(fingerprint),
        format!("# {title}"),
        String::new(),
        format!("- {source}: `tasks.md`"),
        format!("- {count}: {}", focus.len()),
        String::new(),
        headers.to_owned(),
        "| --- | --- | --- | --- | --- | --- |".to_owned(),
    ];
    for task in focus {
        let dependencies = task.dependencies.join(", ");
        let values = [
            task.id.as_str(),
            task.title.as_str(),
            task.status.as_str(),
            task.next_action.as_str(),
            dependencies.as_str(),
            task.verification.as_str(),
        ];
        lines.push(format!(
            "| {} |",
            values
                .iter()
                .map(|value| escape_derived_cell(value))
                .collect::<Vec<_>>()
                .join(" | ")
        ));
    }
    lines.join("\n") + "\n"
}

#[derive(Debug, Default)]
struct GuardrailTable {
    active_ids: Vec<String>,
}

fn separator_cell(cell: &str) -> bool {
    let value = cell.trim();
    let value = value.strip_prefix(':').unwrap_or(value);
    let value = value.strip_suffix(':').unwrap_or(value);
    value.len() >= 3 && value.chars().all(|ch| ch == '-')
}

fn parse_active_guardrails(text: &str) -> Result<GuardrailTable, CoreError> {
    let mut lines = text.lines().enumerate();
    let mut header: Option<(usize, usize, usize)> = None;
    while let Some((line_number, line)) = lines.next() {
        if !line.trim_start().starts_with('|') {
            continue;
        }
        let cells = split_cells(line)?;
        let id = cells.iter().position(|cell| cell == "ID");
        let status = cells
            .iter()
            .position(|cell| matches!(cell.as_str(), "Status" | "狀態"));
        if let (Some(id), Some(status)) = (id, status) {
            header = Some((id, status, cells.len()));
            let (separator_line_number, separator) =
                lines.next().ok_or(CoreError::InvalidSeparator)?;
            let separator_cells = split_cells(separator)?;
            if separator_cells.len() != cells.len()
                || !separator_cells.iter().all(|cell| separator_cell(cell))
            {
                return Err(CoreError::InvalidSeparator);
            }
            let _ = (line_number, separator_line_number);
            break;
        }
    }
    let Some((id_index, status_index, cell_count)) = header else {
        return Ok(GuardrailTable::default());
    };
    let mut active_ids = Vec::new();
    for (line_number, line) in lines {
        if !line.trim_start().starts_with('|') {
            break;
        }
        let cells = split_cells(line)?;
        if cells.len() != cell_count {
            return Err(CoreError::WrongCellCount {
                row: line_number + 1,
                found: cells.len(),
                expected: cell_count,
            });
        }
        let id = cells[id_index].trim();
        let status = cells[status_index].trim();
        if !id.is_empty() && matches!(status, "Active" | "啟用" | "有效") {
            active_ids.push(id.to_owned());
        }
    }
    Ok(GuardrailTable { active_ids })
}

/// `sync_with_options` validates RFC3339 before calling this renderer.
fn date_from_timestamp(timestamp: &str) -> Result<&str, CoreError> {
    if timestamp.len() >= 10
        && timestamp.is_char_boundary(10)
        && timestamp.as_bytes()[4] == b'-'
        && timestamp.as_bytes()[7] == b'-'
        && timestamp[..10]
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
    {
        Ok(&timestamp[..10])
    } else {
        Err(CoreError::MalformedRow {
            row: 0,
            reason: "sync timestamp must be RFC3339".to_owned(),
        })
    }
}

fn daily_entries(text: Option<&str>, target: &str) -> Vec<String> {
    let mut current = false;
    let mut entries = Vec::new();
    for line in text.unwrap_or_default().lines() {
        let value = line.trim();
        if let Some(date) = value.strip_prefix("## ") {
            current = date == target;
            continue;
        }
        if current && let Some(entry) = value.strip_prefix("- ") {
            let entry = entry.trim();
            if !entry.is_empty()
                && !matches!(entry, "None" | "無")
                && !entries.iter().any(|seen| seen == entry)
            {
                entries.push(entry.to_owned());
            }
        }
    }
    entries
}

fn bounded_lines(items: &[String], limit: usize, none: &str) -> String {
    if items.is_empty() {
        return format!("- {none}");
    }
    let mut lines: Vec<String> = items
        .iter()
        .take(limit)
        .map(|item| format!("- {item}"))
        .collect();
    if items.len() > limit {
        lines.push(format!(
            "- [TRUNCATED] {} additional items require canonical file access.",
            items.len() - limit
        ));
    }
    lines.join("\n")
}

fn render_brief(input: &RenderInput<'_>, working_count: usize) -> Result<String, CoreError> {
    let project = input.project;
    let goal = input.goal;
    let cycle = input.cycle;
    let fingerprint = input.workspace_fingerprint;
    let (
        title,
        project_label,
        goal_label,
        cycle_label,
        source_label,
        organized_label,
        fingerprint_label,
        work_label,
        work_line,
        route_label,
        route,
    ) = match input.language {
        WorkspaceLanguage::English => (
            "Mission Brief",
            "Project",
            "North Star",
            "Cycle",
            "Source of truth",
            "Last organized",
            "Source fingerprint",
            "Current work",
            "- Current work ({working_count} items) → `working-set.md`",
            "Read Next Only When Needed",
            [
                "- Modify task lifecycle/order → `tasks.md`",
                "- Need rationale/evidence → `decisions.md`, `notes.md`, `smoke-tests.md`",
            ],
        ),
        WorkspaceLanguage::TraditionalChinese => (
            "任務簡報",
            "專案",
            "北極星",
            "週期",
            "唯一真實來源",
            "最後整理",
            "來源指紋",
            "目前工作",
            "- 目前工作（{working_count} 項）→ `working-set.md`",
            "需要時再讀",
            [
                "- 修改任務生命週期／順序 → `tasks.md`",
                "- 查閱理由／證據 → `decisions.md`、`notes.md`、`smoke-tests.md`",
            ],
        ),
    };
    let day = date_from_timestamp(input.timestamp)?;
    let none = if input.language == WorkspaceLanguage::TraditionalChinese {
        "無"
    } else {
        "None"
    };
    let goal_value = if goal.trim().is_empty() { none } else { goal };
    let cycle_value = if cycle.trim().is_empty() { none } else { cycle };
    let entries = daily_entries(input.daily_log, day);
    let entry_lines = bounded_lines(&entries, 8, none);
    let active_guardrails = parse_active_guardrails(input.guardrails.unwrap_or_default())?;
    let guardrail_lines = bounded_lines(&active_guardrails.active_ids, 20, none);
    let (today_title, guardrail_title, route_stale) = match input.language {
        WorkspaceLanguage::English => (
            "Today's Summary",
            "Relevant Guardrails",
            "- Brief/working set stale or truncated → run `mission_maintenance.py sync` and open canonical files",
        ),
        WorkspaceLanguage::TraditionalChinese => (
            "今日摘要",
            "重要護欄",
            "- 簡報／工作集過期或截斷 → 執行 `mission_maintenance.py sync` 後再讀 canonical files",
        ),
    };
    let normal = format!(
        "<!-- {DERIVED_WARNING} -->\n{}\n# {title}\n\n- {organized_label}: {day}\n- {fingerprint_label}: `{fingerprint}`\n- {source_label}: `tasks.md`\n- {project_label}: {project}\n- {goal_label}: {goal_value}\n- {cycle_label}: {cycle_value}\n\n## {today_title} · {day}\n{entry_lines}\n\n## {guardrail_title} ({})\n{guardrail_lines}\n\n## {route_label}\n{}\n{}\n{route_stale}\n",
        derived_marker(input.workspace_fingerprint),
        active_guardrails.active_ids.len(),
        work_line.replace("{working_count}", &working_count.to_string()),
        route.join("\n"),
    );
    if normal.len() as u64 <= BRIEF_MAX_BYTES {
        return Ok(normal);
    }
    let minimal = format!(
        "<!-- {DERIVED_WARNING} -->\n{}\n# {title}\n\n- {organized_label}: {day}\n- {fingerprint_label}: `{fingerprint}`\n- {source_label}: `tasks.md`\n- {project_label}: {project}\n- {goal_label}: {goal_value}\n- {cycle_label}: {cycle_value}\n\n## Context counts\n- {work_label}: {working_count}\n- {guardrail_title}: {}\n- [TRUNCATED] Brief exceeded its byte budget; read `working-set.md` and canonical files.\n",
        derived_marker(input.workspace_fingerprint),
        active_guardrails.active_ids.len(),
    );
    Ok(minimal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_keeps_language_date_and_none_fallbacks() {
        let english = render_brief(
            &RenderInput {
                project: "Project",
                goal: "Goal",
                cycle: "Cycle",
                workspace_fingerprint: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                tasks_fingerprint: "",
                language: WorkspaceLanguage::English,
                timestamp: "2026-08-29T00:00:00Z",
                daily_log: None,
                guardrails: None,
            },
            0,
        )
        .unwrap();
        assert!(english.contains("- Last organized: 2026-08-29"));
        assert!(english.contains("## Today's Summary · 2026-08-29\n- None"));
        assert!(english.contains("## Relevant Guardrails (0)\n- None"));
        assert!(english.contains("\n- Brief/working set stale or truncated"));
        assert!(!english.contains("\n- - Brief/working set"));

        let chinese = render_brief(
            &RenderInput {
                project: "專案",
                goal: "目標",
                cycle: "週期",
                workspace_fingerprint: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                tasks_fingerprint: "",
                language: WorkspaceLanguage::TraditionalChinese,
                timestamp: "2026-08-29T00:00:00Z",
                daily_log: None,
                guardrails: None,
            },
            0,
        )
        .unwrap();
        assert!(chinese.contains("- 最後整理: 2026-08-29"));
        assert!(chinese.contains("## 今日摘要 · 2026-08-29\n- 無"));
        assert!(chinese.contains("## 重要護欄 (0)\n- 無"));
        assert!(chinese.contains("\n- 簡報／工作集過期或截斷"));
        assert!(!chinese.contains("\n- - 簡報／工作集"));
    }

    #[test]
    fn brief_limits_daily_entries_and_falls_back_to_bounded_minimal_view() {
        let daily = "## 2026-08-29\n".to_owned()
            + &(1..=9)
                .map(|index| format!("- entry-{index}\n"))
                .collect::<String>();
        let mut guardrails = String::from("| ID | Status |\n| --- | --- |\n");
        let mut bounded_guardrails = String::from("| ID | Status |\n| --- | --- |\n");
        for index in 0..21 {
            bounded_guardrails.push_str(&format!("| G-{index} | Active |\n"));
        }
        let bounded = render_brief(
            &RenderInput {
                project: "Project",
                goal: "Goal",
                cycle: "Cycle",
                workspace_fingerprint: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                tasks_fingerprint: "",
                language: WorkspaceLanguage::English,
                timestamp: "2026-08-29T00:00:00Z",
                daily_log: Some(&daily),
                guardrails: Some(&bounded_guardrails),
            },
            1,
        )
        .unwrap();
        assert!(bounded.contains("[TRUNCATED] 1 additional items require canonical file access."));

        for index in 0..20 {
            guardrails.push_str(&format!("| {} | Active |\n", "G".repeat(1000 + index)));
        }
        let brief = render_brief(
            &RenderInput {
                project: "Project",
                goal: "Goal",
                cycle: "Cycle",
                workspace_fingerprint: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                tasks_fingerprint: "",
                language: WorkspaceLanguage::English,
                timestamp: "2026-08-29T00:00:00Z",
                daily_log: Some(&daily),
                guardrails: Some(&guardrails),
            },
            1,
        )
        .unwrap();
        assert!(brief.contains("[TRUNCATED] Brief exceeded its byte budget"));
        assert!(!brief.contains("entry-1"));
        assert!(!brief.contains(&"G".repeat(1000)));
        assert!(brief.len() <= BRIEF_MAX_BYTES as usize);
    }

    #[test]
    fn guardrail_parser_reuses_core_escaping_and_fails_closed() {
        let parsed =
            parse_active_guardrails("| ID | Status |\n| --- | --- |\n| G\\|1\\\\2 | Active |\n")
                .unwrap();
        assert_eq!(parsed.active_ids, vec!["G|1\\2"]);

        assert!(matches!(
            parse_active_guardrails("| ID | Status |\n| --- | --- |\n| G-1 | Active | extra |\n"),
            Err(CoreError::WrongCellCount { .. })
        ));
        assert!(matches!(
            parse_active_guardrails("| ID | Status |\n| --- | --- |\n| G-1 | Active \\"),
            Err(CoreError::MalformedRow { .. })
        ));
    }

    #[test]
    fn timestamp_requires_validated_rfc3339_date_prefix() {
        assert_eq!(
            date_from_timestamp("2026-08-29T00:00:00Z").unwrap(),
            "2026-08-29"
        );
        assert!(date_from_timestamp("not-a-timestamp").is_err());
    }

    #[test]
    fn focus_view_budget_handles_current_p0_task_count() {
        let tasks = (0..12)
            .map(|index| Task {
                id: format!("MC-{index}"),
                title: format!("Provider 設定與驗證任務 {}", "x".repeat(120)),
                kind: "Task".to_owned(),
                parent: "MC-E1".to_owned(),
                priority: "P0".to_owned(),
                status: TaskStatus::InProgress,
                assignee: "Codex".to_owned(),
                dependencies: Vec::new(),
                next_action: format!(
                    "執行 bounded regression 與 review evidence {}",
                    "y".repeat(180)
                ),
                verification: format!(
                    "所有本機契約需可重複驗證且不得宣稱外部 gate {}",
                    "z".repeat(180)
                ),
                estimate: "1h".to_owned(),
                tags: vec!["verification".to_owned()],
                notes: String::new(),
            })
            .collect::<Vec<_>>();
        let focus = render_focus(
            &tasks,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            WorkspaceLanguage::TraditionalChinese,
        );
        assert!(focus.len() > WORKING_SET_MAX_BYTES as usize);
        assert!(focus.len() <= FOCUS_MAX_BYTES as usize);
    }
}
