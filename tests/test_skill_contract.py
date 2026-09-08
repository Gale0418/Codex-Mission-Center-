import json
import re
import unittest
from pathlib import Path


ROOT = Path(__file__).parents[1]
SKILL_ROOT = ROOT / "skills" / "mission-center"
SKILL_PATH = SKILL_ROOT / "SKILL.md"


class SkillContractTests(unittest.TestCase):
    def test_skill_is_a_small_intent_and_risk_router(self):
        text = SKILL_PATH.read_text(encoding="utf-8")
        self.assertLessEqual(len(text.encode("utf-8")), 6144)
        self.assertRegex(text, r"^---\nname: mission-center\n", re.MULTILINE)
        self.assertIn("Use when ", text)

        required_behaviors = (
            "唯一真實來源",
            "至多一個",
            "使用者核准",
            "working-set.md",
            "critical-lessons.md",
            "snapshot.md",
            "Runtime",
            "不得 Done",
            "check-only",
            "git commit",
            "不執行 sync 或 normalize",
        )
        for behavior in required_behaviors:
            with self.subTest(behavior=behavior):
                self.assertIn(behavior, text)
        self.assertRegex(
            text,
            r"sync --root \. --operation-id <id> --timestamp <RFC3339>",
        )

    def test_skill_routes_every_reference_in_one_markdown_hop(self):
        text = SKILL_PATH.read_text(encoding="utf-8")
        linked = set(re.findall(r"\]\((references/[^)]+)\)", text))
        reference_files = {
            f"references/{path.name}"
            for path in (SKILL_ROOT / "references").glob("*.md")
        }
        self.assertEqual(reference_files, linked)
        for relative in linked:
            self.assertTrue((SKILL_ROOT / relative).is_file(), relative)

    def test_skill_does_not_encode_large_protocols_or_bad_runtime_model(self):
        text = SKILL_PATH.read_text(encoding="utf-8").casefold()
        for phrase in (
            "active agent count",
            "one visible helper per active agent",
            "smoketest",
            "hook automatically runs",
        ):
            self.assertNotIn(phrase, text)
    def test_dynamic_expert_council_uses_complexity_evidence_and_approval_gates(self):
        council = (
            SKILL_ROOT / "references" / "dynamic-expert-council.md"
        ).read_text(encoding="utf-8")
        normalized = council.casefold()
        for phrase in (
            "`skip`",
            "`council_lite`",
            "`council_full`",
            "at least three dynamically selected professional perspectives",
            "improbable but feasible",
            "confirm and state the current date",
            "primary source",
            "jina reader",
            "do not invent",
            "evidence discipline",
            "exploration variance",
            "explicit approval",
            "agreed budget",
            "do not consume additional runtime-agent quota",
            "receives:",
            "not responsible for:",
            "low-confidence behavior:",
            "confidence plus unknowns",
            "separate from validators",
            "bounded retries",
            "material dissent",
            "next verification",
        ):
            self.assertIn(phrase, normalized)
        self.assertIn(
            "not by claiming different model settings or temperature values", normalized
        )
        self.assertIn("do not draw from a fixed role catalogue", normalized)

    def test_intake_and_creative_council_have_stop_and_convergence_rules(self):
        intake = (SKILL_ROOT / "references" / "intake-protocol.md").read_text(
            encoding="utf-8"
        )
        council = (SKILL_ROOT / "references" / "intake-council.md").read_text(
            encoding="utf-8"
        )
        self.assertIn("Ask exactly one question", intake)
        self.assertIn("Stop only when", intake)
        self.assertIn("Diverge", council)
        self.assertIn("Converge", council)
        self.assertIn("unexpected but feasible", council)

    def test_research_protocol_has_prior_art_jina_and_clean_room_rules(self):
        research = (
            SKILL_ROOT / "references" / "research-protocol.md"
        ).read_text(encoding="utf-8")
        for phrase in (
            "Prior Art Gate",
            "Jina Reader",
            "Jina Search",
            "Clean-room",
            "Pre-search idea | Source | Adopted insight | License status",
            "AGPL",
            "SSPL",
            "Representative GitHub Screening",
            "three to seven representative candidates",
            "never add weak candidates merely to fill a quota",
            "weak popularity signals",
            "Do not label a project maintained from one recent commit",
            "Adopt",
            "Adapt",
            "Learn",
            "Reject",
            "Temporary detailed screening",
        ):
            self.assertIn(phrase, research)

    def test_real_subagents_require_explicit_user_approval(self):
        orchestration = (
            SKILL_ROOT / "references" / "agent-orchestration.md"
        ).read_text(encoding="utf-8")
        self.assertIn("explicit user approval", orchestration)
        self.assertIn("simulated perspectives", orchestration)

    def test_completion_critic_council_is_budgeted_real_read_only_and_bounded(self):
        critic = (
            SKILL_ROOT / "references" / "completion-critic-council.md"
        ).read_text(encoding="utf-8")
        normalized = critic.casefold()
        for phrase in (
            "after local verification and applicable coderabbit review",
            "before `done` or closeout",
            "`skip`",
            "`critic_lite`",
            "`critic_full`",
            "real subagents, never simulated perspectives",
            "at least two independent critic subagents",
            "at least three independent critic subagents plus a separate evidence-arbiter subagent",
            "explicitly approved the total budget, per-seat budget, tool budget, and wall-clock budget",
            "cannot reach `done` or a clean closeout",
            "do not auto-downgrade to `skip`",
            "immutable snapshot",
            "taskid",
            "revision/hash",
            "parentsnapshot",
            "content-addressed",
            "read-only",
            "must not change `tasks.md`",
            "can never be represented as passing smoke evidence",
            "initial wave and one delta wave",
            "until clean",
            "critical",
            "unresolved `critical` findings block `done`",
            "approver identity, approval time",
            "tasks.md remains the only lifecycle truth",
        ):
            self.assertIn(phrase, normalized)

    def test_completion_critic_council_routes_artifacts_and_preserves_evidence_contract(self):
        critic = (
            SKILL_ROOT / "references" / "completion-critic-council.md"
        ).read_text(encoding="utf-8").casefold()
        for phrase in (
            "game / interactive",
            "visual / audio",
            "article / nonfiction",
            "fiction / dialogue",
            "ui / app",
            "cli / api / library",
            "non-perceptual",
            "journey/player",
            "visual/ux/accessibility",
            "audio/feel",
            "evidence arbiter",
            "clarity",
            "structure",
            "fact evidence",
            "voice",
            "continuity",
            "pacing",
            "cacc-<taskid>-<categoryslug>-<hash8>-<ordinal>",
            "evidence locator",
            "repro-or-read-path",
            "subjective preferences",
            "material dissent",
            "artifact modalities, user journey, audience, acceptance criteria, failure cost",
            "first launch, onboarding",
            "failure/retry",
            "fictional world facts",
            "disposable isolated runtime profile",
            "capability loss never reduces full below three critic subagents",
            "criticproposeddisposition",
            "chairfinaldisposition",
            "repaired finding and its delta-wave updates preserve that id",
            "blind sequential batches",
            "slot limits never reduce the required seat count",
            "output/mission-center-critique/<taskid>-<snapshotid>.json",
        ):
            self.assertIn(phrase, critic)

    def test_coderabbit_precedes_critic_gate_done_and_closeout(self):
        skill = SKILL_PATH.read_text(encoding="utf-8").casefold()
        gates = (SKILL_ROOT / "references" / "execution-gates.md").read_text(
            encoding="utf-8"
        ).casefold()
        closeout = (SKILL_ROOT / "references" / "closeout-format.md").read_text(
            encoding="utf-8"
        ).casefold()
        self.assertIn("references/coderabbit-review-gate.md", skill)
        self.assertIn("references/completion-critic-council.md", skill)
        self.assertLess(
            skill.index("references/coderabbit-review-gate.md"),
            skill.index("references/completion-critic-council.md"),
        )
        self.assertIn("run applicable coderabbit technical review first", gates)
        self.assertIn("before `done` or closeout", gates)
        self.assertIn("affected focused coderabbit review", gates)
        self.assertIn("completion critic council", closeout)
        self.assertIn("conditional section", closeout)
        self.assertIn(
            "prevents `done`, a shipped release, and clean closeout", closeout
        )

    def test_linear_and_execution_references_enforce_rolling_approval(self):
        linear = (SKILL_ROOT / "references" / "linear-parity.md").read_text(
            encoding="utf-8"
        )
        gates = (SKILL_ROOT / "references" / "execution-gates.md").read_text(
            encoding="utf-8"
        )
        workspace = (SKILL_ROOT / "references" / "task-workspace.md").read_text(
            encoding="utf-8"
        )
        self.assertIn("Rolling Planning", linear)
        self.assertIn("full Epic map", linear)
        self.assertIn("approved task draft", gates)
        self.assertIn("Do not write `tasks.md`", gates)
        self.assertIn(
            "Pre-search idea | Source | Adopted insight | License status",
            workspace,
        )

    def test_plugin_metadata_uses_current_license_and_repository(self):
        manifest = json.loads(
            (ROOT / ".codex-plugin" / "plugin.json").read_text(encoding="utf-8")
        )
        repository = "https://github.com/Gale0418/Codex-Mission-Center"
        self.assertEqual(manifest["license"], "MIT")
        self.assertEqual(manifest["homepage"], repository)
        self.assertEqual(manifest["repository"], repository)
        readme = (ROOT / "README.md").read_text(encoding="utf-8")
        self.assertNotIn("GPL-3.0", readme)

    def test_agent_prompt_covers_intake_research_and_approved_publish(self):
        agent = (SKILL_ROOT / "agents" / "openai.yaml").read_text(encoding="utf-8")
        normalized = agent.casefold()
        for phrase in (
            "bounded local context",
            "intent and risk",
            "tasks.md",
            "only lifecycle truth",
            "evidence before done",
        ):
            self.assertIn(phrase, normalized)

    def test_install_wrappers_delegate_without_mutating_marketplace_metadata(self):
        wrappers = (
            "install-windows.ps1",
            "install-unix.sh",
            "install-plugin-windows.ps1",
            "install-plugin-unix.sh",
        )
        for name in wrappers:
            text = (ROOT / "scripts" / name).read_text(encoding="utf-8")
            normalized = text.casefold()
            with self.subTest(name=name):
                self.assertIn("publish_local.py", normalized)
                self.assertNotIn("marketplace.json", normalized)
                self.assertNotIn("remove-item", normalized)
                self.assertNotIn("rm -rf", normalized)

    def test_install_wrappers_register_plugin_on_write_mode(self):
        wrappers = (
            "install-windows.ps1",
            "install-unix.sh",
            "install-plugin-windows.ps1",
            "install-plugin-unix.sh",
        )
        for name in wrappers:
            text = (ROOT / "scripts" / name).read_text(encoding="utf-8").casefold()
            with self.subTest(name=name):
                self.assertIn("--register", text)

    def test_install_wrappers_report_each_publish_mode_accurately(self):
        wrappers = (
            "install-windows.ps1",
            "install-unix.sh",
            "install-plugin-windows.ps1",
            "install-plugin-unix.sh",
        )
        for name in wrappers:
            text = (ROOT / "scripts" / name).read_text(encoding="utf-8").casefold()
            with self.subTest(name=name):
                self.assertIn("dry-run completed", text)
                self.assertIn("verification completed", text)
                self.assertIn("published mission center", text)

    def test_tests_use_portable_temp_directories_and_subprocess_timeouts(self):
        publish_tests = (ROOT / "tests" / "test_publish_local.py").read_text(
            encoding="utf-8"
        )
        workspace_tests = (
            ROOT / "tests" / "test_workspace_templates.py"
        ).read_text(encoding="utf-8")
        self.assertNotIn('dir="C:/tmp"', publish_tests)
        self.assertNotIn('dir="C:/tmp"', workspace_tests)
        self.assertIn("timeout=", workspace_tests)


    def test_coderabbit_gate_is_final_risk_based_and_quota_aware(self):
        skill = SKILL_PATH.read_text(encoding="utf-8")
        gate_path = SKILL_ROOT / "references" / "coderabbit-review-gate.md"
        self.assertIn("references/coderabbit-review-gate.md", skill)
        self.assertTrue(gate_path.is_file())

        gate = gate_path.read_text(encoding="utf-8")
        normalized = gate.casefold()
        for phrase in (
            "after implementation and local verification",
            "explicit consent",
            "risk-based",
            "--dir",
            "--base",
            "--uncommitted",
            "binary",
            "generated",
            "one full scoped review",
            "one focused re-review",
            "regression test",
            "rate limit",
            "do not claim coderabbit passed",
            "codex-managed plugin cache",
            "completed",
            "skipped",
            "unavailable",
        ):
            self.assertIn(phrase, normalized)



if __name__ == "__main__":
    unittest.main()
