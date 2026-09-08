#!/usr/bin/env python3
"""Publish the canonical Mission Center Skill to local derived locations."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import uuid
from pathlib import Path


PLUGIN_ITEMS = (
    ".codex-plugin",
    "assets",
    "hooks",
    "skills",
    "scripts",
    "README.md",
    "LICENSE",
    "NOTICE.md",
    "PRIVACY.md",
    "requirements-runtime.txt",
)
EXCLUDED_DIRS = {".git", "__pycache__", ".pytest_cache"}
EXCLUDED_SUFFIXES = {".pyc", ".pyo"}
FORMAL_EXCLUDED_SUFFIXES = {".py", ".pyc", ".pyo"}
FORMAL_EXCLUDED_PATHS = {
    Path(".codex-plugin/release-preview.json"),
    Path("requirements-runtime.txt"),
    Path("skills/mission-center/assets/visual-hub/update-visual-state.ps1"),
}
MARKETPLACE_CATEGORY_FALLBACK = "Productivity"
PLUGIN_NAME = "mission-center"
PLATFORM_MANIFEST = "platform-manifest.json"
SOURCE_RUNTIME_FILES = (
    Path("bin/mission-center"),
    Path("bin/mission-center.ps1"),
)
PLATFORM_SPECS = {
    "windows-x86_64": ("windows", "x86_64", "bin/windows-x86_64/mission-center.exe"),
    "linux-x86_64": ("linux", "x86_64", "bin/linux-x86_64/mission-center"),
    "macos-x86_64": ("macos", "x86_64", "bin/macos-x86_64/mission-center"),
    "macos-aarch64": ("macos", "aarch64", "bin/macos-aarch64/mission-center"),
}
SEMVER_PATTERN = re.compile(
    r"^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)"
    r"(?:-((?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*)(?:\.(?:0|[1-9]\d*|[0-9A-Za-z-]*[A-Za-z-][0-9A-Za-z-]*))*))?"
    r"(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$"
)


def is_excluded(relative: Path) -> bool:
    return any(part in EXCLUDED_DIRS for part in relative.parts) or (
        relative.suffix.lower() in EXCLUDED_SUFFIXES
    )


def load_json(path: Path) -> dict:
    return json.loads(path.read_text(encoding="utf-8"))


def load_strict_json_bytes(content: bytes, description: str) -> dict:
    def reject_duplicate_keys(pairs):
        result = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"{description} contains duplicate JSON key: {key}")
            result[key] = value
        return result

    try:
        value = json.loads(content.decode("utf-8"), object_pairs_hook=reject_duplicate_keys)
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f"{description} is not valid UTF-8 JSON") from exc
    if not isinstance(value, dict):
        raise ValueError(f"{description} must be a JSON object")
    return value


def validate_semver(version: object) -> str:
    value = str(version)
    if not SEMVER_PATTERN.fullmatch(value):
        raise ValueError(f"Plugin version must be SemVer: {value!r}")
    return value


def normalized_version(version: object) -> str:
    return validate_semver(version).split("+", 1)[0]


def load_plugin_manifest(repo: Path) -> dict:
    manifest_path = repo / ".codex-plugin" / "plugin.json"
    manifest = load_json(manifest_path)
    if not isinstance(manifest, dict):
        raise ValueError("Plugin manifest must be a JSON object")
    if manifest.get("name") != PLUGIN_NAME:
        raise ValueError(f"Plugin name must be {PLUGIN_NAME!r}")
    validate_semver(manifest.get("version"))
    return manifest


def normalize_plugin_manifest_bytes(content: bytes) -> bytes:
    manifest = load_strict_json_bytes(content, "Plugin manifest")
    if not isinstance(manifest, dict):
        raise ValueError("Plugin manifest must be a JSON object")
    if manifest.get("name") != PLUGIN_NAME:
        raise ValueError(f"Plugin name must be {PLUGIN_NAME!r}")
    manifest["version"] = normalized_version(manifest.get("version"))
    return json.dumps(manifest, sort_keys=True, ensure_ascii=False).encode("utf-8")


def normalize_platform_manifest_bytes(content: bytes) -> bytes:
    manifest = load_strict_json_bytes(content, "Platform manifest")
    manifest["version"] = normalized_version(manifest.get("version"))
    artifacts = manifest.get("artifacts")
    if isinstance(artifacts, list):
        for artifact in artifacts:
            if isinstance(artifact, dict):
                artifact["version"] = normalized_version(artifact.get("version"))
    return json.dumps(manifest, sort_keys=True, ensure_ascii=False).encode("utf-8")


def build_marketplace_manifest(plugin_manifest: dict) -> dict:
    if plugin_manifest.get("name") != PLUGIN_NAME:
        raise ValueError(f"Plugin name must be {PLUGIN_NAME!r}")
    validate_semver(plugin_manifest.get("version"))
    plugin_name = PLUGIN_NAME
    display_name = plugin_manifest.get("interface", {}).get("displayName", plugin_name)
    category = (
        plugin_manifest.get("interface", {}).get("category")
        or MARKETPLACE_CATEGORY_FALLBACK
    )
    return {
        "name": f"{plugin_name}-local",
        "interface": {"displayName": f"Local {display_name}"},
        "plugins": [
            {
                "name": plugin_name,
                "source": {
                    "source": "local",
                    "path": f"./plugins/{plugin_name}",
                },
                "policy": {
                    "installation": "AVAILABLE",
                    "authentication": "ON_INSTALL",
                },
                "category": category,
            }
        ],
    }


def serialize_marketplace_manifest(manifest: dict) -> bytes:
    return (json.dumps(manifest, indent=2, ensure_ascii=False) + "\n").encode("utf-8")


def stamped_plugin_manifest_bytes(plugin_manifest: dict) -> bytes:
    stamped = dict(plugin_manifest)
    version_prefix = normalized_version(plugin_manifest["version"])
    stamped["version"] = f"{version_prefix}+codex.{uuid.uuid4().hex}"
    return (json.dumps(stamped, indent=2, ensure_ascii=False) + "\n").encode("utf-8")


def stamped_platform_manifest_bytes(content: bytes, version: str) -> bytes:
    manifest = load_strict_json_bytes(content, "Platform manifest")
    manifest["version"] = version
    for artifact in manifest.get("artifacts", []):
        artifact["version"] = version
    return (json.dumps(manifest, indent=2, ensure_ascii=False) + "\n").encode("utf-8")


def iter_files(root: Path):
    if not root.exists():
        return
    ensure_source_tree(root)
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        relative = path.relative_to(root)
        if not is_excluded(relative):
            yield relative, path


def file_hash(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def normalized_hash(relative: Path, path: Path) -> str:
    if relative.as_posix().endswith(".codex-plugin/plugin.json"):
        content = normalize_plugin_manifest_bytes(path.read_bytes())
        return hashlib.sha256(content).hexdigest()
    if relative.as_posix().endswith(PLATFORM_MANIFEST):
        content = normalize_platform_manifest_bytes(path.read_bytes())
        return hashlib.sha256(content).hexdigest()
    return file_hash(path)


def file_map(root: Path) -> dict[str, str]:
    return {
        relative.as_posix(): normalized_hash(relative, path)
        for relative, path in iter_files(root)
    }


def is_formal_marketplace_excluded(relative: Path) -> bool:
    return (
        relative.parts[:1] == ("scripts",)
        or relative.suffix.lower() in FORMAL_EXCLUDED_SUFFIXES
        or relative in FORMAL_EXCLUDED_PATHS
    )


def iter_marketplace_source_files(
    repo: Path, release_package: Path | None = None
):
    formal_package = release_package is not None
    for name in PLUGIN_ITEMS:
        item = repo / name
        if item.is_file():
            relative = Path(name)
            if not formal_package or not is_formal_marketplace_excluded(relative):
                yield relative, item
        elif item.is_dir():
            for child_relative, path in iter_files(item):
                relative = Path(name) / child_relative
                if not formal_package or not is_formal_marketplace_excluded(relative):
                    yield relative, path


def marketplace_file_map(repo: Path, release_package: Path | None = None) -> dict[str, str]:
    plugin_manifest = load_plugin_manifest(repo)
    result: dict[str, str] = {}
    plugin_root = Path("plugins") / plugin_manifest["name"]
    for relative, path in iter_marketplace_source_files(repo, release_package):
        target = plugin_root / relative
        if target.as_posix().endswith(".codex-plugin/plugin.json"):
            content = normalize_plugin_manifest_bytes(path.read_bytes())
            result[target.as_posix()] = hashlib.sha256(content).hexdigest()
        else:
            result[target.as_posix()] = file_hash(path)
    for relative in SOURCE_RUNTIME_FILES:
        source = repo / relative
        if source.is_file():
            result[(plugin_root / relative).as_posix()] = normalized_hash(relative, source)
    if release_package is not None:
        release_manifest = release_package / PLATFORM_MANIFEST
        release_data = validate_release_package(
            release_package, normalized_version(plugin_manifest["version"])
        )
        result[(plugin_root / PLATFORM_MANIFEST).as_posix()] = normalized_hash(
            Path(PLATFORM_MANIFEST), release_manifest
        )
        artifacts = {artifact["platform"]: artifact for artifact in release_data["artifacts"]}
        for platform, (_, _, expected_path) in PLATFORM_SPECS.items():
            relative = Path(expected_path)
            source = release_package / relative
            if file_hash(source) != artifacts[platform]["sha256"].lower():
                raise ValueError(f"Verified Rust payload checksum mismatch: {relative}")
            result[(plugin_root / relative).as_posix()] = normalized_hash(relative, source)
    manifest_bytes = serialize_marketplace_manifest(build_marketplace_manifest(plugin_manifest))
    result[".agents/plugins/marketplace.json"] = hashlib.sha256(manifest_bytes).hexdigest()
    return result


def _binary_matches_platform(content: bytes, platform: str) -> bool:
    if platform == "windows-x86_64":
        if content[:2] != b"MZ" or len(content) < 64:
            return False
        offset = int.from_bytes(content[60:64], "little")
        return (
            len(content) >= offset + 6
            and content[offset : offset + 4] == b"PE\0\0"
            and int.from_bytes(content[offset + 4 : offset + 6], "little") == 0x8664
        )
    if platform == "linux-x86_64":
        return (
            content[:4] == b"\x7fELF"
            and len(content) >= 20
            and content[4] == 2
            and content[5] == 1
            and int.from_bytes(content[18:20], "little") == 62
        )
    if platform == "macos-x86_64":
        return (
            len(content) >= 8
            and content[:4] in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf")
            and int.from_bytes(content[4:8], "little" if content[:4] == b"\xcf\xfa\xed\xfe" else "big")
            == 0x01000007
        )
    if platform == "macos-aarch64":
        return (
            len(content) >= 8
            and content[:4] in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf")
            and int.from_bytes(content[4:8], "little" if content[:4] == b"\xcf\xfa\xed\xfe" else "big")
            == 0x0100000C
        )
    return False


def validate_release_package(package: Path, expected_version: str) -> dict:
    """Validate an already assembled frozen package without building anything."""
    reject_symlink_components(package, "release package")
    if not package.is_dir():
        raise ValueError(f"Verified Rust release package is not a directory: {package}")

    manifest_path = package / PLATFORM_MANIFEST
    plugin_path = package / ".codex-plugin" / "plugin.json"
    for path, label in ((manifest_path, "platform manifest"), (plugin_path, "plugin manifest")):
        reject_symlink_components(path, "release package")
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"Verified Rust release package is missing {label}: {path}")

    manifest = load_strict_json_bytes(manifest_path.read_bytes(), "Platform manifest")
    if set(manifest) != {"schemaVersion", "pluginName", "version", "artifacts"}:
        raise ValueError("Platform manifest fields do not match the Rust v1 contract")
    version = validate_semver(manifest.get("version"))
    if normalized_version(version) != normalized_version(expected_version):
        raise ValueError(
            f"Verified Rust release package version mismatch: expected {expected_version!r}, got {version!r}"
        )
    if manifest.get("schemaVersion") != "1.0" or manifest.get("pluginName") != PLUGIN_NAME:
        raise ValueError("Platform manifest schema or plugin name is invalid")
    artifacts = manifest.get("artifacts")
    if not isinstance(artifacts, list) or len(artifacts) != len(PLATFORM_SPECS):
        raise ValueError("Platform manifest must contain exactly four Rust artifacts")

    seen = set()
    for artifact in artifacts:
        if not isinstance(artifact, dict) or set(artifact) != {
            "platform",
            "path",
            "sha256",
            "version",
            "os",
            "arch",
            "executable",
        }:
            raise ValueError("Platform artifact fields do not match the Rust v1 contract")
        platform = artifact["platform"]
        if platform not in PLATFORM_SPECS or platform in seen:
            raise ValueError(f"Platform manifest contains an invalid or duplicate platform: {platform!r}")
        seen.add(platform)
        os_name, arch, expected_path = PLATFORM_SPECS[platform]
        if (
            artifact["path"] != expected_path
            or artifact["executable"] != expected_path
            or artifact["version"] != version
            or artifact["os"] != os_name
            or artifact["arch"] != arch
            or not isinstance(artifact["sha256"], str)
            or not re.fullmatch(r"[0-9a-fA-F]{64}", artifact["sha256"])
        ):
            raise ValueError(f"Platform artifact metadata is invalid: {platform}")
        binary = package / expected_path
        reject_symlink_components(binary, "release package")
        if binary.is_symlink() or not binary.is_file() or binary.stat().st_size == 0:
            raise ValueError(f"Verified Rust payload is missing or empty: {expected_path}")
        if os.name != "nt" and not os.access(binary, os.X_OK):
            raise ValueError(f"Verified Rust payload is not executable: {expected_path}")
        content = binary.read_bytes()
        if file_hash(binary) != artifact["sha256"].lower():
            raise ValueError(f"Verified Rust payload checksum mismatch: {expected_path}")
        if not _binary_matches_platform(content, platform):
            raise ValueError(f"Verified Rust payload magic/architecture mismatch: {platform}")

    if seen != set(PLATFORM_SPECS):
        raise ValueError("Platform manifest is missing one or more required Rust artifacts")
    plugin_manifest = load_strict_json_bytes(plugin_path.read_bytes(), "Plugin manifest")
    if (
        plugin_manifest.get("name") != PLUGIN_NAME
        or plugin_manifest.get("version") != version
    ):
        raise ValueError("Verified Rust package plugin.json does not match platform-manifest.json")
    return manifest


def resolve_release_package(
    repo: Path, requested: Path | None, plugin_manifest: dict
) -> Path | None:
    release_contract_path = repo / ".codex-plugin" / "release.json"
    expected_version = normalized_version(plugin_manifest["version"])
    if release_contract_path.is_file():
        contract = load_json(release_contract_path)
        if contract.get("runtime") != "rust" or contract.get("rustOnly") is not True:
            raise ValueError("Stable release contract must declare the Rust-only runtime")
        if normalized_version(contract.get("version")) != expected_version:
            raise ValueError("Stable release contract version does not match plugin.json")
        if requested is None:
            raise ValueError(
                "Stable Rust publishing requires --release-package pointing to an already verified frozen-package-v1; no build or download is performed"
            )
    if requested is None:
        return None
    package = requested.expanduser()
    validate_release_package(package, expected_version)
    return package.resolve()


def map_diff(expected: dict[str, str], actual: dict[str, str]) -> list[str]:
    changes = []
    for name in sorted(expected.keys() | actual.keys()):
        if name not in actual:
            changes.append(f"+ {name}")
        elif name not in expected:
            changes.append(f"- {name}")
        elif expected[name] != actual[name]:
            changes.append(f"M {name}")
    return changes


def validate_target(path: Path, expected_tail: tuple[str, str]) -> Path:
    candidate = path.expanduser()
    reject_symlink_components(candidate, "target")
    resolved = candidate.resolve()
    actual_tail = tuple(part.casefold() for part in resolved.parts[-2:])
    normalized_tail = tuple(part.casefold() for part in expected_tail)
    if actual_tail != normalized_tail:
        expected = "/".join(expected_tail)
        raise ValueError(f"Target must end with {expected}: {resolved}")
    return resolved


def reject_symlink_components(path: Path, label: str) -> None:
    """Reject symlink/junction components before resolving a source or target."""
    candidate = path.expanduser()
    if not candidate.is_absolute():
        candidate = Path.cwd() / candidate
    current = Path(candidate.anchor)
    for part in candidate.parts[1:]:
        current /= part
        if current.is_symlink():
            # macOS exposes a few fixed root aliases (for example /var ->
            # /private/var). These are part of the platform layout, not
            # user-controlled source or target redirects. All other symlink
            # components remain forbidden.
            trusted_macos_aliases = {
                Path("/etc"): Path("/private/etc"),
                Path("/tmp"): Path("/private/tmp"),
                Path("/var"): Path("/private/var"),
            }
            trusted_target = trusted_macos_aliases.get(current)
            if (
                sys.platform == "darwin"
                and trusted_target is not None
                and current.resolve() == trusted_target
            ):
                continue
            raise ValueError(f"{label} must not contain symlinks: {path}")


def ensure_source_tree(root: Path) -> None:
    if not root.exists():
        return
    reject_symlink_components(root, "source")
    root_resolved = root.resolve()
    for current, directories, files in os.walk(root, followlinks=False):
        current_path = Path(current)
        for name in [*directories, *files]:
            item = current_path / name
            if item.is_symlink():
                raise ValueError(f"Source tree must not contain symlinks: {item}")
            try:
                item.resolve().relative_to(root_resolved)
            except ValueError as exc:
                raise ValueError(f"Source path escapes repository: {item}") from exc


def assert_within(root: Path, child: Path, label: str) -> None:
    try:
        child.resolve().relative_to(root.resolve())
    except ValueError as exc:
        raise ValueError(f"{label} escapes its containing root: {child}") from exc


def copy_tree_contents(source: Path, destination: Path) -> None:
    for relative, path in iter_files(source):
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(path, target)


def skill_file_map(repo: Path) -> dict[str, str]:
    result = file_map(repo / "skills" / "mission-center")
    requirements = repo / "requirements-runtime.txt"
    if requirements.is_file():
        result["requirements-runtime.txt"] = file_hash(requirements)
    return result


def stage_skill(repo: Path, source: Path, staging: Path) -> None:
    copy_tree_contents(source, staging)
    requirements = repo / "requirements-runtime.txt"
    if requirements.is_file():
        shutil.copy2(requirements, staging / "requirements-runtime.txt")


def stage_marketplace(
    repo: Path,
    staging: Path,
    stamp_version: bool,
    release_package: Path | None = None,
) -> None:
    plugin_manifest = load_plugin_manifest(repo)
    plugin_root = staging / "plugins" / plugin_manifest["name"]
    release_data = None
    if release_package is not None:
        release_data = validate_release_package(
            release_package, normalized_version(plugin_manifest["version"])
        )
    for relative, source in iter_marketplace_source_files(repo, release_package):
        target = plugin_root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        if relative == Path(".codex-plugin"):
            raise ValueError("Unexpected file entry for .codex-plugin")
        shutil.copy2(source, target)

    for relative in SOURCE_RUNTIME_FILES:
        source = repo / relative
        if source.is_file():
            target = plugin_root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(source, target)

    if release_package is not None:
        assert release_data is not None
        (plugin_root / PLATFORM_MANIFEST).write_bytes(
            stamped_platform_manifest_bytes(
                json.dumps(release_data, ensure_ascii=False).encode("utf-8"),
                plugin_manifest["version"],
            )
        )
        artifacts = {artifact["platform"]: artifact for artifact in release_data["artifacts"]}
        for platform, (_, _, expected_path) in PLATFORM_SPECS.items():
            relative = Path(expected_path)
            target = plugin_root / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(release_package / relative, target)
            if file_hash(target) != artifacts[platform]["sha256"].lower():
                raise ValueError(
                    f"Verified Rust payload checksum mismatch after staging: {relative}"
                )

    manifest_path = plugin_root / ".codex-plugin" / "plugin.json"
    if manifest_path.is_file() and stamp_version:
        stamped = stamped_plugin_manifest_bytes(plugin_manifest)
        manifest_path.write_bytes(stamped)
        platform_manifest_path = plugin_root / PLATFORM_MANIFEST
        if platform_manifest_path.is_file():
            stamped_manifest = json.loads(stamped.decode("utf-8"))
            platform_manifest_path.write_bytes(
                stamped_platform_manifest_bytes(
                    platform_manifest_path.read_bytes(), stamped_manifest["version"]
                )
            )

    marketplace_manifest_path = staging / ".agents" / "plugins" / "marketplace.json"
    marketplace_manifest_path.parent.mkdir(parents=True, exist_ok=True)
    marketplace_manifest_path.write_bytes(
        serialize_marketplace_manifest(build_marketplace_manifest(plugin_manifest))
    )


def replace_from_stage(target: Path, stage_writer) -> None:
    transaction = prepare_file_transaction([(target, stage_writer)])
    try:
        transaction.commit()
    except Exception:
        transaction.rollback()
        raise
    transaction.finalize()


class FileTransaction:
    def __init__(self, entries: list[tuple[Path, Path | None, Path]]) -> None:
        self.entries = entries
        self.committed: list[tuple[Path, Path]] = []

    def commit(self) -> None:
        try:
            for target, staging, backup in self.entries:
                target.parent.mkdir(parents=True, exist_ok=True)
                if target.exists() or target.is_symlink():
                    target.rename(backup)
                if staging is not None:
                    staging.rename(target)
                self.committed.append((target, backup))
        except Exception:
            self.rollback()
            raise

    def rollback(self) -> None:
        for target, backup in reversed(self.committed):
            if target.exists() or target.is_symlink():
                shutil.rmtree(target)
            if backup.exists() or backup.is_symlink():
                backup.rename(target)
        self.committed.clear()
        for target, staging, backup in self.entries:
            if staging is not None and (staging.exists() or staging.is_symlink()):
                shutil.rmtree(staging)
            if backup.exists() or backup.is_symlink():
                # A backup not yet committed belongs to the original target.
                if not target.exists():
                    backup.rename(target)

    def finalize(self) -> None:
        for _, staging, backup in self.entries:
            if staging is not None and (staging.exists() or staging.is_symlink()):
                shutil.rmtree(staging)
            if backup.exists() or backup.is_symlink():
                shutil.rmtree(backup)


def prepare_file_transaction(
    writers: list[tuple[Path, object]],
    removals: list[Path] | None = None,
) -> FileTransaction:
    entries: list[tuple[Path, Path | None, Path]] = []
    try:
        for target, writer in writers:
            reject_symlink_components(target, "target")
            target.parent.mkdir(parents=True, exist_ok=True)
            token = uuid.uuid4().hex
            staging = target.parent / f".{target.name}.staging-{token}"
            backup = target.parent / f".{target.name}.backup-{token}"
            assert_within(target.parent, staging, "staging path")
            assert_within(target.parent, backup, "backup path")
            staging.mkdir()
            entries.append((target, staging, backup))
            writer(staging)
        for target in removals or []:
            reject_symlink_components(target, "target")
            if not target.exists() and not target.is_symlink():
                continue
            token = uuid.uuid4().hex
            backup = target.parent / f".{target.name}.backup-{token}"
            assert_within(target.parent, backup, "backup path")
            entries.append((target, None, backup))
        return FileTransaction(entries)
    except Exception:
        for _, staging, backup in entries:
            if staging is not None and (staging.exists() or staging.is_symlink()):
                shutil.rmtree(staging)
            if backup.exists() or backup.is_symlink():
                shutil.rmtree(backup)
        raise


def print_changes(label: str, changes: list[str]) -> None:
    print(f"[{label}]")
    if changes:
        for change in changes:
            print(change)
    else:
        print("no changes")


def verify_targets(
    canonical: Path,
    personal: Path | None,
    removed_personal: Path | None,
    repo: Path,
    marketplace_root: Path,
    cache_skill: Path | None,
    release_package: Path | None,
) -> bool:
    targets = [
        (
            "marketplace",
            marketplace_file_map(repo, release_package),
            file_map(marketplace_root),
        )
    ]
    if personal is not None:
        targets.insert(0, ("personal", skill_file_map(repo), file_map(personal)))
    if cache_skill is not None:
        targets.append(("cache", file_map(canonical), file_map(cache_skill)))

    valid = True
    for label, expected, actual in targets:
        changes = map_diff(expected, actual)
        print_changes(label, changes)
        valid = valid and not changes
    if removed_personal is not None:
        changes = ["- managed duplicate remains"] if removed_personal.exists() else []
        print_changes("legacy-personal", changes)
        valid = valid and not changes
    return valid


def _is_windows_platform() -> bool:
    """Return the host platform without making callers patch pathlib globals."""
    return os.name == "nt"


def is_usable_codex_executable(candidate: Path, *, from_path: bool = False) -> bool:
    """Return whether a candidate is a usable CLI file.

    WindowsApps command aliases can be discoverable through PATH while still
    rejecting direct subprocess launches.  They are intentionally ignored
    when discovered through PATH; an explicit path or CODEX_CLI_PATH remains
    an intentional override and is validated only as a file.
    """
    try:
        if not candidate.is_file() or not os.access(candidate, os.X_OK):
            return False
        if from_path and _is_windows_platform():
            return not any(part.casefold() == "windowsapps" for part in candidate.parts)
    except OSError:
        return False
    return True


def get_codex_executable(explicit: Path | None = None) -> Path | None:
    candidates: list[Path] = []
    if explicit is not None:
        candidates.append(explicit.expanduser())

    env_override = os.environ.get("CODEX_CLI_PATH")
    if env_override:
        candidates.append(Path(env_override).expanduser())

    codex_home_value = os.environ.get("CODEX_HOME")
    if not codex_home_value:
        user_home = os.environ.get("USERPROFILE") or os.environ.get("HOME") or os.path.expanduser("~")
        codex_home_value = os.path.join(user_home, ".codex")
    codex_home = Path(codex_home_value).expanduser()
    candidates.extend(
        [
            codex_home / ".sandbox-bin" / "codex",
            codex_home / ".sandbox-bin" / "codex.exe",
        ]
    )

    for candidate in candidates:
        if is_usable_codex_executable(candidate):
            return candidate.resolve()

    for name in ("codex", "codex.exe"):
        resolved = shutil.which(name)
        if resolved and is_usable_codex_executable(Path(resolved), from_path=True):
            return Path(resolved).resolve()

    return None


def register_marketplace_and_plugin(
    codex_executable: Path,
    marketplace_root: Path,
    plugin_manifest: dict,
) -> None:
    marketplace_name = f"{plugin_manifest['name']}-local"
    plugin_ref = f"{plugin_manifest['name']}@{marketplace_name}"
    previous_plugin = False
    previous_marketplace = False
    try:
        result = subprocess.run(
            [str(codex_executable), "plugin", "remove", plugin_ref],
            check=False,
        )
        # A successful remove proves that this registration existed before the
        # transaction and therefore must be recreated if a later add fails.
        previous_plugin = getattr(result, "returncode", 1) == 0
        result = subprocess.run(
            [str(codex_executable), "plugin", "marketplace", "remove", marketplace_name],
            check=False,
        )
        previous_marketplace = getattr(result, "returncode", 1) == 0
        subprocess.run(
            [str(codex_executable), "plugin", "marketplace", "add", str(marketplace_root)],
            check=True,
        )
        subprocess.run(
            [str(codex_executable), "plugin", "add", plugin_ref],
            check=True,
        )
    except Exception:
        # The CLI exposes remove/add but no portable transaction primitive. Rebuild
        # the prior local registration when a mutation fails; all rollback errors
        # are suppressed so the original failure remains visible to the caller.
        rollback_registration(
            codex_executable,
            marketplace_root,
            plugin_ref,
            marketplace_name,
            previous_marketplace,
            previous_plugin,
        )
        raise


def rollback_registration(
    codex_executable: Path,
    marketplace_root: Path,
    plugin_ref: str,
    marketplace_name: str,
    had_marketplace: bool,
    had_plugin: bool,
) -> None:
    def run(command: list[str]) -> None:
        try:
            subprocess.run([str(codex_executable), *command], check=False)
        except Exception:
            pass

    run(["plugin", "remove", plugin_ref])
    run(["plugin", "marketplace", "remove", marketplace_name])
    if had_marketplace:
        run(["plugin", "marketplace", "add", str(marketplace_root)])
    if had_plugin:
        run(["plugin", "add", plugin_ref])


def preflight(
    repo: Path,
    personal: Path | None,
    removed_personal: Path | None,
    marketplace: Path,
    cache_skill: Path | None,
    write: bool,
    register: bool,
    codex_cli: Path | None,
    release_package: Path | None,
) -> tuple[Path | None, Path | None, Path | None, dict, Path | None, Path | None]:
    reject_symlink_components(repo, "source repository")
    for name in PLUGIN_ITEMS:
        source = repo / name
        if source.is_symlink():
            raise ValueError(f"Published source must not be a symlink: {source}")
    canonical = repo / "skills" / PLUGIN_NAME
    manifest_path = repo / ".codex-plugin" / "plugin.json"
    if not (canonical / "SKILL.md").is_file():
        raise ValueError(f"Canonical Skill not found: {canonical}")
    if not manifest_path.is_file():
        raise ValueError(f"Plugin manifest not found: {repo}")
    plugin_manifest = load_plugin_manifest(repo)
    verified_release_package = resolve_release_package(repo, release_package, plugin_manifest)
    for relative in SOURCE_RUNTIME_FILES:
        source = repo / relative
        if source.is_symlink():
            raise ValueError(f"Published source must not be a symlink: {source}")
        if release_package is not None and not source.is_file():
            raise ValueError(f"Rust stable launcher is missing: {source}")
    if release_package is not None:
        launcher = repo / SOURCE_RUNTIME_FILES[0]
        if not os.access(launcher, os.X_OK):
            raise ValueError(f"Rust POSIX launcher is not executable: {launcher}")
    personal_target = (
        validate_target(personal, ("skills", PLUGIN_NAME))
        if personal is not None
        else None
    )
    removed_personal_target = (
        validate_target(removed_personal, ("skills", PLUGIN_NAME))
        if removed_personal is not None
        else None
    )
    if removed_personal_target == canonical:
        raise ValueError(
            "--remove-personal-skill must not target the canonical repository Skill"
        )
    if removed_personal_target is not None and removed_personal_target.exists():
        differences = map_diff(
            skill_file_map(repo),
            file_map(removed_personal_target),
        )
        if differences:
            raise RuntimeError(
                "Legacy personal Skill differs from the managed copy; refusing to remove it. "
                f"Back up or remove it manually: {removed_personal_target}"
            )
    marketplace_target = validate_target(marketplace, ("plugins", PLUGIN_NAME))
    marketplace_root = marketplace_target.parent.parent
    assert_within(marketplace_root, marketplace_target, "marketplace plugin target")
    marketplace_manifest = marketplace_root / ".agents" / "plugins" / "marketplace.json"
    assert_within(marketplace_root, marketplace_manifest, "marketplace manifest")
    cache_target = None
    if cache_skill is not None:
        cache_target = validate_target(cache_skill, ("skills", PLUGIN_NAME))
    if cache_target is not None and write:
        raise ValueError("--cache-skill is verify-only; cache is Codex-managed")
    codex_executable = None
    if register:
        codex_executable = get_codex_executable(codex_cli)
        if codex_executable is None:
            raise RuntimeError(
                "Codex executable not found. Set CODEX_CLI_PATH or pass --codex-cli before using --register."
            )
    # Build all source maps before any write; this also validates every derived
    # source path and catches symlink/escape attempts during --register preflight.
    skill_file_map(repo)
    marketplace_file_map(repo, verified_release_package)
    return (
        personal_target,
        removed_personal_target,
        cache_target,
        plugin_manifest,
        codex_executable,
        verified_release_package,
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Publish Mission Center from its canonical repository source."
    )
    parser.add_argument("--repo", required=True, type=Path)
    personal_mode = parser.add_mutually_exclusive_group()
    personal_mode.add_argument(
        "--personal-skill",
        type=Path,
        help="Optional compatibility copy. Omit to publish only the marketplace plugin.",
    )
    personal_mode.add_argument(
        "--remove-personal-skill",
        type=Path,
        help="Remove an exact managed legacy copy during a plugin-only upgrade.",
    )
    parser.add_argument("--marketplace-plugin", required=True, type=Path)
    parser.add_argument(
        "--release-package",
        type=Path,
        help="Already verified frozen-package-v1 directory containing the four Rust payloads; never builds or downloads",
    )
    parser.add_argument("--cache-skill", type=Path)
    parser.add_argument("--register", action="store_true")
    parser.add_argument("--codex-cli", type=Path)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--dry-run", action="store_true")
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--verify", action="store_true")
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    repo_input = args.repo.expanduser()
    reject_symlink_components(repo_input, "source repository")
    repo = repo_input.resolve()
    canonical = repo / "skills" / PLUGIN_NAME
    (
        personal,
        removed_personal,
        cache_skill,
        plugin_manifest,
        codex_executable,
        release_package,
    ) = preflight(
        repo,
        args.personal_skill,
        args.remove_personal_skill,
        args.marketplace_plugin,
        args.cache_skill,
        args.write,
        args.register,
        args.codex_cli,
        args.release_package,
    )
    marketplace = validate_target(args.marketplace_plugin, ("plugins", PLUGIN_NAME))
    marketplace_root = marketplace.parent.parent

    if args.dry_run:
        if personal is not None:
            print_changes("personal", map_diff(skill_file_map(repo), file_map(personal)))
        print_changes(
            "marketplace",
            map_diff(
                marketplace_file_map(repo, release_package),
                file_map(marketplace_root),
            ),
        )
        if cache_skill is not None:
            print_changes("cache", map_diff(file_map(canonical), file_map(cache_skill)))
        if removed_personal is not None:
            print_changes(
                "legacy-personal",
                ["- managed duplicate"] if removed_personal.exists() else [],
            )
        return 0

    if args.write:
        writers = []
        if personal is not None:
            writers.append(
                (personal, lambda staging: stage_skill(repo, canonical, staging))
            )
        writers.append(
            (
                marketplace_root,
                lambda staging: stage_marketplace(
                    repo,
                    staging,
                    stamp_version=args.register,
                    release_package=release_package,
                ),
            )
        )
        transaction = prepare_file_transaction(
            writers,
            removals=[removed_personal] if removed_personal is not None else [],
        )
        try:
            transaction.commit()
            if args.register and codex_executable is not None:
                register_marketplace_and_plugin(
                    codex_executable,
                    marketplace_root,
                    plugin_manifest,
                )
        except Exception:
            transaction.rollback()
            raise
        transaction.finalize()

    return 0 if verify_targets(
        canonical,
        personal,
        removed_personal,
        repo,
        marketplace_root,
        cache_skill,
        release_package,
    ) else 1


if __name__ == "__main__":
    raise SystemExit(main())
