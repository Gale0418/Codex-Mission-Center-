#!/usr/bin/env bash
set -euo pipefail

if [ "${MISSION_CENTER_PYTHON_COMPAT:-0}" != "1" ]; then
  echo "Python compatibility installer is disabled by default. Use a verified Rust package/binary for formal installation; set MISSION_CENTER_PYTHON_COMPAT=1 only for source-checkout compatibility publishing. This wrapper never builds or downloads a Rust package." >&2
  exit 1
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CODEX_ROOT="${CODEX_HOME:-$HOME/.codex}"
PERSONAL_SKILL="${MISSION_CENTER_PERSONAL_SKILL:-$CODEX_ROOT/skills/mission-center}"
MARKETPLACE_PLUGIN="${MISSION_CENTER_MARKETPLACE_PLUGIN:-$CODEX_ROOT/local-marketplaces/mission-center/plugins/mission-center}"
RELEASE_PACKAGE="${MISSION_CENTER_RELEASE_PACKAGE:-}"
MODE="${MISSION_CENTER_PUBLISH_MODE:---write}"
PYTHON_BIN="${MISSION_CENTER_PYTHON:-python3}"
WITH_PERSONAL_SKILL="${MISSION_CENTER_WITH_PERSONAL_SKILL:-0}"

if [ "${1:-}" = "--with-personal-skill" ]; then
  WITH_PERSONAL_SKILL=1
  shift
fi
if [ "$#" -ne 0 ]; then
  echo "Usage: $0 [--with-personal-skill]" >&2
  exit 2
fi

case "$MODE" in
  --dry-run|--write|--verify) ;;
  *) echo "MISSION_CENTER_PUBLISH_MODE must be --dry-run, --write, or --verify" >&2; exit 2 ;;
esac

ARGS=(--repo "$ROOT" --marketplace-plugin "$MARKETPLACE_PLUGIN" "$MODE")
if [ "$WITH_PERSONAL_SKILL" = "1" ]; then
  ARGS+=(--personal-skill "$PERSONAL_SKILL")
else
  ARGS+=(--remove-personal-skill "$PERSONAL_SKILL")
fi
if [ -n "$RELEASE_PACKAGE" ]; then
  ARGS+=(--release-package "$RELEASE_PACKAGE")
fi
if [ "$MODE" = "--write" ]; then
  ARGS+=(--register)
fi
"$PYTHON_BIN" "$ROOT/scripts/publish_local.py" "${ARGS[@]}"

case "$MODE" in
  --dry-run) echo "Dry-run completed. No files were modified." ;;
  --write) echo "Published Mission Center local marketplace plugin and refreshed Codex plugin registration." ;;
  --verify) echo "Verification completed successfully." ;;
esac
