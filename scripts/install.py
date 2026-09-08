#!/usr/bin/env python3
"""Explicit source-checkout compatibility entry point.

The formal plugin is installed from a verified Rust package. This wrapper is
kept only for source-checkout compatibility publishing and never acts as a
formal-runtime fallback.
"""

from __future__ import annotations

import os
import subprocess
import sys
import argparse
from pathlib import Path


COMPAT_OPT_IN = "MISSION_CENTER_PYTHON_COMPAT"


def require_compatibility_opt_in() -> bool:
    if os.environ.get(COMPAT_OPT_IN) != "1":
        print(
            "Python compatibility installer is disabled by default. "
            "Use a verified Rust package/binary for formal installation; "
            f"for source-checkout compatibility publishing, set {COMPAT_OPT_IN}=1. "
            "This wrapper never builds or downloads a Rust package.",
            file=sys.stderr,
        )
        return False
    return True


def build_publish_command(repo_root: Path, *, with_personal_skill: bool = False) -> list[str]:
    codex_home = Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")).expanduser()
    personal = Path(os.environ.get("MISSION_CENTER_PERSONAL_SKILL", codex_home / "skills" / "mission-center")).expanduser()
    marketplace = Path(os.environ.get("MISSION_CENTER_MARKETPLACE_PLUGIN", codex_home / "local-marketplaces" / "mission-center" / "plugins" / "mission-center")).expanduser()
    release_package = os.environ.get("MISSION_CENTER_RELEASE_PACKAGE")
    command = [
        sys.executable, str(repo_root / "scripts" / "publish_local.py"),
        "--repo", str(repo_root), "--marketplace-plugin", str(marketplace),
    ]
    if with_personal_skill or os.environ.get("MISSION_CENTER_WITH_PERSONAL_SKILL") == "1":
        command.extend(["--personal-skill", str(personal)])
    else:
        command.extend(["--remove-personal-skill", str(personal)])
    if release_package:
        command.extend(["--release-package", release_package])
    command.append("--write")
    if os.environ.get("MISSION_CENTER_PUBLISH_REGISTER", "1") != "0":
        command.append("--register")
    return command


def install(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Install the Mission Center local plugin.")
    parser.add_argument("--with-personal-skill", action="store_true")
    args = parser.parse_args(argv)
    if not require_compatibility_opt_in():
        return 1
    repo_root = Path(__file__).resolve().parent.parent
    completed = subprocess.run(
        build_publish_command(repo_root, with_personal_skill=args.with_personal_skill),
        check=False,
    )
    return int(completed.returncode)


if __name__ == "__main__":
    raise SystemExit(install())
