import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("triage.py")
SPEC = importlib.util.spec_from_file_location("triage", MODULE_PATH)
assert SPEC and SPEC.loader
triage = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = triage
SPEC.loader.exec_module(triage)


def issue(number, title, body="", labels=None):
    return {
        "number": number,
        "title": title,
        "body": body,
        "labels": [{"name": label} for label in labels or []],
        "state": "open",
        "html_url": f"https://example.test/issues/{number}",
        "updated_at": "2026-10-05T00:00:00Z",
    }


class TriageTests(unittest.TestCase):
    def test_load_event_distinguishes_issue_and_pr(self):
        with tempfile.TemporaryDirectory() as directory:
            event = Path(directory) / "event.json"
            event.write_text(json.dumps({"issue": {"number": 12}}))
            ref, _ = triage.load_event(event)
            self.assertEqual((ref.kind, ref.number), ("issue", 12))

            event.write_text(json.dumps({"pull_request": {"number": 34}}))
            ref, _ = triage.load_event(event)
            self.assertEqual((ref.kind, ref.number), ("pull_request", 34))

            event.write_text(
                json.dumps(
                    {
                        "workflow_run": {
                            "event": "pull_request",
                            "pull_requests": [{"number": 56}],
                            "display_title": "Triage relay for PR #56",
                        }
                    }
                )
            )
            ref, _ = triage.load_event(event)
            self.assertEqual((ref.kind, ref.number), ("pull_request", 56))

    def test_related_ranking_prefers_explicit_and_file_matches(self):
        current = issue(10, "Faucet retry can pay twice", "Related to #12 and idempotency.", ["bug"])
        explicit = issue(12, "Persist faucet idempotency", "Retries need durable keys.", ["bug"])
        same_file = issue(13, "Unrelated wording", "Different report")
        same_file["pull_request"] = {"url": "https://api.example.test/pulls/13"}
        weak = issue(14, "Improve website SEO", "Metadata and sitemap.", ["enhancement"])

        ranked = triage.rank_related(
            current,
            [weak, same_file, explicit],
            current_files={"crates/ths-server/src/api.rs"},
            candidate_files={13: {"crates/ths-server/src/api.rs"}},
        )
        self.assertEqual([entry["number"] for entry in ranked[:2]], [12, 13])
        self.assertIn("explicitly referenced", ranked[0]["match_reasons"])
        self.assertTrue(any(reason.startswith("same files:") for reason in ranked[1]["match_reasons"]))

    def test_related_ranking_detects_reverse_reference(self):
        current = issue(10, "Wallet synchronization race")
        candidate = issue(11, "Publish canonical scan", "Fixes #10")
        ranked = triage.rank_related(current, [candidate])
        self.assertEqual(ranked[0]["number"], 11)
        self.assertIn("references current item", ranked[0]["match_reasons"])

    def test_extract_output_text_skips_reasoning_items(self):
        payload = {
            "output": [
                {"type": "reasoning", "summary": []},
                {"type": "message", "content": [{"type": "output_text", "text": '{"ok":true}'}]},
            ]
        }
        self.assertEqual(triage.extract_output_text(payload), '{"ok":true}')

    def test_split_telegram_preserves_content_and_limits_chunks(self):
        source = "\n\n".join(["x" * 900 for _ in range(8)])
        chunks = triage.split_telegram(source, 1_500)
        self.assertGreater(len(chunks), 1)
        self.assertTrue(all(len(chunk) <= 1_510 for chunk in chunks))
        rebuilt = "\n\n".join(chunk.split("\n", 1)[1] for chunk in chunks)
        self.assertEqual(rebuilt, source)

    def test_schema_objects_forbid_extra_properties(self):
        def inspect(node):
            if node.get("type") == "object":
                self.assertFalse(node.get("additionalProperties", True))
                self.assertEqual(set(node.get("properties", {})), set(node.get("required", [])))
                for child in node.get("properties", {}).values():
                    inspect(child)
            if node.get("type") == "array":
                inspect(node["items"])

        inspect(triage.REPORT_SCHEMA)

    def test_source_helpers_identify_layers_and_tests(self):
        self.assertEqual(triage.source_area("crates/ths-server/src/api.rs"), "server")
        self.assertEqual(triage.source_area("crates/ths-cli/src/main.rs"), "cli")
        self.assertEqual(triage.source_area("web/src/app/App.tsx"), "web")
        self.assertTrue(triage.is_test_path("web/src/app/App.test.tsx"))
        self.assertTrue(triage.is_test_path("crates/ths-server/tests/recovery.rs"))
        self.assertFalse(triage.is_test_path("crates/ths-server/src/api.rs"))

    def test_unsupported_relationships_are_removed(self):
        report = {
            "broader_pattern": {
                "related_items": [
                    {"number": 12, "type": "issue", "relationship": "same cause"},
                    {"number": 99, "type": "pull_request", "relationship": "invented"},
                ]
            }
        }
        context = {"related_candidates": [{"number": 12, "type": "issue"}]}
        discarded = triage.discard_unsupported_relationships(report, context)
        self.assertEqual(discarded, [99])
        self.assertEqual([item["number"] for item in report["broader_pattern"]["related_items"]], [12])


if __name__ == "__main__":
    unittest.main()
