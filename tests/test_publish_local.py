import json
import hashlib
import os
import shutil
import stat
import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import patch

from tests import workspace_tempdir


ROOT = Path(__file__).parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

from publish_local import (
    FileTransaction,
    PLATFORM_SPECS,
    get_codex_executable,
    is_usable_codex_executable,
    main,
    normalized_version,
    register_marketplace_and_plugin,
    reject_symlink_components,
    stage_marketplace,
    validate_target,
)


def write(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")


def make_fake_repo(root: Path) -> Path:
    repo = root / "repo"
    write(repo / ".codex-plugin" / "plugin.json", '{"name":"mission-center","version":"0.1.0"}\n')
    write(repo / "assets" / "icon.svg", "<svg/>\n")
    write(repo / "scripts" / "install.txt", "installer\n")
    write(repo / "README.md", "readme\n")
    write(repo / "LICENSE", "license\n")
    write(repo / "NOTICE.md", "notice\n")
    write(repo / "PRIVACY.md", "privacy\n")
    write(repo / "requirements-runtime.txt", "websockets>=16.1,<17\n")
    write(repo / "skills" / "mission-center" / "SKILL.md", "canonical\n")
    write(
        repo / "skills" / "mission-center" / "references" / "rules.md",
        "rules\n",
    )
    write(
        repo / "skills" / "mission-center" / "scripts" / "__pycache__" / "bad.pyc",
        "generated\n",
    )
    return repo


def fake_binary(platform: str) -> bytes:
    content = bytearray(128)
    if platform == "windows-x86_64":
        content[0:2] = b"MZ"
        content[60:64] = (64).to_bytes(4, "little")
        content[64:68] = b"PE\0\0"
        content[68:70] = (0x8664).to_bytes(2, "little")
    elif platform == "linux-x86_64":
        content[0:4] = b"\x7fELF"
        content[4] = 2
        content[5] = 1
        content[18:20] = (62).to_bytes(2, "little")
    elif platform == "macos-x86_64":
        content[0:4] = b"\xcf\xfa\xed\xfe"
        content[4:8] = (0x01000007).to_bytes(4, "little")
    else:
        content[0:4] = b"\xcf\xfa\xed\xfe"
        content[4:8] = (0x0100000C).to_bytes(4, "little")
    return bytes(content)


def make_stable_fake_repo(root: Path) -> Path:
    repo = make_fake_repo(root)
    write(
        repo / ".codex-plugin" / "release.json",
        '{"version":"0.1.0","runtime":"rust","rustOnly":true}\n',
    )
    write(repo / "bin" / "mission-center", "#!/bin/sh\nexit 0\n")
    write(repo / "bin" / "mission-center.ps1", "Write-Output 'mission-center'\n")
    for path in (repo / "bin" / "mission-center", repo / "bin" / "mission-center.ps1"):
        path.chmod(path.stat().st_mode | stat.S_IXUSR)
    return repo


def make_verified_release_package(root: Path) -> Path:
    package = root / "release-package"
    write(package / ".codex-plugin" / "plugin.json", '{"name":"mission-center","version":"0.1.0"}\n')
    artifacts = []
    for platform, (_, arch, relative) in PLATFORM_SPECS.items():
        payload = fake_binary(platform)
        path = package / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(payload)
        path.chmod(path.stat().st_mode | stat.S_IXUSR)
        artifacts.append(
            {
                "platform": platform,
                "path": relative,
                "sha256": hashlib.sha256(payload).hexdigest(),
                "version": "0.1.0",
                "os": relative.split("/", 2)[1].split("-")[0],
                "arch": arch,
                "executable": relative,
            }
        )
    write(
        package / "platform-manifest.json",
        json.dumps(
            {
                "schemaVersion": "1.0",
                "pluginName": "mission-center",
                "version": "0.1.0",
                "artifacts": artifacts,
            }
        ),
    )
    return package


class PublishLocalTests(unittest.TestCase):
    def test_rejects_user_controlled_symlink_component(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            real = root / "real"
            real.mkdir()
            link = root / "link"
            try:
                link.symlink_to(real, target_is_directory=True)
            except (NotImplementedError, OSError) as exc:
                if os.name == "nt" and getattr(exc, "winerror", None) == 1314:
                    self.skipTest("symlink creation requires SeCreateSymbolicLinkPrivilege")
                raise
            with self.assertRaisesRegex(ValueError, "must not contain symlinks"):
                reject_symlink_components(link / "child", "target")

    def test_codex_discovery_prefers_sandbox_and_rejects_windowsapps_path_alias(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            sandbox = root / ".sandbox-bin" / "codex.exe"
            sandbox.parent.mkdir(parents=True)
            sandbox.write_text("sandbox\n", encoding="utf-8")
            sandbox.chmod(sandbox.stat().st_mode | stat.S_IXUSR)
            alias = root / "WindowsApps" / "codex.exe"
            alias.parent.mkdir(parents=True)
            alias.write_text("alias\n", encoding="utf-8")
            alias.chmod(alias.stat().st_mode | stat.S_IXUSR)

            with patch.dict(
                os.environ,
                {
                    "CODEX_HOME": str(root),
                    "HOME": str(root),
                    "USERPROFILE": str(root),
                },
                clear=True,
            ):
                with patch("publish_local.shutil.which", return_value=str(alias)):
                    self.assertEqual(get_codex_executable(), sandbox.resolve())

                sandbox.unlink()
                with patch("publish_local.shutil.which", return_value=str(alias)):
                    with patch("publish_local._is_windows_platform", return_value=True):
                        self.assertIsNone(get_codex_executable())

    def test_codex_candidate_must_be_a_file(self):
        with workspace_tempdir("publish-local-") as temporary:
            directory = Path(temporary) / "codex.exe"
            directory.mkdir()
            self.assertFalse(is_usable_codex_executable(directory))

    def test_semver_normalization_discards_arbitrary_build_metadata(self):
        self.assertEqual(normalized_version("1.2.3-beta.2+vendor.build"), "1.2.3-beta.2")
        with self.assertRaises(ValueError):
            normalized_version("1.2")

    def test_stable_rust_publish_requires_release_package_before_writing(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_stable_fake_repo(root)
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            sentinel = marketplace / "keep.txt"
            write(sentinel, "keep\n")

            with self.assertRaisesRegex(ValueError, "requires --release-package"):
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--write",
                    ]
                )
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep\n")

    def test_stable_rust_publish_copies_launcher_and_verified_payload(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_stable_fake_repo(root)
            package = make_verified_release_package(root)
            marketplace = root / "marketplace" / "plugins" / "mission-center"

            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--release-package",
                        str(package),
                        "--write",
                    ]
                ),
                0,
            )
            self.assertTrue((marketplace / "bin" / "mission-center").is_file())
            self.assertTrue((marketplace / "bin" / "mission-center.ps1").is_file())
            self.assertTrue((marketplace / "platform-manifest.json").is_file())
            for _, _, relative in PLATFORM_SPECS.values():
                self.assertTrue((marketplace / relative).is_file())
            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--release-package",
                        str(package),
                        "--verify",
                    ]
                ),
                0,
            )

    def test_direct_stage_rejects_tampered_artifact_path_without_external_write(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_stable_fake_repo(root)
            package = make_verified_release_package(root)
            manifest_path = package / "platform-manifest.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
            manifest["artifacts"][0]["path"] = "../../../outside"
            manifest_path.write_text(json.dumps(manifest), encoding="utf-8")
            staging = root / "staging"
            outside = root / "outside"

            with self.assertRaisesRegex(ValueError, "artifact metadata is invalid"):
                stage_marketplace(
                    repo,
                    staging,
                    stamp_version=False,
                    release_package=package,
                )

            self.assertFalse(outside.exists())
            self.assertFalse(staging.exists())

    def test_stage_rejects_payload_changed_after_validation(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_stable_fake_repo(root)
            package = make_verified_release_package(root)
            payload = package / PLATFORM_SPECS["macos-aarch64"][2]
            original_copy = shutil.copy2

            def change_before_copy(source, target):
                if Path(source) == payload:
                    payload.write_bytes(payload.read_bytes() + b"changed-after-validation")
                return original_copy(source, target)

            with patch("publish_local.shutil.copy2", side_effect=change_before_copy):
                with self.assertRaisesRegex(ValueError, "checksum mismatch after staging"):
                    stage_marketplace(
                        repo,
                        root / "staging",
                        stamp_version=False,
                        release_package=package,
                    )

    def test_formal_package_excludes_compatibility_inputs_from_stage_and_map(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_stable_fake_repo(root)
            package = make_verified_release_package(root)
            write(repo / "scripts" / "compatibility.py", "compatibility\n")
            write(repo / "assets" / "compatibility.py", "compatibility\n")
            write(
                repo / "skills" / "mission-center" / "runtime.py",
                "compatibility\n",
            )
            write(
                repo / "skills" / "mission-center" / "assets" / "visual-hub" / "update-visual-state.ps1",
                "compatibility\n",
            )
            write(repo / ".codex-plugin" / "release-preview.json", "preview\n")
            marketplace = root / "marketplace" / "plugins" / "mission-center"

            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--release-package",
                        str(package),
                        "--write",
                    ]
                ),
                0,
            )
            for relative in (
                "scripts",
                "assets/compatibility.py",
                "skills/mission-center/runtime.py",
                "skills/mission-center/assets/visual-hub/update-visual-state.ps1",
                "requirements-runtime.txt",
                ".codex-plugin/release-preview.json",
            ):
                self.assertFalse(
                    (marketplace / relative).exists(),
                    f"formal package unexpectedly contains {relative}",
                )
            self.assertTrue((marketplace / ".codex-plugin" / "release.json").is_file())
            self.assertTrue((marketplace / "skills" / "mission-center" / "SKILL.md").is_file())
            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--release-package",
                        str(package),
                        "--verify",
                    ]
                ),
                0,
            )

    def test_legacy_marketplace_keeps_compatibility_scripts_without_release_package(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            write(repo / "scripts" / "compatibility.py", "compatibility\n")
            marketplace = root / "marketplace" / "plugins" / "mission-center"

            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--write",
                    ]
                ),
                0,
            )
            self.assertEqual(
                (marketplace / "scripts" / "compatibility.py").read_text(encoding="utf-8"),
                "compatibility\n",
            )

    def test_release_package_rejects_symlinked_manifest_parent(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_stable_fake_repo(root)
            package = make_verified_release_package(root)
            manifest_dir = package / ".codex-plugin"
            real_manifest_dir = root / "real-plugin-manifest"
            manifest_dir.rename(real_manifest_dir)
            try:
                manifest_dir.symlink_to(real_manifest_dir, target_is_directory=True)
            except (NotImplementedError, OSError) as exc:
                if os.name == "nt" and getattr(exc, "winerror", None) == 1314:
                    self.skipTest("symlink creation requires SeCreateSymbolicLinkPrivilege")
                raise
            marketplace = root / "marketplace" / "plugins" / "mission-center"

            with self.assertRaisesRegex(ValueError, "release package must not contain symlinks"):
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--release-package",
                        str(package),
                        "--write",
                    ]
                )
            self.assertFalse(marketplace.exists())

    def test_release_package_rejects_symlinked_binary_parent(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_stable_fake_repo(root)
            package = make_verified_release_package(root)
            binary_dir = package / "bin" / "linux-x86_64"
            real_binary_dir = root / "real-linux-x86_64"
            binary_dir.rename(real_binary_dir)
            try:
                binary_dir.symlink_to(real_binary_dir, target_is_directory=True)
            except (NotImplementedError, OSError) as exc:
                if os.name == "nt" and getattr(exc, "winerror", None) == 1314:
                    self.skipTest("symlink creation requires SeCreateSymbolicLinkPrivilege")
                raise
            marketplace = root / "marketplace" / "plugins" / "mission-center"

            with self.assertRaisesRegex(ValueError, "release package must not contain symlinks"):
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--release-package",
                        str(package),
                        "--write",
                    ]
                )
            self.assertFalse(marketplace.exists())

    def test_republishes_verified_package_with_old_cachebuster(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_stable_fake_repo(root)
            package = make_verified_release_package(root)
            manifest_path = package / "platform-manifest.json"
            manifest = json.loads(manifest_path.read_text())
            manifest["version"] = "0.1.0+codex.previous"
            for artifact in manifest["artifacts"]:
                artifact["version"] = manifest["version"]
            manifest_path.write_text(json.dumps(manifest))
            write(package / ".codex-plugin/plugin.json",
                  json.dumps({"name": "mission-center", "version": manifest["version"]}))
            write(repo / ".codex-plugin/plugin.json",
                  json.dumps({"name": "mission-center", "version": "0.1.0+codex.current"}))
            marketplace = root / "marketplace/plugins/mission-center"
            self.assertEqual(main([
                "--repo", str(repo), "--marketplace-plugin", str(marketplace),
                "--release-package", str(package), "--write",
            ]), 0)
            published = json.loads((marketplace / "platform-manifest.json").read_text())
            self.assertEqual(published["version"], "0.1.0+codex.current")
            self.assertTrue(all(a["version"] == published["version"] for a in published["artifacts"]))

    def test_stable_rust_publish_rejects_checksum_drift_before_writing(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_stable_fake_repo(root)
            package = make_verified_release_package(root)
            payload = package / PLATFORM_SPECS["macos-aarch64"][2]
            payload.write_bytes(payload.read_bytes() + b"drift")
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            sentinel = marketplace / "keep.txt"
            write(sentinel, "keep\n")

            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--release-package",
                        str(package),
                        "--write",
                    ]
                )
            self.assertEqual(sentinel.read_text(encoding="utf-8"), "keep\n")

    def test_register_stamps_plugin_and_platform_manifest_together(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_stable_fake_repo(root)
            package = make_verified_release_package(root)
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            fake_codex = root / "fake-codex"
            fake_codex.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            fake_codex.chmod(fake_codex.stat().st_mode | stat.S_IXUSR)

            with patch("publish_local.subprocess.run") as run_mock:
                run_mock.return_value.returncode = 0
                self.assertEqual(
                    main(
                        [
                            "--repo",
                            str(repo),
                            "--marketplace-plugin",
                            str(marketplace),
                            "--release-package",
                            str(package),
                            "--write",
                            "--register",
                            "--codex-cli",
                            str(fake_codex),
                        ]
                    ),
                    0,
                )
            plugin = json.loads(
                (marketplace / ".codex-plugin" / "plugin.json").read_text(encoding="utf-8")
            )
            platform_manifest = json.loads(
                (marketplace / "platform-manifest.json").read_text(encoding="utf-8")
            )
            self.assertTrue(plugin["version"].startswith("0.1.0+codex."))
            self.assertEqual(platform_manifest["version"], plugin["version"])
            self.assertTrue(
                all(artifact["version"] == plugin["version"] for artifact in platform_manifest["artifacts"])
            )

    def test_dry_run_does_not_create_targets(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            result = main(
                [
                    "--repo",
                    str(repo),
                    "--personal-skill",
                    str(personal),
                    "--marketplace-plugin",
                    str(marketplace),
                    "--dry-run",
                ]
            )
            self.assertEqual(result, 0)
            self.assertFalse(personal.exists())
            self.assertFalse(marketplace.exists())

    def test_preflight_ignores_unpublished_repository_trees(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            external = root / "external"
            external.mkdir()
            unrelated = repo / "rust" / "target" / "unrelated-link"
            unrelated.parent.mkdir(parents=True)
            try:
                unrelated.symlink_to(external, target_is_directory=True)
            except (NotImplementedError, OSError) as exc:
                if os.name == "nt" and getattr(exc, "winerror", None) == 1314:
                    self.skipTest("symlink creation requires SeCreateSymbolicLinkPrivilege")
                raise

            marketplace = root / "marketplace" / "plugins" / "mission-center"
            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--dry-run",
                    ]
                ),
                0,
            )

    def test_preflight_rejects_published_top_level_symlink(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            external = root / "external-readme.md"
            external.write_text("external\n", encoding="utf-8")
            readme = repo / "README.md"
            readme.unlink()
            try:
                readme.symlink_to(external)
            except (NotImplementedError, OSError) as exc:
                if os.name == "nt" and getattr(exc, "winerror", None) == 1314:
                    self.skipTest("symlink creation requires SeCreateSymbolicLinkPrivilege")
                raise

            marketplace = root / "marketplace" / "plugins" / "mission-center"
            with self.assertRaisesRegex(ValueError, "Published source must not be a symlink"):
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--dry-run",
                    ]
                )

    def test_plugin_only_write_and_verify_do_not_create_personal_skill(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"

            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--write",
                    ]
                ),
                0,
            )
            self.assertFalse(personal.exists())
            self.assertTrue((marketplace / ".codex-plugin" / "plugin.json").is_file())
            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--verify",
                    ]
                ),
                0,
            )

    def test_plugin_only_upgrade_removes_matching_legacy_personal_skill(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--personal-skill",
                        str(personal),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--write",
                    ]
                ),
                0,
            )

            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--remove-personal-skill",
                        str(personal),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--write",
                    ]
                ),
                0,
            )
            self.assertFalse(personal.exists())

    def test_plugin_only_upgrade_preserves_modified_personal_skill(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--personal-skill",
                        str(personal),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--write",
                    ]
                ),
                0,
            )
            write(personal / "custom.txt", "keep me\n")

            with self.assertRaisesRegex(RuntimeError, "differs from the managed copy"):
                main(
                    [
                        "--repo",
                        str(repo),
                        "--remove-personal-skill",
                        str(personal),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--write",
                    ]
                )
            self.assertEqual(
                (personal / "custom.txt").read_text(encoding="utf-8"),
                "keep me\n",
            )

    def test_remove_personal_skill_rejects_canonical_source(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            canonical = repo / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"

            with self.assertRaisesRegex(ValueError, "canonical repository Skill"):
                main(
                    [
                        "--repo",
                        str(repo),
                        "--remove-personal-skill",
                        str(canonical),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--write",
                    ]
                )
            self.assertTrue((canonical / "SKILL.md").is_file())

    def test_write_syncs_skill_and_plugin_without_generated_files(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            write(personal / "obsolete.txt", "remove\n")
            result = main(
                [
                    "--repo",
                    str(repo),
                    "--personal-skill",
                    str(personal),
                    "--marketplace-plugin",
                    str(marketplace),
                    "--write",
                ]
            )
            self.assertEqual(result, 0)
            self.assertEqual(
                (personal / "SKILL.md").read_text(encoding="utf-8"),
                "canonical\n",
            )
            self.assertEqual(
                (personal / "requirements-runtime.txt").read_text(encoding="utf-8"),
                "websockets>=16.1,<17\n",
            )
            self.assertFalse((personal / "obsolete.txt").exists())
            self.assertFalse((personal / "scripts" / "__pycache__").exists())
            self.assertTrue(
                (marketplace.parent.parent / ".agents" / "plugins" / "marketplace.json").is_file()
            )
            self.assertTrue((marketplace / ".codex-plugin" / "plugin.json").is_file())
            self.assertEqual(
                (marketplace / "PRIVACY.md").read_text(encoding="utf-8"),
                "privacy\n",
            )
            self.assertEqual(
                (marketplace / "requirements-runtime.txt").read_text(encoding="utf-8"),
                "websockets>=16.1,<17\n",
            )
            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--personal-skill",
                        str(personal),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--verify",
                    ]
                ),
                0,
            )

    def test_write_with_register_refreshes_plugin_version_and_calls_codex_cli(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            write(
                repo / ".codex-plugin" / "plugin.json",
                '{"name":"mission-center","version":"0.1.0+codex.previous","interface":{"displayName":"Mission Center","category":"Productivity"}}\n',
            )
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            fake_codex = root / "fake-codex"
            fake_codex.write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
            fake_codex.chmod(fake_codex.stat().st_mode | stat.S_IXUSR)

            with patch("publish_local.subprocess.run") as run_mock:
                run_mock.return_value.returncode = 0
                result = main(
                    [
                        "--repo",
                        str(repo),
                        "--personal-skill",
                        str(personal),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--write",
                        "--register",
                        "--codex-cli",
                        str(fake_codex),
                    ]
                )

            self.assertEqual(result, 0)
            manifest = (marketplace / ".codex-plugin" / "plugin.json").read_text(
                encoding="utf-8"
            )
            stamped_version = json.loads(manifest)["version"]
            self.assertEqual(stamped_version.count("+"), 1)
            self.assertTrue(stamped_version.startswith("0.1.0+codex."))
            self.assertNotIn("codex.previous", stamped_version)
            marketplace_manifest = (
                marketplace.parent.parent / ".agents" / "plugins" / "marketplace.json"
            ).read_text(encoding="utf-8")
            self.assertIn('"name": "mission-center-local"', marketplace_manifest)
            self.assertIn('"path": "./plugins/mission-center"', marketplace_manifest)
            expected_calls = [
                ([str(fake_codex), "plugin", "remove", "mission-center@mission-center-local"], False, {0}),
                ([str(fake_codex), "plugin", "marketplace", "remove", "mission-center-local"], False, {0}),
                ([str(fake_codex), "plugin", "marketplace", "add", str(marketplace.parent.parent)], True, {0, 4}),
                ([str(fake_codex), "plugin", "add", "mission-center@mission-center-local"], True, {0}),
            ]
            self.assertEqual(len(run_mock.call_args_list), len(expected_calls))
            for actual_call, (expected_command, expected_check, path_indexes) in zip(run_mock.call_args_list, expected_calls):
                actual_command = actual_call.args[0]
                self.assertEqual(len(actual_command), len(expected_command))
                for index, (actual, expected) in enumerate(zip(actual_command, expected_command)):
                    if index in path_indexes:
                        self.assertTrue(Path(actual).samefile(Path(expected)))
                    else:
                        self.assertEqual(actual, expected)
                self.assertEqual(actual_call.kwargs, {"check": expected_check})

    def test_verify_reports_drift(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            main(
                [
                    "--repo",
                    str(repo),
                    "--personal-skill",
                    str(personal),
                    "--marketplace-plugin",
                    str(marketplace),
                    "--write",
                ]
            )
            write(personal / "SKILL.md", "drifted\n")
            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--personal-skill",
                        str(personal),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--verify",
                    ]
                ),
                1,
            )

    def test_write_rejects_codex_managed_cache_target(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            cache = root / "cache" / "skills" / "mission-center"
            with self.assertRaisesRegex(ValueError, "Codex-managed"):
                main(
                    [
                        "--repo",
                        str(repo),
                        "--personal-skill",
                        str(personal),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--cache-skill",
                        str(cache),
                        "--write",
                    ]
                )

    def test_rejects_targets_outside_expected_tail(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            with self.assertRaisesRegex(ValueError, "skills/mission-center"):
                validate_target(root / "mission-center", ("skills", "mission-center"))
            with self.assertRaisesRegex(ValueError, "plugins/mission-center"):
                validate_target(root / "mission-center", ("plugins", "mission-center"))

    def test_verify_reports_plugin_drift_outside_skill_directory(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            main(
                [
                    "--repo",
                    str(repo),
                    "--personal-skill",
                    str(personal),
                    "--marketplace-plugin",
                    str(marketplace),
                    "--write",
                ]
            )
            write(marketplace / "assets" / "icon.svg", "drifted\n")
            self.assertEqual(
                main(
                    [
                        "--repo",
                        str(repo),
                        "--personal-skill",
                        str(personal),
                        "--marketplace-plugin",
                        str(marketplace),
                        "--verify",
                    ]
                ),
                1,
            )

    def test_register_requires_resolvable_codex_cli(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            write(
                repo / ".codex-plugin" / "plugin.json",
                '{"name":"mission-center","version":"0.1.0","interface":{"displayName":"Mission Center"}}\n',
            )
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            with patch("publish_local.get_codex_executable", return_value=None):
                with self.assertRaisesRegex(RuntimeError, "Codex executable not found"):
                    main(
                        [
                            "--repo",
                            str(repo),
                            "--personal-skill",
                            str(personal),
                            "--marketplace-plugin",
                            str(marketplace),
                            "--write",
                            "--register",
                        ]
                    )

    def test_register_preflight_happens_before_existing_targets_change(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            write(personal / "old.txt", "keep\n")
            write(marketplace / "old.txt", "keep\n")
            with patch("publish_local.get_codex_executable", return_value=None):
                with self.assertRaisesRegex(RuntimeError, "Codex executable not found"):
                    main([
                        "--repo", str(repo), "--personal-skill", str(personal),
                        "--marketplace-plugin", str(marketplace), "--write", "--register",
                    ])
            self.assertEqual((personal / "old.txt").read_text(encoding="utf-8"), "keep\n")
            self.assertEqual((marketplace / "old.txt").read_text(encoding="utf-8"), "keep\n")

    def test_registration_failure_rolls_back_both_published_targets(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            write(personal / "old.txt", "personal-old\n")
            write(marketplace / "old.txt", "marketplace-old\n")
            fake_codex = root / "fake-codex"
            fake_codex.write_text("fake\n", encoding="utf-8")
            fake_codex.chmod(fake_codex.stat().st_mode | stat.S_IXUSR)
            failure = subprocess.CalledProcessError(7, [str(fake_codex), "plugin", "marketplace", "add"])
            outcomes = [
                subprocess.CompletedProcess([], 0),
                subprocess.CompletedProcess([], 0),
                failure,
                *([subprocess.CompletedProcess([], 0)] * 4),
            ]
            with patch("publish_local.subprocess.run", side_effect=outcomes) as run:
                with self.assertRaises(subprocess.CalledProcessError):
                    main([
                        "--repo", str(repo), "--personal-skill", str(personal),
                        "--marketplace-plugin", str(marketplace), "--write", "--register",
                        "--codex-cli", str(fake_codex),
                    ])
            self.assertEqual((personal / "old.txt").read_text(encoding="utf-8"), "personal-old\n")
            self.assertEqual((marketplace / "old.txt").read_text(encoding="utf-8"), "marketplace-old\n")
            self.assertEqual(len(run.call_args_list), 7)
            self.assertFalse(any("staging-" in item.name or "backup-" in item.name for item in personal.parent.iterdir()))

    def test_registration_failure_restores_removed_legacy_personal_skill(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            repo = make_fake_repo(root)
            personal = root / "personal" / "skills" / "mission-center"
            marketplace = root / "marketplace" / "plugins" / "mission-center"
            self.assertEqual(
                main(
                    [
                        "--repo", str(repo),
                        "--personal-skill", str(personal),
                        "--marketplace-plugin", str(marketplace),
                        "--write",
                    ]
                ),
                0,
            )
            fake_codex = root / "fake-codex"
            fake_codex.write_text("fake\n", encoding="utf-8")
            fake_codex.chmod(fake_codex.stat().st_mode | stat.S_IXUSR)
            failure = subprocess.CalledProcessError(
                7,
                [str(fake_codex), "plugin", "marketplace", "add"],
            )
            outcomes = [
                subprocess.CompletedProcess([], 0),
                subprocess.CompletedProcess([], 0),
                failure,
                *([subprocess.CompletedProcess([], 0)] * 4),
            ]

            with patch("publish_local.subprocess.run", side_effect=outcomes):
                with self.assertRaises(subprocess.CalledProcessError):
                    main(
                        [
                            "--repo", str(repo),
                            "--remove-personal-skill", str(personal),
                            "--marketplace-plugin", str(marketplace),
                            "--write", "--register",
                            "--codex-cli", str(fake_codex),
                        ]
                    )

            self.assertEqual(
                (personal / "SKILL.md").read_text(encoding="utf-8"),
                "canonical\n",
            )
            self.assertFalse(
                any(
                    "staging-" in item.name or "backup-" in item.name
                    for item in personal.parent.iterdir()
                )
            )

    def test_file_transaction_rollback_is_idempotent_after_commit_failure(self):
        with workspace_tempdir("publish-local-") as temporary:
            root = Path(temporary)
            first_target = root / "first"
            first_target.mkdir()
            write(first_target / "state.txt", "original\n")
            first_staging = root / ".first.staging"
            first_staging.mkdir()
            write(first_staging / "state.txt", "replacement\n")
            second_target = root / "second"
            second_staging = root / ".second.staging"
            first_backup = root / ".first.backup"
            second_backup = root / ".second.backup"
            transaction = FileTransaction(
                [
                    (first_target, first_staging, first_backup),
                    (second_target, second_staging, second_backup),
                ]
            )

            with self.assertRaises(FileNotFoundError):
                transaction.commit()
            # commit() already rolls back its partial commit; the caller's
            # defensive rollback must not remove the restored target.
            transaction.rollback()
            self.assertEqual(
                (first_target / "state.txt").read_text(encoding="utf-8"),
                "original\n",
            )

    def test_registration_oserror_does_not_recreate_unknown_registrations(self):
        fake_codex = Path("codex")
        with patch("publish_local.subprocess.run", side_effect=OSError("unavailable")) as run:
            with self.assertRaisesRegex(OSError, "unavailable"):
                register_marketplace_and_plugin(
                    fake_codex,
                    Path("marketplace-root"),
                    {"name": "mission-center"},
                )
        commands = [call.args[0][1:] for call in run.call_args_list]
        self.assertEqual(
            commands,
            [
                ["plugin", "remove", "mission-center@mission-center-local"],
                ["plugin", "remove", "mission-center@mission-center-local"],
                ["plugin", "marketplace", "remove", "mission-center-local"],
            ],
        )


if __name__ == "__main__":
    unittest.main()
