#!/usr/bin/env python3
"""Checks the pull request preview policy and the workflow that uses it.

    python3 tests/preview_workflow.py

The suite covers the decisions in `tools/preview/preview.py` and the contract
of `.github/workflows/preview.yml`: which events start a preview, where the
approval gate sits, and which job may see a secret.
"""

from __future__ import annotations

import contextlib
import io
import json
import os
import pathlib
import sys
import tempfile
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools" / "preview"))

import preview  # noqa: E402  (the module is loaded by path)

WORKFLOW = ROOT / ".github" / "workflows" / "preview.yml"


# --------------------------------------------------------------------------
# Helpers
# --------------------------------------------------------------------------


def block(text: str, header: str, indent: int) -> list[str]:
    """The lines of the block that starts with `header` at `indent` spaces.

    The workflow is checked as text on purpose: the rules below are about
    which keys a job carries, and a text block keeps the check readable
    without a YAML parser.
    """
    lines = text.splitlines()
    start = None
    for index, line in enumerate(lines):
        if line.strip() == header and len(line) - len(line.lstrip(" ")) == indent:
            start = index
            break
    if start is None:
        raise AssertionError(f"{header!r} does not start a block at {indent} spaces")
    collected = []
    for line in lines[start + 1 :]:
        if not line.strip():
            collected.append(line)
            continue
        if len(line) - len(line.lstrip(" ")) <= indent:
            break
        collected.append(line)
    return collected


def section(text: str, header: str, indent: int) -> str:
    """The text of the block that starts with `header`."""
    return "\n".join(block(text, header, indent))


class FakeClient(preview.GitHubClient):
    """A GitHub client that answers from memory."""

    def __init__(self, pull_requests=None, comments=None):
        super().__init__("owner/repo", "token")
        self.pull_requests = dict(pull_requests or {})
        self.comment_lists = dict(comments or {})
        self.created: list[tuple[int, str]] = []
        self.updated: list[tuple[int, str]] = []

    def pull_request(self, number: int) -> dict:
        return self.pull_requests.get(number, {})

    def comments(self, number: int) -> list[dict]:
        return list(self.comment_lists.get(number, []))

    def create_comment(self, number: int, body: str) -> dict:
        self.created.append((number, body))
        comment = {
            "id": 4242,
            "user": {"login": preview.STATUS_ACCOUNT},
            "author_association": "MEMBER",
            "body": body,
        }
        self.comment_lists.setdefault(number, []).append(comment)
        return comment

    def update_comment(self, comment_id: int, body: str) -> dict:
        self.updated.append((comment_id, body))
        for comments in self.comment_lists.values():
            for comment in comments:
                if comment.get("id") == comment_id:
                    comment["body"] = body
                    return comment
        return {"id": comment_id}


class FakeDocker:
    """A Docker command runner that answers from memory."""

    def __init__(self, labels: dict[str, str]):
        self.labels = dict(labels)
        self.calls: list[list[str]] = []

    def __call__(self, arguments) -> str:
        self.calls.append(list(arguments))
        if arguments[:2] == ["docker", "inspect"]:
            name = arguments[-1]
            if name not in self.labels:
                raise preview.PreviewError(f"Error response from daemon: No such object: {name}")
            return self.labels[name] + "\n"
        return ""


def comment_body(author: str, association: str, body: str) -> dict:
    """A comment as the API reports it."""
    return {
        "id": 1,
        "user": {"login": author},
        "author_association": association,
        "body": body,
    }


def status_comment(pr: int, blob: dict) -> dict:
    """A status comment as the workflow writes it."""
    approval = preview.Approval(
        sha=blob.get("sha", ""), by=blob.get("approved_by", "maintainer")
    )
    body = preview.status_comment_body(
        preview.Plan(action="deploy", pr=pr, sha=blob.get("sha", "")),
        url=blob.get("url", ""),
        approval=approval,
    )
    return {
        "id": 99,
        "user": {"login": preview.STATUS_ACCOUNT},
        "author_association": "MEMBER",
        "body": body,
    }


# --------------------------------------------------------------------------
# Names
# --------------------------------------------------------------------------


class NameTests(unittest.TestCase):
    def test_instance_name_uses_the_launcher_prefix(self):
        self.assertEqual(preview.instance_name(412), "preview-pr-412")

    def test_instance_name_matches_the_launcher_name_rules(self):
        for number in (1, 9, 10, 412, 99999):
            name = preview.instance_name(number)
            self.assertLessEqual(len(name), 40)
            self.assertTrue(name[0].isalpha())
            self.assertTrue(name.endswith(str(number)))
            for character in name:
                self.assertIn(character, "abcdefghijklmnopqrstuvwxyz0123456789-")

    def test_instance_name_rejects_a_number_that_is_not_positive(self):
        for number in (0, -1, -412):
            with self.assertRaises(preview.PreviewError):
                preview.instance_name(number)

    def test_instance_name_rejects_a_number_that_is_not_a_number(self):
        for number in (None, "412", 4.12, True, [412]):
            with self.assertRaises(preview.PreviewError):
                preview.instance_name(number)

    def test_resource_names_match_the_launcher_layout(self):
        names = preview.resource_names(412)
        self.assertEqual(
            names["containers"],
            [
                "ths-preview-pr-412-app",
                "ths-preview-pr-412-zakura",
                "ths-preview-pr-412-lightwalletd",
                "ths-preview-pr-412-init",
            ],
        )
        self.assertEqual(
            names["volumes"],
            [
                "ths-preview-pr-412-chain",
                "ths-preview-pr-412-wallet",
                "ths-preview-pr-412-lightwalletd",
                "ths-preview-pr-412-config",
            ],
        )
        self.assertEqual(names["network"], "ths-preview-pr-412")

    def test_resources_of_two_pull_requests_never_collide(self):
        first = preview.resource_names(1)
        second = preview.resource_names(2)
        everything = first["containers"] + first["volumes"] + [first["network"]]
        other = second["containers"] + second["volumes"] + [second["network"]]
        self.assertEqual(set(everything) & set(other), set())

    def test_resources_do_not_collide_with_a_developer_instance(self):
        names = preview.resource_names(412)
        everything = names["containers"] + names["volumes"] + [names["network"]]
        for name in everything:
            self.assertNotIn("ths-alice", name)


# --------------------------------------------------------------------------
# Ports
# --------------------------------------------------------------------------


class PortTests(unittest.TestCase):
    def test_offsets_are_multiples_of_ten(self):
        for offset in preview.candidate_offsets(412):
            self.assertEqual(offset % 10, 0)

    def test_offsets_stay_inside_the_port_range(self):
        for number in (1, 412, 99999):
            for offset in preview.candidate_offsets(number):
                ports = preview.instance_ports(offset)
                for port in ports.values():
                    self.assertGreater(port, 0)
                    self.assertLessEqual(port, 65535)

    def test_offsets_are_deterministic(self):
        self.assertEqual(preview.candidate_offsets(412), preview.candidate_offsets(412))

    def test_candidates_cover_every_slot_once(self):
        candidates = preview.candidate_offsets(412)
        self.assertEqual(len(candidates), preview.OFFSET_SLOTS)
        self.assertEqual(len(set(candidates)), len(candidates))

    def test_two_pull_requests_start_at_different_offsets(self):
        self.assertNotEqual(
            preview.candidate_offsets(1)[0], preview.candidate_offsets(2)[0]
        )

    def test_instance_ports_add_the_offset(self):
        ports = preview.instance_ports(20)
        self.assertEqual(ports["dashboard"], 32805 + 20)
        self.assertEqual(ports["rpc"], 18232 + 20)
        self.assertEqual(ports["p2p"], 18233 + 20)
        self.assertEqual(ports["lightwalletd"], 9067 + 20)

    def test_instance_ports_reject_an_offset_that_is_not_a_multiple_of_ten(self):
        for offset in (1, 15, -10):
            with self.assertRaises(preview.PreviewError):
                preview.instance_ports(offset)

    def test_instance_ports_reject_an_offset_that_overflows(self):
        with self.assertRaises(preview.PreviewError):
            preview.instance_ports(preview.MAX_PORT_OFFSET + 10)

    def test_allocate_offset_skips_ports_that_are_taken(self):
        first = preview.candidate_offsets(412)[0]
        busy = set(preview.instance_ports(first).values())
        offset = preview.allocate_offset(412, lambda port: port not in busy)
        self.assertNotEqual(offset, first)
        self.assertEqual(offset % 10, 0)
        self.assertFalse(busy & set(preview.instance_ports(offset).values()))

    def test_allocate_offset_fails_when_nothing_is_free(self):
        with self.assertRaises(preview.PreviewError):
            preview.allocate_offset(412, lambda port: False)

    def test_allocate_offset_is_stable_for_the_same_host(self):
        busy = {32805 + 30, 18232 + 30}
        first = preview.allocate_offset(412, lambda port: port not in busy)
        second = preview.allocate_offset(412, lambda port: port not in busy)
        self.assertEqual(first, second)

    def test_a_busy_host_moves_the_preview_to_another_offset(self):
        taken = preview.instance_ports(preview.candidate_offsets(412)[0])["dashboard"]
        offset = preview.allocate_offset(412, lambda port: port != taken)
        self.assertNotEqual(offset, preview.candidate_offsets(412)[0])


# --------------------------------------------------------------------------
# Addresses
# --------------------------------------------------------------------------


class UrlTests(unittest.TestCase):
    def test_url_replaces_the_placeholder(self):
        self.assertEqual(
            preview.preview_url("https://pr-{pr}.preview.example.com", 412),
            "https://pr-412.preview.example.com",
        )

    def test_url_requires_the_placeholder(self):
        with self.assertRaises(preview.PreviewError):
            preview.preview_url("https://preview.example.com", 412)

    def test_url_requires_an_http_address(self):
        with self.assertRaises(preview.PreviewError):
            preview.preview_url("ftp://pr-{pr}.example.com", 412)

    def test_url_rejects_a_pull_request_number_that_is_not_positive(self):
        with self.assertRaises(preview.PreviewError):
            preview.preview_url("https://pr-{pr}.example.com", 0)


# --------------------------------------------------------------------------
# State
# --------------------------------------------------------------------------


class StateTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = self.directory.name

    def test_state_round_trip(self):
        state = preview.PreviewState(
            pr=412,
            instance=preview.instance_name(412),
            sha="a" * 40,
            port_offset=20,
            url="https://pr-412.preview.example.com",
        )
        preview.write_state(self.root, state)
        stored = preview.read_state(self.root, 412)
        self.assertIsNotNone(stored)
        self.assertEqual(stored.sha, "a" * 40)
        self.assertEqual(stored.port_offset, 20)
        self.assertEqual(stored.url, "https://pr-412.preview.example.com")
        self.assertNotEqual(stored.updated_at, "")

    def test_state_is_absent_for_an_unknown_preview(self):
        self.assertIsNone(preview.read_state(self.root, 412))

    def test_state_rejects_a_corrupt_file(self):
        path = preview.state_path(self.root, 412)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text("{not json", encoding="utf-8")
        with self.assertRaises(preview.PreviewError):
            preview.read_state(self.root, 412)

    def test_clear_state_reports_whether_it_removed_anything(self):
        preview.write_state(
            self.root, preview.PreviewState(pr=412, instance=preview.instance_name(412))
        )
        self.assertTrue(preview.clear_state(self.root, 412))
        self.assertFalse(preview.clear_state(self.root, 412))

    def test_state_files_are_scoped_to_the_pull_request(self):
        preview.write_state(
            self.root, preview.PreviewState(pr=412, instance=preview.instance_name(412))
        )
        self.assertIsNone(preview.read_state(self.root, 413))
        self.assertNotEqual(
            preview.state_path(self.root, 412), preview.state_path(self.root, 413)
        )


# --------------------------------------------------------------------------
# Commands
# --------------------------------------------------------------------------


class CommandTests(unittest.TestCase):
    def test_command_is_recognised(self):
        self.assertEqual(preview.parse_command("/preview approve"), ("approve", ""))

    def test_command_is_case_insensitive(self):
        self.assertEqual(preview.parse_command("/PREVIEW Approve"), ("approve", ""))

    def test_command_may_have_leading_whitespace(self):
        self.assertEqual(preview.parse_command("\n  /preview approve\n"), ("approve", ""))

    def test_command_must_start_the_comment(self):
        self.assertIsNone(preview.parse_command("please /preview approve this"))

    def test_command_may_carry_arguments(self):
        self.assertEqual(
            preview.parse_command("/preview teardown now"), ("teardown", "now")
        )

    def test_unknown_command_is_not_a_command(self):
        for body in ("/preview destroy", "/preview", "/previewapprove", "hello"):
            self.assertIsNone(preview.parse_command(body))

    def test_empty_comment_is_not_a_command(self):
        self.assertIsNone(preview.parse_command(""))
        self.assertIsNone(preview.parse_command(None))

    def test_maintainer_associations(self):
        for association in ("OWNER", "MEMBER", "COLLABORATOR", "owner", "member"):
            self.assertTrue(preview.is_maintainer(association))
        for association in ("CONTRIBUTOR", "FIRST_TIME_CONTRIBUTOR", "NONE", "", "triage"):
            self.assertFalse(preview.is_maintainer(association))


# --------------------------------------------------------------------------
# Status comment
# --------------------------------------------------------------------------


class StatusCommentTests(unittest.TestCase):
    def test_body_carries_a_machine_readable_record(self):
        body = preview.status_comment_body(
            preview.Plan(action="deploy", pr=412, sha="b" * 40),
            url="https://pr-412.preview.example.com",
        )
        self.assertTrue(body.startswith(preview.STATUS_MARKER))
        blob = preview.read_status_blob({"body": body})
        self.assertEqual(blob["pr"], 412)
        self.assertEqual(blob["sha"], "b" * 40)
        self.assertEqual(blob["url"], "https://pr-412.preview.example.com")

    def test_body_reports_the_approver(self):
        body = preview.status_comment_body(
            preview.Plan(action="deploy", pr=412, sha="b" * 40, approved_by="maintainer"),
            url="https://pr-412.preview.example.com",
        )
        self.assertIn("@maintainer", body)
        blob = preview.read_status_blob({"body": body})
        self.assertEqual(blob["approved"]["by"], "maintainer")

    def test_body_reports_a_failure(self):
        body = preview.status_comment_body(
            preview.Plan(action="deploy", pr=412, sha="b" * 40), error="the image build failed"
        )
        self.assertIn("the image build failed", body)

    def test_body_explains_how_to_approve(self):
        body = preview.status_comment_body(
            preview.Plan(action="await-approval", pr=412, sha="b" * 40), state="await-approval"
        )
        self.assertIn("/preview approve", body)

    def test_body_reports_a_withdrawn_approval(self):
        body = preview.status_comment_body(
            preview.Plan(action="deny", pr=412, sha="b" * 40), state="denied"
        )
        self.assertIn("withdrawn", body)

    def test_blob_of_a_comment_without_a_marker(self):
        self.assertEqual(preview.read_status_blob({"body": "hello"}), {})
        self.assertEqual(preview.read_status_blob(None), {})

    def test_blob_of_a_broken_marker(self):
        self.assertEqual(preview.read_status_blob({"body": "<!-- ths-preview {oops -->"}), {})

    def test_find_status_comment_ignores_other_authors(self):
        comments = [
            comment_body("someone", "CONTRIBUTOR", f"{preview.STATUS_MARKER} {{}} -->"),
        ]
        self.assertIsNone(preview.find_status_comment(comments))

    def test_find_status_comment_finds_the_workflow_comment(self):
        comments = [
            comment_body("someone", "CONTRIBUTOR", "hello"),
            comment_body(preview.STATUS_ACCOUNT, "MEMBER", f"{preview.STATUS_MARKER} {{}} -->"),
        ]
        found = preview.find_status_comment(comments)
        self.assertIsNotNone(found)
        self.assertEqual(found["id"], 1)

    def test_upsert_creates_when_there_is_no_comment(self):
        client = FakeClient()
        preview.upsert_status_comment(client, 412, "body")
        self.assertEqual(len(client.created), 1)
        self.assertEqual(client.created[0][0], 412)
        self.assertEqual(client.updated, [])

    def test_upsert_updates_the_existing_comment(self):
        client = FakeClient(
            comments={
                412: [
                    {
                        "id": 7,
                        "user": {"login": preview.STATUS_ACCOUNT},
                        "body": f"{preview.STATUS_MARKER} {{}} -->",
                    }
                ]
            }
        )
        preview.upsert_status_comment(client, 412, "body")
        self.assertEqual(client.created, [])
        self.assertEqual(client.updated, [(7, "body")])


# --------------------------------------------------------------------------
# Plan
# --------------------------------------------------------------------------


class PlanTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = self.directory.name

    def pull_request_event(self, action: str, number: int = 412, sha: str = "c" * 40) -> dict:
        return {
            "action": action,
            "pull_request": {
                "number": number,
                "head": {"sha": sha},
            },
        }

    def comment_event(self, body: str, association: str = "MEMBER", number: int = 412) -> dict:
        return {
            "action": "created",
            "issue": {"number": number, "pull_request": {"url": "https://example.com"}},
            "comment": comment_body("maintainer", association, body),
        }

    def dispatch_event(self, pr: str = "412", action: str = "deploy") -> dict:
        return {"inputs": {"pr": pr, "action": action}}

    def client(self, sha: str = "c" * 40, comments=None) -> FakeClient:
        return FakeClient(
            pull_requests={412: {"number": 412, "head": {"sha": sha}}},
            comments=comments or {},
        )

    def test_opened_pull_request_awaits_approval(self):
        plan = preview.plan_for_event("pull_request", self.pull_request_event("opened"))
        self.assertEqual(plan.action, "await-approval")
        self.assertEqual(plan.pr, 412)
        self.assertEqual(plan.instance, "preview-pr-412")

    def test_the_trusted_pull_request_event_is_the_same(self):
        for event_name in preview.PULL_REQUEST_EVENTS:
            plan = preview.plan_for_event(event_name, self.pull_request_event("opened"))
            self.assertEqual(plan.action, "await-approval")
            self.assertEqual(plan.pr, 412)

    def test_the_workflow_event_name_is_understood(self):
        plan = preview.plan_for_event("pull_request_target", self.pull_request_event("closed"))
        self.assertEqual(plan.action, "teardown")
        self.assertEqual(plan.instance, "preview-pr-412")

    def test_synchronize_pull_request_awaits_approval(self):
        plan = preview.plan_for_event("pull_request", self.pull_request_event("synchronize"))
        self.assertEqual(plan.action, "await-approval")

    def test_reopened_pull_request_awaits_approval(self):
        plan = preview.plan_for_event("pull_request", self.pull_request_event("reopened"))
        self.assertEqual(plan.action, "await-approval")

    def test_closed_pull_request_tears_down(self):
        plan = preview.plan_for_event("pull_request", self.pull_request_event("closed"))
        self.assertEqual(plan.action, "teardown")
        self.assertEqual(plan.pr, 412)
        self.assertEqual(plan.instance, "preview-pr-412")

    def test_pull_request_event_that_does_not_change_the_preview_is_ignored(self):
        plan = preview.plan_for_event("pull_request", self.pull_request_event("labeled"))
        self.assertEqual(plan.action, "ignore")

    def test_pull_request_payload_without_a_number_is_ignored(self):
        plan = preview.plan_for_event("pull_request", {"action": "opened", "pull_request": {}})
        self.assertEqual(plan.action, "ignore")

    def test_pull_request_with_a_matching_approval_deploys(self):
        comment = status_comment(412, {"sha": "c" * 40, "url": "https://pr-412.example.com"})
        client = self.client(comments={412: [comment]})
        plan = preview.plan_for_event(
            "pull_request", self.pull_request_event("synchronize"), client, self.root
        )
        self.assertEqual(plan.action, "deploy")

    def test_pull_request_with_a_stale_approval_awaits_approval(self):
        comment = status_comment(412, {"sha": "a" * 40, "url": "https://pr-412.example.com"})
        client = self.client(comments={412: [comment]})
        plan = preview.plan_for_event(
            "pull_request", self.pull_request_event("synchronize"), client, self.root
        )
        self.assertEqual(plan.action, "await-approval")

    def test_state_file_records_an_approval_when_the_comment_is_gone(self):
        preview.write_state(
            self.root,
            preview.PreviewState(
                pr=412, instance=preview.instance_name(412), sha="c" * 40, port_offset=20
            ),
        )
        plan = preview.plan_for_event(
            "pull_request", self.pull_request_event("synchronize"), self.client(), self.root
        )
        self.assertEqual(plan.action, "deploy")

    def test_state_file_with_another_commit_does_not_approve(self):
        preview.write_state(
            self.root,
            preview.PreviewState(
                pr=412, instance=preview.instance_name(412), sha="a" * 40, port_offset=20
            ),
        )
        plan = preview.plan_for_event(
            "pull_request", self.pull_request_event("synchronize"), self.client(), self.root
        )
        self.assertEqual(plan.action, "await-approval")

    def test_comment_on_an_issue_is_ignored(self):
        event = self.comment_event("/preview approve")
        event["issue"].pop("pull_request")
        plan = preview.plan_for_event("issue_comment", event)
        self.assertEqual(plan.action, "ignore")

    def test_comment_without_a_command_is_ignored(self):
        plan = preview.plan_for_event("issue_comment", self.comment_event("looks good"))
        self.assertEqual(plan.action, "ignore")

    def test_comment_from_someone_without_write_access_is_ignored(self):
        event = self.comment_event("/preview approve", association="CONTRIBUTOR")
        plan = preview.plan_for_event("issue_comment", event)
        self.assertEqual(plan.action, "ignore")
        self.assertIn("not a maintainer", plan.reason)

    def test_approval_by_a_maintainer_deploys(self):
        plan = preview.plan_for_event(
            "issue_comment", self.comment_event("/preview approve"), self.client()
        )
        self.assertEqual(plan.action, "deploy")
        self.assertEqual(plan.sha, "c" * 40)
        self.assertEqual(plan.approved_by, "maintainer")

    def test_denial_is_recorded(self):
        plan = preview.plan_for_event("issue_comment", self.comment_event("/preview deny"))
        self.assertEqual(plan.action, "deny")

    def test_teardown_command_tears_down(self):
        plan = preview.plan_for_event("issue_comment", self.comment_event("/preview teardown"))
        self.assertEqual(plan.action, "teardown")

    def test_status_command_reports(self):
        plan = preview.plan_for_event("issue_comment", self.comment_event("/preview status"))
        self.assertEqual(plan.action, "status")

    def test_approval_without_a_readable_head_waits(self):
        plan = preview.plan_for_event(
            "issue_comment", self.comment_event("/preview approve"), FakeClient()
        )
        self.assertEqual(plan.action, "await-approval")

    def test_dispatch_deploy_without_approval_awaits(self):
        plan = preview.plan_for_event(
            "workflow_dispatch", self.dispatch_event(), self.client(), self.root
        )
        self.assertEqual(plan.action, "await-approval")

    def test_dispatch_deploy_with_approval_deploys(self):
        comment = status_comment(412, {"sha": "c" * 40, "url": "https://pr-412.example.com"})
        client = self.client(comments={412: [comment]})
        plan = preview.plan_for_event(
            "workflow_dispatch", self.dispatch_event(), client, self.root
        )
        self.assertEqual(plan.action, "deploy")

    def test_dispatch_teardown_tears_down(self):
        plan = preview.plan_for_event(
            "workflow_dispatch", self.dispatch_event(action="teardown"), self.client()
        )
        self.assertEqual(plan.action, "teardown")

    def test_dispatch_with_a_number_that_is_not_a_number_is_ignored(self):
        plan = preview.plan_for_event(
            "workflow_dispatch", self.dispatch_event(pr="not-a-number"), self.client()
        )
        self.assertEqual(plan.action, "ignore")

    def test_dispatch_with_an_unknown_action_is_ignored(self):
        plan = preview.plan_for_event(
            "workflow_dispatch", self.dispatch_event(action="destroy"), self.client()
        )
        self.assertEqual(plan.action, "ignore")

    def test_unknown_event_is_ignored(self):
        plan = preview.plan_for_event("push", {})
        self.assertEqual(plan.action, "ignore")

    def test_outputs_are_workflow_friendly(self):
        plan = preview.plan_for_event("pull_request", self.pull_request_event("closed"))
        outputs = plan.outputs()
        self.assertEqual(outputs["action"], "teardown")
        self.assertEqual(outputs["pr"], "412")
        for value in outputs.values():
            self.assertNotIn("\n", value)


# --------------------------------------------------------------------------
# Teardown
# --------------------------------------------------------------------------


class TeardownTests(unittest.TestCase):
    def owned(self) -> dict[str, str]:
        names = preview.resource_names(412)
        labels = {name: "preview-pr-412" for name in names["containers"]}
        labels.update({name: "preview-pr-412" for name in names["volumes"]})
        labels[names["network"]] = "preview-pr-412"
        return labels

    def test_owned_resources_are_removed(self):
        docker = FakeDocker(self.owned())
        removals = preview.teardown_resources(412, docker, apply=True)
        self.assertTrue(all(removal.removed for removal in removals))
        self.assertEqual(len(removals), 9)
        self.assertIn(["docker", "rm", "-f", "ths-preview-pr-412-app"], docker.calls)
        self.assertIn(["docker", "volume", "rm", "ths-preview-pr-412-chain"], docker.calls)
        self.assertIn(["docker", "network", "rm", "ths-preview-pr-412"], docker.calls)

    def test_resources_of_another_instance_are_kept(self):
        labels = self.owned()
        labels["ths-preview-pr-412-app"] = "alice"
        docker = FakeDocker(labels)
        removals = preview.teardown_resources(412, docker, apply=True)
        kept = [removal for removal in removals if "was kept" in removal.reason]
        self.assertEqual(len(kept), 1)
        self.assertEqual(kept[0].name, "ths-preview-pr-412-app")
        self.assertNotIn(["docker", "rm", "-f", "ths-preview-pr-412-app"], docker.calls)

    def test_missing_resources_are_reported(self):
        docker = FakeDocker({})
        removals = preview.teardown_resources(412, docker, apply=True)
        self.assertEqual(len(removals), 9)
        self.assertTrue(all("not present" in removal.reason for removal in removals))
        self.assertFalse(any(removal.removed for removal in removals))
        self.assertFalse(any(removal.failed for removal in removals))

    def test_a_broken_docker_is_reported(self):
        def broken(arguments):
            raise preview.PreviewError("Cannot connect to the Docker daemon")

        removals = preview.teardown_resources(412, broken, apply=True)
        self.assertTrue(all(removal.failed for removal in removals))

    def test_dry_run_removes_nothing(self):
        docker = FakeDocker(self.owned())
        removals = preview.teardown_resources(412, docker, apply=False)
        self.assertFalse(any(removal.removed for removal in removals))
        self.assertTrue(all("would be removed" in removal.reason for removal in removals))
        self.assertFalse([call for call in docker.calls if call[:2] != ["docker", "inspect"]])

    def test_every_resource_of_the_instance_is_covered(self):
        removals = preview.teardown_resources(412, FakeDocker(self.owned()), apply=False)
        names = preview.resource_names(412)
        covered = {removal.name for removal in removals}
        self.assertEqual(
            covered, set(names["containers"]) | set(names["volumes"]) | {names["network"]}
        )

    def test_the_launcher_metadata_is_removed(self):
        with tempfile.TemporaryDirectory() as directory:
            os.environ["XDG_CONFIG_HOME"] = directory
            self.addCleanup(os.environ.pop, "XDG_CONFIG_HOME", None)
            metadata = preview.launcher_metadata_directory(412)
            self.assertEqual(metadata.name, "preview-pr-412")
            self.assertEqual(metadata.parent.name, "thus-spoke-zakura")
            metadata.mkdir(parents=True)
            (metadata / "instance.json").write_text("{}", encoding="utf-8")
            self.assertTrue(preview.remove_launcher_metadata(412))
            self.assertFalse(metadata.exists())
            self.assertFalse(preview.remove_launcher_metadata(412))


class VerifyTests(unittest.TestCase):
    def test_an_open_pull_request_at_the_approved_commit_passes(self):
        client = FakeClient(
            pull_requests={412: {"number": 412, "state": "open", "head": {"sha": "a" * 40}}}
        )
        self.assertEqual(preview.verify_approval(412, "a" * 40, client), "a" * 40)

    def test_a_closed_pull_request_is_refused(self):
        client = FakeClient(
            pull_requests={412: {"number": 412, "state": "closed", "head": {"sha": "a" * 40}}}
        )
        with self.assertRaises(preview.PreviewError):
            preview.verify_approval(412, "a" * 40, client)

    def test_a_pull_request_that_moved_on_is_refused(self):
        client = FakeClient(
            pull_requests={412: {"number": 412, "state": "open", "head": {"sha": "b" * 40}}}
        )
        with self.assertRaises(preview.PreviewError):
            preview.verify_approval(412, "a" * 40, client)

    def test_a_pull_request_without_a_state_is_refused(self):
        client = FakeClient(pull_requests={412: {"number": 412, "head": {"sha": "a" * 40}}})
        with self.assertRaises(preview.PreviewError):
            preview.verify_approval(412, "a" * 40, client)

    def test_verification_without_a_client_is_refused(self):
        with self.assertRaises(preview.PreviewError):
            preview.verify_approval(412, "a" * 40, None)


class RouteTests(unittest.TestCase):
    def test_route_template_carries_the_host_and_the_port(self):
        rendered = preview.render_route(
            "{host} {{\n    reverse_proxy 127.0.0.1:{port}\n}}\n".replace("{{", "{").replace("}}", "}"),
            host="pr-412.preview.example.com",
            port=33535,
            pr=412,
            instance="preview-pr-412",
        )
        self.assertIn("pr-412.preview.example.com {", rendered)
        self.assertIn("reverse_proxy 127.0.0.1:33535", rendered)

    def test_route_rejects_an_unknown_placeholder(self):
        with self.assertRaises(preview.PreviewError):
            preview.render_route(
                "{host} {unknown}",
                host="pr-412.preview.example.com",
                port=33535,
                pr=412,
                instance="preview-pr-412",
            )

    def test_default_route_template_is_a_proxy_route(self):
        rendered = preview.render_route(
            preview.DEFAULT_ROUTE_TEMPLATE,
            host="pr-412.preview.example.com",
            port=33535,
            pr=412,
            instance="preview-pr-412",
        )
        self.assertIn("127.0.0.1:33535", rendered)
        self.assertNotIn("{host}", rendered)
        self.assertNotIn("{port}", rendered)

    def test_route_file_is_written_and_removed(self):
        with tempfile.TemporaryDirectory() as directory:
            path = preview.write_route(directory, 412, "route\n")
            self.assertTrue(path.is_file())
            self.assertEqual(path.name, "ths-preview-pr-412.conf")
            self.assertTrue(preview.remove_route(directory, 412))
            self.assertFalse(path.exists())
            self.assertFalse(preview.remove_route(directory, 412))

    def test_route_path_is_scoped_to_the_pull_request(self):
        with tempfile.TemporaryDirectory() as directory:
            self.assertNotEqual(
                preview.proxy_route_path(directory, 412),
                preview.proxy_route_path(directory, 413),
            )

    def test_route_path_is_absent_without_an_ingress(self):
        self.assertIsNone(preview.proxy_route_path(None, 412))


class EnsurePreviewTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = self.directory.name

    def test_first_call_allocates_a_port_block(self):
        state = preview.ensure_preview(
            412, "https://pr-{pr}.preview.example.com", self.root, sha="a" * 40
        )
        self.assertEqual(state.instance, "preview-pr-412")
        self.assertEqual(state.port_offset % 10, 0)
        self.assertEqual(state.url, "https://pr-412.preview.example.com")
        self.assertEqual(state.sha, "a" * 40)

    def test_a_later_commit_keeps_the_port_block(self):
        first = preview.ensure_preview(412, "https://pr-{pr}.preview.example.com", self.root)
        second = preview.ensure_preview(
            412, "https://pr-{pr}.preview.example.com", self.root, sha="b" * 40
        )
        self.assertEqual(first.port_offset, second.port_offset)
        self.assertEqual(second.sha, "b" * 40)

    def test_the_state_is_recorded(self):
        preview.ensure_preview(412, "https://pr-{pr}.preview.example.com", self.root)
        stored = preview.read_state(self.root, 412)
        self.assertIsNotNone(stored)
        self.assertEqual(stored.pr, 412)

    def test_a_template_without_a_placeholder_is_refused(self):
        with self.assertRaises(preview.PreviewError):
            preview.ensure_preview(412, "https://preview.example.com", self.root)


class CommandTests(unittest.TestCase):
    """The command line the workflow calls."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = self.directory.name
        self.client = FakeClient()
        original = preview.GitHubClient
        preview.GitHubClient = lambda repository, token, **kwargs: self.client
        self.addCleanup(lambda: setattr(preview, "GitHubClient", original))
        os.environ["GITHUB_REPOSITORY"] = "owner/repo"
        os.environ["GITHUB_TOKEN"] = "token"

    def run_cli(self, *arguments: str) -> int:
        """Runs the command line the way the workflow runs it."""
        with contextlib.redirect_stdout(io.StringIO()):
            return preview.main(list(arguments))

    def test_plan_writes_the_workflow_outputs(self):
        with tempfile.TemporaryDirectory() as directory:
            payload = pathlib.Path(directory) / "event.json"
            payload.write_text(
                json.dumps(
                    {
                        "action": "closed",
                        "pull_request": {"number": 412, "head": {"sha": "a" * 40}},
                    }
                ),
                encoding="utf-8",
            )
            outputs = pathlib.Path(directory) / "output.txt"
            code = self.run_cli(
                *[
                    "plan",
                    "--event-name",
                    "pull_request",
                    "--event",
                    str(payload),
                    "--state-dir",
                    self.root,
                    "--github-output",
                    str(outputs),
                    "--no-api",
                ]
            )
            self.assertEqual(code, 0)
            written = outputs.read_text(encoding="utf-8")
            self.assertIn("action=teardown", written)
            self.assertIn("pr=412", written)

    def test_plan_reports_a_broken_payload(self):
        with tempfile.TemporaryDirectory() as directory:
            payload = pathlib.Path(directory) / "event.json"
            payload.write_text("{not json", encoding="utf-8")
            code = self.run_cli(
                *[
                    "plan",
                    "--event-name",
                    "pull_request",
                    "--event",
                    str(payload),
                    "--no-api",
                ]
            )
            self.assertEqual(code, 2)

    def test_configure_writes_the_state_and_the_outputs(self):
        with tempfile.TemporaryDirectory() as directory:
            outputs = pathlib.Path(directory) / "output.txt"
            code = self.run_cli(
                *[
                    "configure",
                    "--pr",
                    "412",
                    "--template",
                    "https://pr-{pr}.preview.example.com",
                    "--sha",
                    "a" * 40,
                    "--state-dir",
                    self.root,
                    "--github-output",
                    str(outputs),
                ]
            )
            self.assertEqual(code, 0)
            written = outputs.read_text(encoding="utf-8")
            self.assertIn("instance=preview-pr-412", written)
            self.assertIn("url=https://pr-412.preview.example.com", written)
            self.assertIn("dashboard_port=", written)

    def test_configure_refuses_a_template_without_a_placeholder(self):
        code = self.run_cli(
            *[
                "configure",
                "--pr",
                "412",
                "--template",
                "https://preview.example.com",
                "--state-dir",
                self.root,
            ]
        )
        self.assertEqual(code, 2)

    def test_report_writes_the_status_comment(self):
        code = self.run_cli(
            *["report", "--pr", "412", "--state", "deployed", "--url", "https://pr-412.example.com"]
        )
        self.assertEqual(code, 0)
        self.assertEqual(len(self.client.created), 1)
        body = self.client.created[0][1]
        self.assertIn("https://pr-412.example.com", body)
        self.assertIn(preview.STATUS_MARKER, body)

    def test_report_keeps_the_approval_of_the_same_commit(self):
        self.client.comment_lists[412] = [
            status_comment(412, {"sha": "a" * 40, "url": "https://pr-412.example.com"})
        ]
        self.run_cli(*["report", "--pr", "412", "--state", "deployed", "--sha", "a" * 40])
        body = self.client.updated[0][1]
        self.assertIn("@maintainer", body)

    def test_report_drops_the_approval_of_another_commit(self):
        self.client.comment_lists[412] = [
            status_comment(412, {"sha": "a" * 40, "url": "https://pr-412.example.com"})
        ]
        self.run_cli(*["report", "--pr", "412", "--state", "deployed", "--sha", "b" * 40])
        body = self.client.updated[0][1]
        self.assertNotIn("@maintainer", body)

    def test_report_records_a_new_approval(self):
        self.run_cli(
            *[
                "report",
                "--pr",
                "412",
                "--state",
                "approved",
                "--sha",
                "a" * 40,
                "--approved-by",
                "maintainer",
            ]
        )
        body = self.client.created[0][1]
        self.assertIn("@maintainer", body)
        blob = preview.read_status_blob({"body": body})
        self.assertEqual(blob["approved"]["sha"], "a" * 40)

    def test_report_drops_the_address_of_a_preview_that_is_gone(self):
        self.client.comment_lists[412] = [
            status_comment(412, {"sha": "a" * 40, "url": "https://pr-412.example.com"})
        ]
        self.run_cli(*["report", "--pr", "412", "--state", "removed", "--sha", "a" * 40])
        body = self.client.updated[0][1]
        self.assertNotIn("https://pr-412.example.com", body)

    def test_report_drops_the_address_while_waiting_for_an_approval(self):
        self.client.comment_lists[412] = [
            status_comment(412, {"sha": "a" * 40, "url": "https://pr-412.example.com"})
        ]
        self.run_cli(
            *["report", "--pr", "412", "--state", "await-approval", "--sha", "b" * 40]
        )
        body = self.client.updated[0][1]
        self.assertNotIn("https://pr-412.example.com", body)

    def test_teardown_clears_the_state(self):
        preview.write_state(
            self.root, preview.PreviewState(pr=412, instance=preview.instance_name(412))
        )
        original = preview._run_docker
        preview._run_docker = FakeDocker({})
        self.addCleanup(lambda: setattr(preview, "_run_docker", original))
        with contextlib.redirect_stdout(io.StringIO()):
            code = self.run_cli(*["teardown", "--pr", "412", "--state-dir", self.root, "--dry-run"])
        self.assertEqual(code, 0)
        self.assertIsNone(preview.read_state(self.root, 412))

    def test_state_reports_a_missing_preview(self):
        code = self.run_cli(*["state", "--pr", "412", "--state-dir", self.root])
        self.assertEqual(code, 1)

    def test_verify_passes_for_an_open_pull_request(self):
        self.client.pull_requests[412] = {
            "number": 412,
            "state": "open",
            "head": {"sha": "a" * 40},
        }
        code = self.run_cli(*["verify", "--pr", "412", "--sha", "a" * 40])
        self.assertEqual(code, 0)

    def test_verify_fails_for_a_closed_pull_request(self):
        self.client.pull_requests[412] = {
            "number": 412,
            "state": "closed",
            "head": {"sha": "a" * 40},
        }
        code = self.run_cli(*["verify", "--pr", "412", "--sha", "a" * 40])
        self.assertEqual(code, 2)

    def test_route_writes_and_removes_the_route(self):
        preview.ensure_preview(412, "https://pr-{pr}.preview.example.com", self.root)
        with tempfile.TemporaryDirectory() as directory:
            code = self.run_cli(
                *[
                    "route",
                    "--pr",
                    "412",
                    "--directory",
                    directory,
                    "--state-dir",
                    self.root,
                ]
            )
            self.assertEqual(code, 0)
            path = pathlib.Path(directory) / "ths-preview-pr-412.conf"
            self.assertTrue(path.is_file())
            self.assertIn("127.0.0.1:", path.read_text(encoding="utf-8"))
            code = self.run_cli(
                *[
                    "route",
                    "--pr",
                    "412",
                    "--directory",
                    directory,
                    "--state-dir",
                    self.root,
                    "--remove",
                ]
            )
            self.assertEqual(code, 0)
            self.assertFalse(path.exists())

    def test_route_without_a_state_fails(self):
        with tempfile.TemporaryDirectory() as directory:
            code = self.run_cli(
                *[
                    "route",
                    "--pr",
                    "412",
                    "--directory",
                    directory,
                    "--state-dir",
                    self.root,
                ]
            )
            self.assertEqual(code, 2)


class ScenarioTests(unittest.TestCase):
    """The whole flow, from an opened pull request to its removal."""

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = self.directory.name
        self.client = FakeClient(
            pull_requests={
                412: {
                    "number": 412,
                    "state": "open",
                    "head": {"sha": "a" * 40},
                }
            }
        )
        self.comments: list[dict] = []

    def pull_request_event(self, action: str, sha: str) -> dict:
        self.client.pull_requests[412]["head"]["sha"] = sha
        return {
            "action": action,
            "pull_request": {"number": 412, "head": {"sha": sha}},
        }

    def comment_event(self, body: str, association: str = "MEMBER") -> dict:
        comment = comment_body("maintainer", association, body)
        self.comments.append(comment)
        self.client.comment_lists[412] = list(self.comments)
        return {
            "action": "created",
            "issue": {"number": 412, "pull_request": {"url": "https://example.com"}},
            "comment": comment,
        }

    def plan(self, event_name: str, event: dict) -> preview.Plan:
        return preview.plan_for_event(event_name, event, self.client, self.root)

    def test_a_pull_request_waits_for_approval_and_then_deploys(self):
        opened = self.plan("pull_request", self.pull_request_event("opened", "a" * 40))
        self.assertEqual(opened.action, "await-approval")

        # A contributor cannot approve the pull request.
        stranger = self.plan(
            "issue_comment", self.comment_event("/preview approve", "CONTRIBUTOR")
        )
        self.assertEqual(stranger.action, "ignore")

        # A maintainer can.
        approved = self.plan("issue_comment", self.comment_event("/preview approve"))
        self.assertEqual(approved.action, "deploy")
        self.assertEqual(approved.sha, "a" * 40)

        # The deployment records the approval and the address.
        preview.upsert_status_comment(
            self.client,
            412,
            preview.status_comment_body(
                approved, url="https://pr-412.preview.example.com", state="deployed"
            ),
        )
        self.assertEqual(len(self.client.created), 1)
        blob = preview.read_status_blob({"body": self.client.created[0][1]})
        self.assertEqual(blob["state"], "deployed")
        self.assertEqual(blob["url"], "https://pr-412.preview.example.com")
        self.assertEqual(blob["approved"]["sha"], "a" * 40)

        # The same commit is still approved, so a rerun deploys it again.
        again = self.plan("pull_request", self.pull_request_event("synchronize", "a" * 40))
        self.assertEqual(again.action, "deploy")

        # A new commit needs a new approval.
        moved = self.plan("pull_request", self.pull_request_event("synchronize", "b" * 40))
        self.assertEqual(moved.action, "await-approval")

        # The maintainer approves the new commit.
        reapproved = self.plan("issue_comment", self.comment_event("/preview approve"))
        self.assertEqual(reapproved.action, "deploy")
        self.assertEqual(reapproved.sha, "b" * 40)

        # Closing the pull request removes the preview.
        self.client.pull_requests[412]["state"] = "closed"
        closed = self.plan("pull_request", self.pull_request_event("closed", "b" * 40))
        self.assertEqual(closed.action, "teardown")
        self.assertEqual(closed.instance, "preview-pr-412")

    def test_a_denial_withdraws_the_approval(self):
        self.plan("issue_comment", self.comment_event("/preview approve"))
        denied = self.plan("issue_comment", self.comment_event("/preview deny"))
        self.assertEqual(denied.action, "deny")

    def test_a_teardown_command_removes_the_preview(self):
        teardown = self.plan("issue_comment", self.comment_event("/preview teardown"))
        self.assertEqual(teardown.action, "teardown")
        self.assertEqual(teardown.instance, "preview-pr-412")

    def test_a_comment_on_an_issue_does_nothing(self):
        event = self.comment_event("/preview approve")
        event["issue"].pop("pull_request")
        self.assertEqual(self.plan("issue_comment", event).action, "ignore")

    def test_the_preview_keeps_its_port_block_between_commits(self):
        first = preview.ensure_preview(
            412, "https://pr-{pr}.preview.example.com", self.root, sha="a" * 40
        )
        second = preview.ensure_preview(
            412, "https://pr-{pr}.preview.example.com", self.root, sha="b" * 40
        )
        self.assertEqual(first.port_offset, second.port_offset)
        self.assertEqual(first.url, second.url)


# --------------------------------------------------------------------------
# The workflow
# --------------------------------------------------------------------------


class WorkflowContractTests(unittest.TestCase):
    """The workflow is the security boundary, so its shape is checked here."""

    @classmethod
    def setUpClass(cls):
        cls.text = WORKFLOW.read_text(encoding="utf-8")

    def test_the_workflow_exists(self):
        self.assertTrue(WORKFLOW.is_file())

    def test_the_workflow_uses_the_trusted_pull_request_event(self):
        triggers = section(self.text, "on:", 0)
        self.assertIn("pull_request_target:", triggers)

    def test_the_workflow_never_runs_untrusted_code_in_a_secret_bearing_run(self):
        triggers = section(self.text, "on:", 0)
        self.assertNotIn("pull_request:", triggers)

    def test_the_workflow_reacts_to_comments(self):
        triggers = section(self.text, "on:", 0)
        self.assertIn("issue_comment:", triggers)

    def test_the_workflow_can_be_run_by_hand(self):
        triggers = section(self.text, "on:", 0)
        self.assertIn("workflow_dispatch:", triggers)

    def test_the_workflow_tears_down_on_close(self):
        triggers = section(self.text, "on:", 0)
        self.assertIn("closed", triggers)

    def test_the_top_level_permissions_are_read_only(self):
        permissions = section(self.text, "permissions:", 0)
        self.assertIn("contents: read", permissions)
        for scope in ("write", "id-token", "packages"):
            self.assertNotIn(scope, permissions)

    def test_the_build_job_may_not_see_a_secret(self):
        build = section(self.text, "build:", 2)
        self.assertNotIn("secrets.", build)
        self.assertNotIn("environment:", build)

    def test_the_build_job_may_only_read_the_repository(self):
        build = section(self.text, "build:", 2)
        permissions = section(build, "permissions:", 4)
        self.assertIn("contents: read", permissions)
        self.assertNotIn("write", permissions)

    def test_the_deploy_job_uses_the_preview_environment(self):
        deploy = section(self.text, "deploy:", 2)
        self.assertIn("environment:", deploy)
        self.assertIn("preview", deploy)

    def test_the_teardown_job_uses_the_preview_environment(self):
        teardown = section(self.text, "teardown:", 2)
        self.assertIn("environment:", teardown)

    def test_the_workflow_only_publishes_loopback_addresses(self):
        self.assertNotIn("0.0.0.0:", self.text)

    def test_the_workflow_uses_the_pinned_actions(self):
        for line in self.text.splitlines():
            if "uses:" in line and "@" in line:
                self.assertRegex(line, r"uses: [\w.-]+/[^\s]+@[0-9a-f]{40}(\s|$)")


if __name__ == "__main__":
    unittest.main(verbosity=2)
