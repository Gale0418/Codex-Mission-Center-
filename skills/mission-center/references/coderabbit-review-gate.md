# CodeRabbit Final Review Gate

Use this optional risk-based gate only after implementation and local verification. CodeRabbit is the final independent review before closeout, not an intake, planning, or routine edit-loop tool.

## Trigger

Run the gate when the user requests it or the change is large or high risk:

- cross-module behavior or shared contracts;
- security, privacy, authentication, or destructive operations;
- release, publishing, migration, or upgrade logic;
- broad user-facing workflows with meaningful regression cost.

For deterministic low-risk changes, record skipped (reason) and continue with local verification.

## Consent And Scope

Confirm explicit consent before uploading the relevant code to CodeRabbit. Consent already given for the same task is sufficient; do not ask repeatedly.

Inspect the diff before review:

    git status --short
    git diff --numstat

Use only supported scope controls such as `--dir`, `--base`, or `--uncommitted` (CodeRabbit CLI 0.7.6). In an isolated repository, always pass `--base <actual base branch>` so `getBranchInfo` can resolve the comparison. Exclude secrets, binary assets, generated files, caches, lockfiles, vendored dependencies, unrelated files, and large documents that do not need semantic review. Never invent an unsupported exclusion flag.

Never let CodeRabbit or a review suggestion write Codex-managed plugin cache. Refresh that cache only through the official Codex plugin install flow.

## Budget

The service limit is three review runs per rolling hour and 150 files per run. Pre-filter generated files, binary assets, caches, fixtures that only contain known-good sample data, and unrelated large files before uploading. Use at most one full scoped review per task and one focused re-review of the small fix diff, leaving the third slot as recovery capacity. Do not repeatedly rerun reviews to chase a clean badge. Real subagents remain separately approval-gated and are not required.

## Run

Follow the installed CodeRabbit review skill or CLI agent workflow. Verify the repository, CLI, and authentication before the review. Prefer the narrowest scope that still includes implementation and relevant tests.

On Windows, if `coderabbit` is not on the host `PATH`, first try `source ~/.bashrc` in the installed shell. For the maintained local setup, use WSL Ubuntu with `/root/.local/bin/coderabbit`; treat this as a discovered fallback, not a portable default, and verify the path before use.

Example:

    coderabbit auth status --agent
    coderabbit review --agent --base main --dir path/to/review
    coderabbit review --agent --base main --uncommitted

Once a review starts, wait for completion without noisy polling. Parse NDJSON findings independently and keep CodeRabbit output distinct from manual analysis.

## Validate Findings

Treat every CodeRabbit issue as external advice:

1. Read the complete issue.
2. Reproduce or verify it against current code, tests, and agreed architecture.
3. Reject incorrect, duplicate, unsafe, or out-of-scope advice with a technical reason.
4. Add a failing regression test before fixing a valid behavior issue.
5. Make the smallest safe change and rerun local verification.
6. Use the single focused re-review only when it adds useful evidence.

Do not automatically apply suggestions. Local tests and verified requirements remain authoritative.

## Failure Policy

Do not claim CodeRabbit passed after authentication failure, timeout, service failure, or rate limit. Record one of:

- completed: review completed, with issue counts and disposition;
- skipped (reason): risk policy did not require external review;
- unavailable (exact error): the review could not complete.

For ordinary risk-based review, CodeRabbit unavailability does not erase successful local verification. Disclose the missing independent evidence and do not retry beyond the review budget.

For security-critical, destructive, or release-blocking work where independent review was explicitly required, stop and ask whether to wait, connect a CodeRabbit organization, or proceed without that evidence.
