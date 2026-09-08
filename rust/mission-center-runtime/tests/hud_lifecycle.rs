use mission_center_runtime::{
    BrowserOpener, ErrorCode, FrozenHudAssets, HUD_MANAGED_ASSETS, HudLauncher, HudServerConfig,
    LaunchStatus, MAX_HOOK_INPUT_BYTES, parse_bounded_hook_input,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn temp_workspace(name: &str) -> PathBuf {
    let temp = std::env::temp_dir();
    #[cfg(target_os = "macos")]
    let temp = temp.canonicalize().expect("canonical temporary directory");
    let path = temp.join(format!(
        "mission-center-runtime-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(path.join("output/mission-center-assets")).unwrap();
    fs::create_dir_all(path.join("output/mission-center-runtime")).unwrap();
    for asset in HUD_MANAGED_ASSETS {
        fs::write(path.join("output/mission-center-assets").join(asset), asset).unwrap();
    }
    path
}

fn test_config(workspace: &Path) -> HudServerConfig {
    let config = HudServerConfig::new(workspace);
    #[cfg(windows)]
    {
        let files = HUD_MANAGED_ASSETS
            .iter()
            .map(|name| ((*name).to_owned(), name.as_bytes().to_vec()))
            .collect::<Vec<_>>();
        let state = config.state.clone();
        config.with_frozen_assets(FrozenHudAssets::from_state(files, state).unwrap())
    }
    #[cfg(not(windows))]
    {
        config
    }
}

fn test_config_with_large_optional_asset(workspace: &Path) -> HudServerConfig {
    let config = test_config(workspace);
    #[cfg(windows)]
    {
        let mut files = HUD_MANAGED_ASSETS
            .iter()
            .map(|name| ((*name).to_owned(), name.as_bytes().to_vec()))
            .collect::<Vec<_>>();
        files.push(("visual-state.json".to_owned(), vec![b'a'; 16 * 1024 * 1024]));
        let state = config.state.clone();
        config.with_frozen_assets(FrozenHudAssets::from_state(files, state).unwrap())
    }
    #[cfg(not(windows))]
    {
        config
    }
}

#[cfg(windows)]
#[test]
fn windows_filesystem_loader_fails_closed_before_server_start() {
    let workspace = temp_workspace("windows-path-policy");
    let before = fs::read_dir(workspace.join("output")).unwrap().count();
    let result = HudLauncher::new().launch(HudServerConfig::new(&workspace), false);
    assert_eq!(result.unwrap_err().code, ErrorCode::UnsafePath);
    assert_eq!(
        fs::read_dir(workspace.join("output")).unwrap().count(),
        before
    );
    assert_eq!(
        mission_center_runtime::fingerprint_hud_assets(&workspace.join("output"))
            .unwrap_err()
            .code,
        ErrorCode::UnsafePath
    );
    let _ = fs::remove_dir_all(workspace);
}

#[test]
fn frozen_bundle_constructor_enforces_allowlist_and_is_usable() {
    let files = HUD_MANAGED_ASSETS
        .iter()
        .map(|name| ((*name).to_owned(), name.as_bytes().to_vec()))
        .collect::<Vec<_>>();
    let state = HudServerConfig::new(std::env::temp_dir()).state;
    let bundle = FrozenHudAssets::from_state(files, state).unwrap();
    let workspace = temp_workspace("frozen-bundle");
    let config = HudServerConfig::new(&workspace).with_frozen_assets(bundle);
    let launcher = HudLauncher::new();
    let result = launcher.launch(config, false).unwrap();
    assert!(result.server.is_running());
    for _ in 0..16 {
        assert!(
            request(&result.server, "/mission-center-assets/visual-summary.html")
                .starts_with("HTTP/1.1 200")
        );
    }
    launcher.shutdown_all().unwrap();
    let _ = fs::remove_dir_all(workspace);
}

#[test]
fn accepted_connection_waits_for_request_within_timeout() {
    use std::io::{Read, Write};
    let workspace = temp_workspace("delayed-request");
    let launcher = HudLauncher::new();
    let result = launcher.launch(test_config(&workspace), false).unwrap();
    let mut stream = std::net::TcpStream::connect(result.server.address()).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    // Give the nonblocking listener time to accept before request bytes arrive.
    std::thread::sleep(Duration::from_millis(40));
    let request = format!(
        "GET /mission-center-assets/visual-summary.html HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        result.server.port()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 200"), "{response:?}");
    launcher.shutdown_all().unwrap();
    let _ = fs::remove_dir_all(workspace);
}

fn request(server: &mission_center_runtime::HudServer, path: &str) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(server.address()).unwrap();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        server.port()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    String::from_utf8(response).unwrap()
}

fn request_method(server: &mission_center_runtime::HudServer, method: &str, path: &str) -> String {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(server.address()).unwrap();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        server.port()
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    String::from_utf8(response).unwrap()
}

#[test]
fn launcher_reuses_only_matching_nonce_and_version_and_applies_cooldown() {
    let workspace = temp_workspace("reuse");
    let launcher = HudLauncher::new();
    let config = test_config(&workspace)
        .with_port(0)
        .with_nonce("nonce-a")
        .with_version("test-version");
    let first = launcher.launch(config.clone(), false).unwrap();
    assert_eq!(first.status, LaunchStatus::Launched);
    assert!(first.server.health_check().is_ok());
    let second = launcher.launch(config.clone(), false).unwrap();
    assert_eq!(second.status, LaunchStatus::Cooldown);
    assert_eq!(first.server.port(), second.server.port());
    let wrong_nonce = launcher.launch(config.clone().with_nonce("nonce-b"), false);
    assert_eq!(wrong_nonce.unwrap_err().code, ErrorCode::ReuseRejected);
    let wrong_version = launcher.launch(config.with_version("other-version"), false);
    assert_eq!(wrong_version.unwrap_err().code, ErrorCode::VersionMismatch);
    launcher.shutdown_all().unwrap();
    assert!(!first.server.is_running());
    let _ = fs::remove_dir_all(workspace);
}

#[test]
fn launcher_binds_random_or_requested_loopback_port_and_shutdown_closes_socket() {
    let workspace = temp_workspace("socket");
    let launcher = HudLauncher::new();
    let first = launcher
        .launch(test_config(&workspace).with_port(0), false)
        .unwrap();
    assert_ne!(first.server.port(), 0);
    assert!(first.server.address().ip().is_loopback());
    assert!(request(&first.server, "/_mission-center/health").starts_with("HTTP/1.1 200"));
    assert!(
        request_method(&first.server, "POST", "/_mission-center/health")
            .starts_with("HTTP/1.1 405")
    );
    assert!(
        request(&first.server, "/mission-center-runtime/runtime-state.json")
            .starts_with("HTTP/1.1 200")
    );
    let port = first.server.port();
    first.server.shutdown().unwrap();
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
    let specified = launcher
        .launch(test_config(&workspace).with_port(port), false)
        .unwrap();
    assert_eq!(specified.server.port(), port);
    specified.server.shutdown().unwrap();
    let _ = fs::remove_dir_all(workspace);
}

#[test]
#[cfg(unix)]
fn asset_change_stops_old_server_before_relaunch() {
    let workspace = temp_workspace("asset-change");
    let launcher = HudLauncher::new();
    let first = launcher.launch(test_config(&workspace), false).unwrap();
    let asset = workspace
        .join("output/mission-center-assets")
        .join("visual-summary.html");
    fs::write(asset, "changed").unwrap();
    let second = launcher.launch(test_config(&workspace), false).unwrap();
    assert!(!first.server.is_running());
    assert!(second.server.is_running());
    assert_ne!(
        first.server.hud_asset_fingerprint(),
        second.server.hud_asset_fingerprint()
    );
    launcher.shutdown_all().unwrap();
    let _ = fs::remove_dir_all(workspace);
}

#[test]
fn loaded_asset_map_is_immutable_and_allowlist_only() {
    let workspace = temp_workspace("frozen-assets");
    let launcher = HudLauncher::new();
    let result = launcher.launch(test_config(&workspace), false).unwrap();
    let before = request(&result.server, "/mission-center-assets/visual-summary.html");
    let assets = workspace.join("output/mission-center-assets");
    let moved = workspace.join("output/mission-center-assets-moved");
    fs::rename(&assets, &moved).unwrap();
    fs::create_dir_all(&assets).unwrap();
    fs::write(assets.join("secret.txt"), "not allowlisted").unwrap();
    fs::write(assets.join("visual-summary.html"), "replacement").unwrap();
    let after = request(&result.server, "/mission-center-assets/visual-summary.html");
    assert_eq!(before, after);
    assert!(
        request(&result.server, "/mission-center-assets/secret.txt").starts_with("HTTP/1.1 404")
    );
    result.server.shutdown().unwrap();
    let _ = fs::remove_dir_all(&assets);
    fs::rename(moved, assets).unwrap();
    let _ = fs::remove_dir_all(workspace);
}

#[test]
#[cfg(unix)]
fn oversized_asset_is_rejected_before_server_start() {
    let workspace = temp_workspace("oversized-asset");
    fs::write(
        workspace
            .join("output/mission-center-assets")
            .join("visual-summary.html"),
        vec![b'x'; mission_center_runtime::MAX_HTTP_BODY_BYTES + 1],
    )
    .unwrap();
    let error = HudLauncher::new()
        .launch(test_config(&workspace), false)
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::AssetUnavailable);
    let _ = fs::remove_dir_all(workspace);
}

#[test]
fn client_that_does_not_read_response_cannot_block_shutdown() {
    use std::io::Write;
    let workspace = temp_workspace("slow-client");
    // This optional allowlisted asset makes the response larger than a normal
    // socket send buffer without changing the launch fingerprint.
    fs::write(
        workspace
            .join("output/mission-center-assets")
            .join("visual-state.json"),
        vec![b'a'; 16 * 1024 * 1024],
    )
    .unwrap();
    let launcher = HudLauncher::new();
    let result = launcher
        .launch(test_config_with_large_optional_asset(&workspace), false)
        .unwrap();
    let mut client = std::net::TcpStream::connect(result.server.address()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let request = format!(
        "GET /mission-center-assets/visual-state.json HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n\r\n",
        result.server.port()
    );
    client.write_all(request.as_bytes()).unwrap();
    let started = Instant::now();
    result.server.shutdown().unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    let _ = fs::remove_dir_all(workspace);
}

#[test]
fn hook_input_is_bounded_and_does_not_retain_prompt() {
    let oversized = vec![b'x'; MAX_HOOK_INPUT_BYTES + 1];
    assert_eq!(
        parse_bounded_hook_input(&oversized).unwrap_err().code,
        ErrorCode::HookInputTooLarge
    );
    let parsed = parse_bounded_hook_input(
        br#"{"hook_event_name":"UserPromptSubmit","prompt":"secret prompt","cwd":"."}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(parsed.hook_event_name, "UserPromptSubmit");
    assert_eq!(parsed.cwd.as_deref(), Some("."));
    let valid = br#"{"hook_event_name":"x"}"#;
    let mut exact = valid.to_vec();
    exact.extend(std::iter::repeat_n(
        b' ',
        MAX_HOOK_INPUT_BYTES - valid.len(),
    ));
    assert_eq!(exact.len(), MAX_HOOK_INPUT_BYTES);
    assert!(parse_bounded_hook_input(&exact).is_ok());
    assert_eq!(
        parse_bounded_hook_input(&[0xff]).unwrap_err().code,
        ErrorCode::InvalidUtf8
    );
}

#[test]
fn health_tampering_is_rejected() {
    let err = mission_center_runtime::validate_health_payload(
        &serde_json::json!({
            "service":"mission-center-hud",
            "status":"ok",
            "version":"v",
            "workspaceFingerprint":"right",
            "sessionNonce":"wrong",
            "hudAssetFingerprint":"assets"
        }),
        "right",
        "nonce",
        "assets",
        "v",
    )
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::HealthMismatch);
}

#[test]
fn traversal_and_symlink_assets_fail_closed() {
    let workspace = temp_workspace("path");
    let launcher = HudLauncher::new();
    let server = launcher.launch(test_config(&workspace), false).unwrap();
    assert!(
        request(
            &server.server,
            "/mission-center-assets/%2e%2e/runtime-state.json"
        )
        .starts_with("HTTP/1.1 400")
    );
    server.server.shutdown().unwrap();
    let assets = workspace.join("output/mission-center-assets");
    let target = assets.join("mission-starfield.webp");
    let _ = fs::remove_file(&target);
    fs::write(workspace.join("outside"), "outside").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(workspace.join("outside"), &target).unwrap();
    #[cfg(windows)]
    if std::os::windows::fs::symlink_file(workspace.join("outside"), &target).is_err() {
        // Windows CI without SeCreateSymbolicLinkPrivilege cannot materialize
        // this fixture; containment is covered by the traversal assertion.
        let _ = fs::remove_dir_all(workspace);
        return;
    }
    assert_eq!(
        mission_center_runtime::fingerprint_hud_assets(&workspace.join("output"))
            .unwrap_err()
            .code,
        ErrorCode::UnsafePath
    );
    let _ = fs::remove_dir_all(workspace);
}

#[derive(Default)]
struct RecordingOpener(Arc<Mutex<Vec<String>>>);
impl BrowserOpener for RecordingOpener {
    fn open(&self, url: &str) -> bool {
        self.0.lock().unwrap().push(url.to_owned());
        false
    }
}

#[test]
fn opener_failure_keeps_server_usable() {
    let workspace = temp_workspace("opener");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let launcher = HudLauncher::with_opener(Arc::new(RecordingOpener(Arc::clone(&calls))));
    let result = launcher.launch(test_config(&workspace), true).unwrap();
    assert!(!result.browser_opened);
    assert_eq!(result.browser_status, "unavailable_server_kept");
    assert!(result.server.health_check().is_ok());
    assert_eq!(calls.lock().unwrap().len(), 1);
    launcher.shutdown_all().unwrap();
    let _ = fs::remove_dir_all(workspace);
}
