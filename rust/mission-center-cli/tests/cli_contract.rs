use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

static WORKSPACE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn workspace(tasks: &str) -> std::path::PathBuf {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
        .to_string();
    // macOS exposes the temporary directory through /var, which is a symlink to
    // /private/var. Resolve that platform alias before exercising the production
    // symlink guard so the fixture itself does not look like an unsafe workspace.
    let temp = std::env::temp_dir();
    #[cfg(target_os = "macos")]
    let temp = temp.canonicalize().expect("canonical temporary directory");
    let sequence = WORKSPACE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let root = temp.join(format!(
        "mission-center-rust-{}-{suffix}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(root.join("MissionCenter")).expect("create fixture");
    fs::write(root.join("MissionCenter/tasks.md"), tasks).expect("write fixture");
    root
}

fn assert_machine_envelope(
    output: &std::process::Output,
    command: &str,
    code: i32,
) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(code));
    assert!(
        output.stderr.is_empty(),
        "CLI diagnostics must not pollute stderr"
    );
    let text = String::from_utf8(output.stdout.clone()).expect("utf8");
    let trimmed = text.trim();
    assert!(
        !trimmed.is_empty() && trimmed == text.trim_end(),
        "stdout must be one JSON value"
    );
    let payload: serde_json::Value = serde_json::from_str(trimmed).expect("machine JSON");
    let object = payload.as_object().expect("envelope object");
    for key in object.keys() {
        assert!(
            matches!(
                key.as_str(),
                "schemaVersion" | "command" | "route" | "status" | "data" | "errorCode" | "error"
            ),
            "unknown root field: {key}"
        );
    }
    assert_eq!(payload["schemaVersion"], "1.0");
    assert_eq!(payload["command"], command);
    assert!(payload.get("data").is_some());
    payload
}

#[test]
fn machine_envelope_and_exit_abi_covers_read_only_and_failure_routes() {
    let schema: serde_json::Value =
        serde_json::from_str(include_str!("../schemas/cli-envelope.schema.json"))
            .expect("CLI envelope schema");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["error"]["additionalProperties"], false);
    let root =
        workspace("| ID | Title | Status |\n| --- | --- | --- |\n| MC-ABI | ABI | Ready |\n");
    let binary = env!("CARGO_BIN_EXE_mission-center");
    let cases = [
        (
            "status",
            vec!["status", "--root", root.to_str().unwrap()],
            1,
        ),
        (
            "resume",
            vec!["resume", "--root", root.to_str().unwrap()],
            0,
        ),
        (
            "reconcile",
            vec!["reconcile", "--root", root.to_str().unwrap()],
            0,
        ),
        (
            "doctor",
            vec!["doctor", "--root", root.to_str().unwrap()],
            0,
        ),
        ("runtime", vec!["runtime", "capability"], 0),
        ("hud", vec!["hud", "capability"], 0),
        ("publish", vec!["publish", "--operation-id", "abi-op"], 1),
        (
            "publish",
            vec![
                "publish",
                "verify",
                "--version",
                "0.5.1",
                "--platform",
                "windows-x86_64",
            ],
            1,
        ),
    ];
    for (command, args, code) in cases {
        let arg_count = args.len();
        let output = std::process::Command::new(binary)
            .args(args)
            .output()
            .expect("run CLI");
        let payload = assert_machine_envelope(&output, command, code);
        if code == 1 && command == "publish" && arg_count == 3 {
            assert_eq!(payload["errorCode"], "unsupported");
            assert_eq!(payload["data"]["written"], false);
        }
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn native_reconcile_routes_are_explicit_and_bounded() {
    let root = workspace(
        "| ID | Title | Status |\n| --- | --- | --- |\n| MC-RECON | Reconcile | Ready |\n",
    );
    let binary = env!("CARGO_BIN_EXE_mission-center");
    for command in ["install", "publish"] {
        let output = Command::new(binary)
            .args([
                command,
                "reconcile",
                "--root",
                root.to_str().expect("utf8 root"),
            ])
            .output()
            .expect("run reconcile");
        let payload = assert_machine_envelope(&output, command, 0);
        assert_eq!(payload["data"]["reconciled"], true);
        assert_eq!(payload["data"]["receipts"].as_array().unwrap().len(), 0);
        assert_eq!(payload["data"]["mutationSupported"], true);
    }
    let output = Command::new(binary)
        .args([
            "reconcile",
            "--transaction-root",
            root.to_str().expect("utf8 root"),
        ])
        .output()
        .expect("run top-level reconcile");
    let payload = assert_machine_envelope(&output, "reconcile", 0);
    assert_eq!(payload["data"]["reconciled"], true);
    assert_eq!(payload["data"]["mutationSupported"], true);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn workspace_commands_reject_unknown_duplicate_and_missing_flags() {
    let root =
        workspace("| ID | Title | Status |\n| --- | --- | --- |\n| MC-FLAG | flags | Ready |\n");
    for args in [
        vec!["status", "--unknown"],
        vec!["resume", "--date"],
        vec!["reconcile", "--date=", "--root", root.to_str().unwrap()],
        vec![
            "doctor",
            "--root",
            root.to_str().unwrap(),
            "--root",
            root.to_str().unwrap(),
        ],
        vec!["verify", "--unknown"],
        vec![
            "snapshot",
            "--operation-id",
            "op",
            "--operation-id",
            "other",
        ],
        vec!["pulse", "--operation-id"],
        vec!["handoff", "--task-id", "--bad"],
        vec!["closeout", "--archive=true"],
        vec!["project-map", "--verify=true"],
        vec!["claim", "MC-FLAG", "--owner"],
        vec!["release-claim", "MC-FLAG", "--fence", "1", "--fence", "2"],
        vec!["transition", "MC-FLAG", "Done", "--unknown"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
            .args(args)
            .output()
            .expect("run CLI");
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stderr.is_empty());
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
        assert_eq!(payload["status"], "error");
        assert_eq!(payload["errorCode"], "argument_error");
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn resume_routes_completed_workspace_without_handoff() {
    let root = workspace("| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | 完成 | Done |\n");
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["resume", "--root"])
        .arg(&root)
        .output()
        .expect("run cli");
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert!(text.contains("\"route\":\"complete\""));
    assert!(text.contains("\"actionableHandoff\":false"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn valid_transition_commits_once_and_replays_safely() {
    let source = "| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | 審查 | Review |\n";
    let root = workspace(source);
    let task = mission_center_core::parse_tasks_markdown(source)
        .expect("parse task")
        .remove(0);
    let evidence = root.join("output/mission-center-evidence");
    fs::create_dir_all(&evidence).expect("create evidence");
    fs::write(evidence.join("smoke.md"), "pass").expect("write evidence");
    let passports = root.join("output/mission-center-passports");
    fs::create_dir_all(&passports).expect("create passport directory");
    fs::write(
        passports.join("MC-1.json"),
        serde_json::json!({
            "schemaVersion":"1.0",
            "artifactType":"completion-passport",
            "taskId":"MC-1",
            "taskDigest":mission_center_core::canonical_task_digest(&task),
            "status":"current",
            "verification":{"result":"pass","evidenceRefs":["output/mission-center-evidence/smoke.md"]},
            "findings":[]
        }).to_string(),
    ).expect("write passport");
    let path = root.join("MissionCenter/tasks.md");
    let before = fs::read(&path).expect("read before");
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args([
            "transition",
            "MC-1",
            "Done",
            "--operation-id",
            "transition-cli",
            "--timestamp",
            "2026-08-29T13:10:00Z",
            "--root",
        ])
        .arg(&root)
        .output()
        .expect("run cli");
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert!(text.contains("\"status\":\"committed\""));
    assert!(text.contains("\"written\":true"));
    assert!(output.status.success());
    assert_ne!(fs::read(&path).expect("read after"), before);
    assert!(
        fs::read_to_string(&path)
            .expect("read status")
            .contains("| MC-1 | 審查 | Done |")
    );
    let replay = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args([
            "transition",
            "MC-1",
            "Done",
            "--operation-id",
            "transition-cli",
            "--timestamp",
            "2026-08-29T13:10:00Z",
            "--root",
        ])
        .arg(&root)
        .output()
        .expect("replay cli");
    let replay_text = String::from_utf8(replay.stdout).expect("utf8 replay");
    assert!(replay_text.contains("\"status\":\"replay\""));
    assert!(replay.status.success());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn transition_reaches_task_in_a_later_canonical_table() {
    let source = "| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | First | Done |\n\n## Migration notes\n\n| ID | Title | Status |\n| --- | --- | --- |\n| MC-2 | Later | Review |\n";
    let root = workspace(source);
    let task = mission_center_core::parse_tasks_markdown(source)
        .expect("parse task")
        .into_iter()
        .find(|task| task.id == "MC-2")
        .expect("later task");
    let evidence = root.join("output/mission-center-evidence");
    fs::create_dir_all(&evidence).expect("create evidence");
    fs::write(evidence.join("smoke.md"), "pass").expect("write evidence");
    let passports = root.join("output/mission-center-passports");
    fs::create_dir_all(&passports).expect("create passport directory");
    fs::write(
        passports.join("MC-2.json"),
        serde_json::json!({
            "schemaVersion":"1.0",
            "artifactType":"completion-passport",
            "taskId":"MC-2",
            "taskDigest":mission_center_core::canonical_task_digest(&task),
            "status":"current",
            "verification":{"result":"pass","evidenceRefs":["output/mission-center-evidence/smoke.md"]},
            "findings":[]
        }).to_string(),
    ).expect("write passport");
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args([
            "transition",
            "MC-2",
            "Done",
            "--operation-id",
            "later-table-transition",
            "--timestamp",
            "2026-08-29T13:10:00Z",
            "--root",
        ])
        .arg(&root)
        .output()
        .expect("run cli");
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert!(text.contains("\"status\":\"committed\""), "{text}");
    assert!(output.status.success(), "{text}");
    assert!(
        fs::read_to_string(root.join("MissionCenter/tasks.md"))
            .expect("read status")
            .contains("| MC-2 | Later | Done |")
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn sync_then_append_keeps_new_task_in_the_transition_view() {
    let root =
        workspace("| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | First | Ready |\n");
    let sync = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["sync", "--root"])
        .arg(&root)
        .args([
            "--operation-id",
            "append-sync",
            "--timestamp",
            "2026-08-29T13:11:00Z",
        ])
        .output()
        .expect("run sync");
    assert!(
        sync.status.success(),
        "{}",
        String::from_utf8_lossy(&sync.stdout)
    );

    let mut tasks = fs::OpenOptions::new()
        .append(true)
        .open(root.join("MissionCenter/tasks.md"))
        .expect("open tasks");
    tasks
        .write_all(
            b"\n## Appended tasks\n\n| ID | Title | Status |\n| --- | --- | --- |\n| MC-2 | Second | Ready |\n",
        )
        .expect("append task table");

    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args([
            "transition",
            "MC-2",
            "In Progress",
            "--operation-id",
            "append-transition",
            "--timestamp",
            "2026-08-29T13:12:00Z",
            "--root",
        ])
        .arg(&root)
        .output()
        .expect("run transition");
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert!(output.status.success(), "{payload}");
    assert_eq!(payload["status"], "committed");
    assert_eq!(payload["data"]["from"], "Ready");
    assert_eq!(payload["data"]["to"], "In Progress");
    assert!(
        fs::read_to_string(root.join("MissionCenter/tasks.md"))
            .expect("read tasks")
            .contains("| MC-2 | Second | In Progress |")
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn transition_reaches_schema_continuation_row_without_repeated_header() {
    let root = workspace(
        "| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | First | Ready |\n\n| MC-2 | Second | Ready |\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args([
            "transition",
            "MC-2",
            "In Progress",
            "--operation-id",
            "continuation-transition",
            "--timestamp",
            "2026-08-29T13:13:00Z",
            "--root",
        ])
        .arg(&root)
        .output()
        .expect("run transition");
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert!(output.status.success(), "{payload}");
    assert_eq!(payload["status"], "committed");
    assert_eq!(payload["data"]["from"], "Ready");
    assert!(
        fs::read_to_string(root.join("MissionCenter/tasks.md"))
            .expect("read tasks")
            .contains("| MC-2 | Second | In Progress |")
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn status_allows_security_vocabulary_in_canonical_task_titles() {
    let root = workspace(
        "| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | Bearer Key argv hardening | Ready |\n",
    );
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["status", "--root"])
        .arg(&root)
        .output()
        .expect("run cli");
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(output.status.code(), Some(1), "{payload}");
    assert_eq!(payload["status"], "stale");
    assert_ne!(payload["errorCode"], "privacy_violation");
    assert_eq!(payload["data"]["taskCount"], 1);
    assert_eq!(
        payload["data"]["tasks"][0]["title"],
        "Bearer Key argv hardening"
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn missing_root_value_is_an_argument_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["status", "--root"])
        .output()
        .expect("run cli");
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert!(text.contains("\"errorCode\":\"argument_error\""));
    assert!(!output.status.success());

    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["status", "--root="])
        .output()
        .expect("run cli");
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert!(text.contains("\"errorCode\":\"argument_error\""));
    assert!(!output.status.success());
}

fn run_hook_adapter(input: &[u8]) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["hook", "adapter"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook adapter");
    child
        .stdin
        .take()
        .expect("hook stdin")
        .write_all(input)
        .expect("write hook input");
    child.wait_with_output().expect("hook output")
}

fn run_hook_route(payload: &serde_json::Value) -> serde_json::Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["hook", "route"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook route");
    child
        .stdin
        .take()
        .expect("hook route stdin")
        .write_all(&serde_json::to_vec(payload).expect("hook route payload"))
        .expect("write hook route input");
    let output = child.wait_with_output().expect("hook route output");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    serde_json::from_slice(&output.stdout).expect("hook route JSON")
}

#[test]
fn rust_hook_route_matches_prompt_router_boundaries_without_retaining_prompt() {
    let routed = [
        "$mission-center",
        "不要用一般 plan；請用 $mission-center",
        "Do not use generic plan; please use @Mission Center",
        "請規劃一個高影響的多步驟專案",
        "规划一个高风险多步骤项目",
        "Create a project plan for a high-impact multi-step migration",
        "高リスクの複数ステップのプロジェクト計画",
        "고위험 다단계 프로젝트 계획",
    ];
    for prompt in routed {
        let payload = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": prompt,
            "cwd": "does-not-exist",
            "secret": "must never be echoed",
        });
        let result = run_hook_route(&payload);
        assert_eq!(
            result["hookSpecificOutput"]["hookEventName"], "UserPromptSubmit",
            "routed prompt: {prompt}"
        );
        assert!(result["hookSpecificOutput"]["additionalContext"].is_string());
        assert!(!serde_json::to_string(&result).unwrap().contains(prompt));
    }

    for prompt in [
        "Explain `$mission-center`",
        "引用 \"$mission-center\"",
        "不要規劃高影響多步驟專案",
        "不要用 $mission-center",
        "Do not invoke @Mission Center",
        "plan this",
        "goal",
        "continue",
        "請解釋高影響專案規劃",
    ] {
        let payload = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": prompt,
            "cwd": "does-not-exist",
        });
        assert_eq!(run_hook_route(&payload), serde_json::json!({}));
    }

    let root =
        workspace("| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | route | Ready |\n");
    for prompt in ["resume Mission Center work", " G\n O！", "OK…"] {
        let payload = serde_json::json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": prompt,
            "cwd": root,
        });
        let result = run_hook_route(&payload);
        assert!(result["hookSpecificOutput"]["additionalContext"].is_string());
    }
    let _ = fs::remove_dir_all(root);
}

#[test]
fn rust_hook_adapter_is_bounded_non_sensitive_and_fail_closed() {
    let capability = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["hook", "capability"])
        .output()
        .expect("run hook capability");
    let capability_payload = assert_machine_envelope(&capability, "hook", 0);
    assert_eq!(capability_payload["data"]["supported"], true);
    assert_eq!(capability_payload["data"]["promptRetained"], false);
    assert_eq!(capability_payload["data"]["sideEffects"], false);
    assert_eq!(capability_payload["data"]["inputMaxBytes"], 64 * 1024);

    let valid = run_hook_adapter(
        br#"{"hook_event_name":"UserPromptSubmit","prompt":"secret prompt","cwd":"."}"#,
    );
    let valid_payload = assert_machine_envelope(&valid, "hook", 0);
    assert_eq!(valid_payload["status"], "ok");
    assert_eq!(valid_payload["data"]["accepted"], true);
    assert_eq!(valid_payload["data"]["promptRetained"], false);
    assert_eq!(valid_payload["data"]["sideEffects"], false);
    assert!(!String::from_utf8_lossy(&valid.stdout).contains("secret prompt"));

    let ignored = run_hook_adapter(br#"{"hook_event_name":"Stop"}"#);
    let ignored_payload = assert_machine_envelope(&ignored, "hook", 0);
    assert_eq!(ignored_payload["status"], "ignored");
    assert_eq!(ignored_payload["data"]["reason"], "invalid-hook-event");

    let invalid = run_hook_adapter(br#"not-json"#);
    let invalid_payload = assert_machine_envelope(&invalid, "hook", 1);
    assert_eq!(invalid_payload["errorCode"], "invalid_json");

    let oversized = run_hook_adapter(&vec![b'x'; 64 * 1024 + 1]);
    let oversized_payload = assert_machine_envelope(&oversized, "hook", 1);
    assert_eq!(oversized_payload["errorCode"], "stdin_too_large");
}

#[test]
fn stale_status_always_returns_failure() {
    let root =
        workspace("| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | stale | Ready |\n");
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["status", "--root"])
        .arg(&root)
        .output()
        .expect("run cli");
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert!(text.contains("\"status\":\"stale\""));
    assert!(!output.status.success());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn status_brief_bytes_count_utf8_bytes_not_scalar_count() {
    let root =
        workspace("| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | brief | Ready |\n");
    fs::write(root.join("MissionCenter/brief.md"), "繁中").expect("write brief");
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["status", "--root"])
        .arg(&root)
        .output()
        .expect("run status");
    let payload = assert_machine_envelope(&output, "status", 1);
    assert_eq!(payload["data"]["briefBytes"], 6);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn init_and_sync_are_versioned_idempotent_workspace_operations() {
    let root = workspace("| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | 初始 | Ready |\n");
    let init = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["init", "--root"])
        .arg(&root)
        .args([
            "--operation-id",
            "init-contract-1",
            "--timestamp",
            "2026-08-29T00:00:00Z",
            "--language",
            "zh-tw",
        ])
        .output()
        .expect("run init");
    let init_payload = assert_machine_envelope(&init, "init", 0);
    assert_eq!(init_payload["status"], "committed");
    assert_eq!(init_payload["data"]["changed"], true);
    assert!(root.join("MissionCenter/brief.md").is_file());
    assert!(root.join("MissionCenter/progress.md").is_file());
    assert!(
        String::from_utf8(fs::read(root.join("MissionCenter/tasks.md")).unwrap())
            .unwrap()
            .contains("MC-1")
    );

    let replay = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["init", "--root"])
        .arg(&root)
        .args([
            "--operation-id",
            "init-contract-1",
            "--timestamp",
            "2026-08-29T00:00:00Z",
            "--language",
            "zh-tw",
        ])
        .output()
        .expect("replay init");
    assert_eq!(
        assert_machine_envelope(&replay, "init", 0)["status"],
        "replay"
    );

    let sync = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["sync", "--root"])
        .arg(&root)
        .args([
            "--operation-id",
            "sync-contract-1",
            "--timestamp",
            "2026-08-29T00:00:01Z",
        ])
        .output()
        .expect("run sync");
    let sync_payload = assert_machine_envelope(&sync, "sync", 0);
    assert_eq!(sync_payload["status"], "committed");
    let progress = fs::read_to_string(root.join("MissionCenter/progress.md")).unwrap();
    assert!(progress.contains("進度條"));
    let status = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["status", "--root"])
        .arg(&root)
        .args(["--date", "2026-08-29"])
        .output()
        .expect("run status after sync");
    let status_payload = assert_machine_envelope(&status, "status", 0);
    assert_eq!(status_payload["data"]["dateFresh"], true);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn top_level_and_sync_help_are_discoverable_machine_envelopes() {
    for args in [vec!["--help"], vec!["help", "sync"], vec!["sync", "--help"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
            .args(args)
            .output()
            .expect("run help");
        assert!(output.status.success());
        let payload: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("machine help JSON");
        assert_eq!(payload["status"], "ok");
        assert!(payload["data"]["usage"].as_str().is_some());
    }
    let sync = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["sync", "--help"])
        .output()
        .expect("run sync help");
    let payload: serde_json::Value = serde_json::from_slice(&sync.stdout).expect("sync help JSON");
    let usage = payload["data"]["usage"].as_str().unwrap();
    assert!(usage.contains("--operation-id <id>"));
    assert!(usage.contains("--timestamp <RFC3339>"));
}

#[test]
fn reconcile_stale_is_nonfatal_but_status_stale_is_fatal() {
    let root =
        workspace("| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | stale | Ready |\n");
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["reconcile", "--root"])
        .arg(&root)
        .output()
        .expect("run cli");
    let text = String::from_utf8(output.stdout).expect("utf8");
    assert!(text.contains("\"status\":\"stale\""));
    assert!(output.status.success());
    let _ = fs::remove_dir_all(root);
}

#[test]
fn runtime_capability_and_replay_are_bounded_read_only_contracts() {
    let capability = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["runtime", "capability"])
        .output()
        .expect("run cli");
    let payload: serde_json::Value = serde_json::from_slice(&capability.stdout).expect("json");
    assert_eq!(payload["route"], "runtime");
    assert_eq!(payload["data"]["persistent"], false);
    assert_eq!(payload["data"]["transports"]["stdio"], true);

    let event = br#"{"schemaVersion":"1.0","eventId":"evt1","timestamp":"2026-08-29T00:00:00Z","provider":"codex","sessionId":"s1","threadId":null,"turnId":null,"agentId":"a1","parentAgentId":null,"taskIds":[],"eventType":"started","activity":"Working","attention":"none","sequence":1,"state":"working","activityKind":"tool_use"}
"#;
    let replay = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["runtime", "replay"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().expect("stdin").write_all(event)?;
            child.wait_with_output()
        })
        .expect("run replay");
    let payload: serde_json::Value = serde_json::from_slice(&replay.stdout).expect("json");
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["data"]["persistent"], false);
    assert_eq!(payload["data"]["state"]["sourceStatus"], "replay");
}

#[test]
fn bare_cli_and_bare_runtime_do_not_panic_on_empty_argument_slices() {
    let root =
        workspace("| ID | Title | Status |\n| --- | --- | --- |\n| MC-1 | ready | Ready |\n");
    let bare = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .current_dir(&root)
        .output()
        .expect("run bare cli");
    assert_ne!(bare.status.code(), Some(101));
    let bare_payload: serde_json::Value =
        serde_json::from_slice(&bare.stdout).expect("bare CLI JSON envelope");
    assert_eq!(bare_payload["command"], "status");

    let runtime = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .arg("runtime")
        .output()
        .expect("run bare runtime");
    let runtime_payload = assert_machine_envelope(&runtime, "runtime", 0);
    assert_eq!(runtime_payload["data"]["transports"]["stdio"], true);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn unsupported_mutations_return_operation_id_without_writing() {
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["publish", "--operation-id", "op-cli-contract"])
        .output()
        .expect("run cli");
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(payload["errorCode"], "unsupported");
    assert_eq!(payload["data"]["operationId"], "op-cli-contract");
    assert_eq!(payload["data"]["written"], false);
}

#[test]
fn native_publish_requires_explicit_package_and_destination() {
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["publish", "apply", "--operation-id", "cli-publish"])
        .output()
        .expect("run CLI");
    let payload = assert_machine_envelope(&output, "publish", 2);
    assert_eq!(payload["errorCode"], "argument_error");
}

#[test]
fn native_install_requires_explicit_package_and_destination() {
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["install", "apply", "--operation-id", "cli-native"])
        .output()
        .expect("run CLI");
    let payload = assert_machine_envelope(&output, "install", 2);
    assert_eq!(payload["errorCode"], "argument_error");
}

#[test]
fn native_registration_requires_explicit_roots_and_operation_id() {
    let binary = env!("CARGO_BIN_EXE_mission-center");
    let output = Command::new(binary)
        .args([
            "install",
            "register",
            "apply",
            "--operation-id",
            "registration-test",
        ])
        .output()
        .expect("run registration validation");
    let payload = assert_machine_envelope(&output, "install", 2);
    assert_eq!(payload["errorCode"], "argument_error");
}

#[test]
fn install_package_shorthand_uses_native_fail_closed_path() {
    let package = std::env::temp_dir().join(format!(
        "mission-center-cli-missing-package-{}",
        std::process::id()
    ));
    let destination = std::env::temp_dir().join(format!(
        "mission-center-cli-install-target-{}",
        std::process::id()
    ));
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args([
            "install",
            "--package",
            package.to_str().expect("package path"),
            "--destination",
            destination.to_str().expect("destination path"),
            "--operation-id",
            "cli-native-shorthand",
            "--platform",
            "windows-x86_64",
            "--version",
            "0.5.1",
        ])
        .output()
        .expect("run CLI");
    let payload = assert_machine_envelope(&output, "install", 1);
    assert_eq!(payload["errorCode"], "not_found");
    assert_ne!(payload["errorCode"], "unsupported");
    assert!(
        !destination.exists(),
        "failed validation must not create target"
    );
}

#[test]
fn runtime_unknown_flag_and_oversize_stdin_use_stable_exit_codes() {
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["runtime", "state", "--unknown"])
        .output()
        .expect("run cli");
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(payload["errorCode"], "argument_error");

    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["runtime", "replay"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child
                .stdin
                .take()
                .expect("stdin")
                .write_all(&vec![b'x'; 8 * 1024 * 1024 + 1])?;
            child.wait_with_output()
        })
        .expect("run cli");
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(payload["errorCode"], "stdin_too_large");
}

#[test]
fn runtime_rejects_privacy_content_without_persisting_input() {
    let event = br#"{"schemaVersion":"1.0","eventId":"evt-private","timestamp":"2026-08-29T00:00:00Z","provider":"codex","sessionId":"s1","threadId":null,"turnId":null,"agentId":"a1","parentAgentId":null,"taskIds":[],"eventType":"started","activity":"Working","attention":"none","sequence":1,"state":"working","activityKind":"tool_use","token":"do-not-emit"}
"#;
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["runtime", "stdio"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            child.stdin.take().expect("stdin").write_all(event)?;
            child.wait_with_output()
        })
        .expect("run cli");
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(payload["errorCode"], "privacy_violation");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("do-not-emit"));
}

#[test]
fn health_missing_flags_and_publish_unknown_mode_are_argument_errors() {
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["runtime", "health"])
        .output()
        .expect("run cli");
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(payload["errorCode"], "argument_error");

    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["publish", "unknown", "--operation-id", "op"])
        .output()
        .expect("run cli");
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(payload["errorCode"], "argument_error");
}

#[test]
fn hud_capability_reports_compile_time_assets_without_nonce() {
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["hud", "capability"])
        .output()
        .expect("run cli");
    let text = String::from_utf8(output.stdout).expect("utf8");
    let payload: serde_json::Value = serde_json::from_str(&text).expect("json");
    assert!(output.status.success());
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["data"]["embedded"], true);
    assert_eq!(payload["data"]["assets"].as_array().unwrap().len(), 8);
    assert_eq!(payload["data"]["launch"]["supported"], false);
    assert_eq!(payload["data"]["persistent"], true);
    assert_eq!(payload["data"]["crossProcessReuse"], true);
    assert_eq!(payload["data"]["lifecycle"], "managed-child");
    assert_eq!(payload["data"]["commands"]["hook"], true);
    assert_eq!(payload["data"]["managed"]["ttlSeconds"], 21600);
    assert!(!text.contains("sessionNonce"));
}

#[test]
fn hud_serve_once_is_filesystem_asset_independent_and_bounded() {
    let root = workspace("| ID | Title | Status |\n| --- | --- | --- |\n");
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["hud", "serve-once", "--root"])
        .arg(&root)
        .output()
        .expect("run cli");
    let payload = assert_machine_envelope(&output, "hud", 1);
    assert_eq!(payload["errorCode"], "unsupported");
    assert_eq!(payload["data"]["written"], false);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn hud_state_requires_bounded_stdin_and_launch_requires_foreground() {
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["hud", "serve-once", "--state", "-"])
        .output()
        .expect("run state cli");
    let payload = assert_machine_envelope(&output, "hud", 1);
    assert_eq!(payload["errorCode"], "unsupported");

    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["hud", "launch"])
        .output()
        .expect("run launch cli");
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(payload["errorCode"], "unsupported");
    assert_eq!(payload["data"]["foregroundOnly"], true);
}

#[test]
fn hud_serve_once_timeout_is_bounded_and_returns_stable_error() {
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["hud", "serve-once", "--timeout-ms", "100"])
        .output()
        .expect("run timeout cli");
    let error = assert_machine_envelope(&output, "hud", 1);
    assert_eq!(error["errorCode"], "unsupported");
}

#[test]
fn hud_foreground_stays_alive_until_stdin_eof() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["hud", "launch", "--foreground"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("run foreground cli");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(child.try_wait().expect("poll foreground cli").is_none());
    drop(child.stdin.take());
    let output = child.wait_with_output().expect("wait foreground cli");
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<_> = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    assert_eq!(
        lines.len(),
        1,
        "foreground must emit exactly one JSON envelope"
    );
    let serving: serde_json::Value = serde_json::from_str(lines[0]).expect("json");
    assert_eq!(serving["data"]["mode"], "launch");
    assert_eq!(
        serving["data"]["sidePanelIntent"]["type"],
        "mission-center/hud-side-panel"
    );
    assert_eq!(
        serving["data"]["sidePanelIntent"]["surface"],
        "codex-sidebar-or-preview"
    );
    assert_eq!(serving["data"]["sidePanelIntent"]["mode"], "reuse");
    assert_eq!(serving["data"]["presentation"]["status"], "not_requested");
    assert_eq!(
        serving["data"]["presentation"]["externalBrowserRequested"],
        false
    );
}

#[test]
fn hud_serve_once_open_is_rejected_before_server_start() {
    let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["hud", "serve-once", "--open"])
        .output()
        .expect("run cli");
    let payload = assert_machine_envelope(&output, "hud", 1);
    assert_eq!(payload["errorCode"], "unsupported");
}

#[test]
fn frozen_package_parser_rejects_duplicate_keys_unknown_fields_and_noncanonical_base64() {
    for package in [
        r#"{"format":"frozen-package-v1","schemaVersion":"1.0","files":[],"files":[]}"#,
        r#"{"format":"frozen-package-v1","schemaVersion":"1.0","extra":1,"files":[]}"#,
        r#"{"format":"frozen-package-v1","schemaVersion":"1.0","files":[{"path":"x","contentBase64":"AB==","executable":false}]}"#,
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
            .args([
                "publish",
                "verify",
                "--version",
                "0.5.1",
                "--platform",
                "windows-x86_64",
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child
                    .stdin
                    .take()
                    .expect("stdin")
                    .write_all(package.as_bytes())?;
                child.wait_with_output()
            })
            .expect("run cli");
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(payload["errorCode"], "invalid_manifest");
    }
}

#[test]
fn publish_select_keeps_the_top_level_publish_command_token() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args([
            "publish",
            "select",
            "--version",
            "0.5.1",
            "--platform",
            "windows-x86_64",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("run publish select");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(include_bytes!("../../ci/fixtures/publish-verify.json"))
        .expect("write package");
    let output = child.wait_with_output().expect("wait publish select");
    let payload = assert_machine_envelope(&output, "publish", 0);
    assert_eq!(payload["status"], "pass");
    assert_eq!(payload["data"]["selected"], true);
}

#[test]
fn publish_and_install_stage_emit_verified_non_mutating_receipts() {
    let package = include_bytes!("../../ci/fixtures/publish-verify.json");
    for args in [
        vec![
            "publish",
            "stage",
            "--operation-id",
            "cli-stage-publish",
            "--version",
            "0.5.1",
            "--platform",
            "windows-x86_64",
        ],
        vec![
            "install",
            "stage",
            "--operation-id",
            "cli-stage-install",
            "--version",
            "0.5.1",
            "--platform",
            "linux-x86_64",
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_mission-center"))
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut child| {
                use std::io::Write;
                child.stdin.take().expect("stdin").write_all(package)?;
                child.wait_with_output()
            })
            .expect("run staging cli");
        assert_eq!(output.status.code(), Some(0));
        assert!(output.stderr.is_empty());
        let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("json");
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["data"]["staged"], true);
        assert_eq!(payload["data"]["receipt"]["written"], false);
        assert_eq!(payload["data"]["receipt"]["rollbackSupported"], false);
    }
}

#[test]
fn policy_validation_envelope_escapes_quotes_and_newlines_on_stdout_only() {
    let input = r#"{"command\"\n":"value"}"#;
    let mut child = Command::new(env!("CARGO_BIN_EXE_mission-center"))
        .args(["security", "--input", "-"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run cli");
    use std::io::Write;
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(input.as_bytes())
        .expect("write input");
    let output = child.wait_with_output().expect("wait cli");
    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty(), "protocol must remain on stdout");

    let payload: serde_json::Value = serde_json::from_slice(&output.stdout).expect("stdout json");
    assert_eq!(payload["status"], "error");
    assert_eq!(payload["errorCode"], "validation_failed");
    assert_eq!(payload["error"]["code"], payload["errorCode"]);
    let message = payload["error"]["message"].as_str().expect("message");
    let remediation = payload["error"]["remediation"]
        .as_str()
        .expect("remediation");
    assert!(!message.is_empty() && message.len() <= 512);
    assert!(!remediation.is_empty() && remediation.len() <= 512);
    assert!(payload["data"]["errors"].as_array().is_some());
    assert!(String::from_utf8_lossy(&output.stdout).contains("\\n"));
}
