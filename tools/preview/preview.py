#!/usr/bin/env python3
"""Decides what a pull request preview deployment does.

`.github/workflows/preview.yml` keeps its decisions in this module so the rules
can be exercised without a runner: which pull request a webhook payload refers
to, whether a maintainer approved it, which Docker resources belong to it, and
what the status comment says.

The module never runs pull request code. It only reads webhook payloads, calls
the GitHub API, and computes names, ports, and comments.

    python3 tools/preview/preview.py plan --event-name pull_request --event payload.json
    python3 tools/preview/preview.py teardown --pr 412 --dry-run
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import shutil
import socket
import subprocess
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any, Callable, Iterable, Sequence

# The launcher names every resource of an instance `ths-<instance>` and labels
# it with the instance name. A preview instance is `preview-pr-<number>`, so
# its resources are `ths-preview-pr-<number>-*` and never collide with a
# developer's own environment.
INSTANCE_PREFIX = "preview-pr-"
RESOURCE_PREFIX = "ths-"
INSTANCE_LABEL = "com.zakura.ths.instance"

# The launcher's default loopback ports. A preview adds `--port-offset` to all
# four so that several previews can run on one host at the same time.
DASHBOARD_BASE_PORT = 32805
RPC_BASE_PORT = 18232
P2P_BASE_PORT = 18233
LIGHTWALLETD_BASE_PORT = 9067
MAX_PORT = 65535
PORT_STRIDE = 10

# The dashboard carries the largest base port, so it limits the offset.
MAX_PORT_OFFSET = ((MAX_PORT - DASHBOARD_BASE_PORT) // PORT_STRIDE) * PORT_STRIDE
OFFSET_SLOTS = MAX_PORT_OFFSET // PORT_STRIDE

# Containers, volumes, and the network the launcher creates for an instance.
CONTAINER_SERVICES = ("app", "zakura", "lightwalletd", "init")
VOLUME_SUFFIXES = ("chain", "wallet", "lightwalletd", "config")

# The status comment is written by the workflow's own token. Only a comment
# from that account is read back, so a pull request author cannot forge one.
STATUS_MARKER = "<!-- ths-preview"
STATUS_ACCOUNT = "github-actions[bot]"

# `author_association` values that mean the comment author can already change
# the repository. A contributor without write access is never trusted.
MAINTAINER_ASSOCIATIONS = frozenset({"owner", "member", "collaborator"})

COMMAND_PATTERN = re.compile(
    r"^\s*/preview\s+(?P<command>[a-z]+)(?P<arguments>.*)$", re.IGNORECASE | re.DOTALL
)
COMMANDS = ("approve", "deny", "teardown", "status")

# The states that keep a recorded approval. A withdrawn, denied, or removed
# preview has no approval, so a later commit starts over.
KEEPS_APPROVAL = frozenset({"approved", "deployed", "failed", "status"})

# Actions of a pull request payload that ask for a preview of the current head.
PREVIEW_ACTIONS = ("opened", "synchronize", "reopened", "ready_for_review")

# The workflow listens to `pull_request_target`, so that it runs in the
# context of the base branch and keeps its secrets away from pull request
# code. The payload of both events is the same.
PULL_REQUEST_EVENTS = ("pull_request", "pull_request_target")

DEFAULT_ROUTE_TEMPLATE = "{host} {\n    reverse_proxy 127.0.0.1:{port}\n}\n"
STATE_DIRECTORY_VARIABLE = "THS_PREVIEW_STATE_DIR"
DEFAULT_STATE_DIRECTORY = pathlib.Path.home() / ".local" / "state" / "ths-preview"


class PreviewError(Exception):
    """A preview request cannot be served as asked."""


@dataclass(frozen=True)
class Plan:
    """What the workflow should do for one webhook payload."""

    action: str
    pr: int | None = None
    sha: str = ""
    reason: str = ""
    approved_by: str = ""
    instance: str = ""
    url: str = ""
    bot_account: str = STATUS_ACCOUNT

    @property
    def deploys(self) -> bool:
        return self.action == "deploy"

    def outputs(self) -> dict[str, str]:
        """The values the workflow writes to `$GITHUB_OUTPUT`."""
        return {
            "action": self.action,
            "pr": "" if self.pr is None else str(self.pr),
            "sha": self.sha,
            "reason": self.reason,
            "approved_by": self.approved_by,
            "instance": self.instance,
            "url": self.url,
        }


def instance_name(pr: int) -> str:
    """The launcher instance name for a pull request.

    The launcher accepts 1 to 40 lowercase letters, digits, or internal
    hyphens, so the name is checked here instead of failing inside the
    launcher minutes later.
    """
    number = _pull_request_number(pr)
    name = f"{INSTANCE_PREFIX}{number}"
    if len(name) > 40:
        raise PreviewError(f"instance name {name!r} is longer than 40 characters")
    return name


def resource_prefix(pr: int) -> str:
    """The prefix every Docker resource of a preview carries."""
    return f"{RESOURCE_PREFIX}{instance_name(pr)}"


def resource_names(pr: int) -> dict[str, list[str]]:
    """The Docker resources the launcher creates for a preview instance."""
    prefix = resource_prefix(pr)
    return {
        "containers": [f"{prefix}-{service}" for service in CONTAINER_SERVICES],
        "volumes": [f"{prefix}-{suffix}" for suffix in VOLUME_SUFFIXES],
        "network": prefix,
    }


def base_ports() -> dict[str, int]:
    """The launcher's default loopback ports."""
    return {
        "dashboard": DASHBOARD_BASE_PORT,
        "rpc": RPC_BASE_PORT,
        "p2p": P2P_BASE_PORT,
        "lightwalletd": LIGHTWALLETD_BASE_PORT,
    }


def instance_ports(offset: int) -> dict[str, int]:
    """The ports a preview instance publishes for one offset."""
    if offset < 0 or offset % PORT_STRIDE:
        raise PreviewError(f"port offset {offset} is not a multiple of {PORT_STRIDE}")
    ports = {}
    for name, base in base_ports().items():
        port = base + offset
        if port > MAX_PORT:
            raise PreviewError(f"port offset {offset} overflows the {name} port")
        ports[name] = port
    return ports


def candidate_offsets(pr: int) -> list[int]:
    """Every usable offset, in the order a preview should try them.

    The order starts at a position derived from the pull request number so
    that two previews rarely want the same port, and then walks every slot, so
    a busy host still finds a free one.
    """
    number = _pull_request_number(pr)
    start = (number * 2654435761) % OFFSET_SLOTS
    return [
        PORT_STRIDE * ((start + step) % OFFSET_SLOTS + 1) for step in range(OFFSET_SLOTS)
    ]


def port_is_free(port: int, host: str = "127.0.0.1") -> bool:
    """Whether a loopback port can still be bound."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            probe.bind((host, port))
        except OSError:
            return False
    return True


def allocate_offset(
    pr: int,
    is_free: Callable[[int], bool] = port_is_free,
) -> int:
    """The first offset whose four ports are free on this host.

    The offset is recorded with the preview, so later commits reuse it and the
    preview keeps its address.
    """
    for offset in candidate_offsets(pr):
        ports = instance_ports(offset)
        if all(is_free(port) for port in ports.values()):
            return offset
    raise PreviewError("no free port block is left for this preview")


def preview_url(template: str, pr: int) -> str:
    """The address reviewers open, built from the configured template."""
    number = _pull_request_number(pr)
    if "{pr}" not in template:
        raise PreviewError(f"preview URL template {template!r} has no {{pr}} placeholder")
    url = template.replace("{pr}", str(number))
    if not url.startswith(("http://", "https://")):
        raise PreviewError(f"preview URL {url!r} is not an http or https address")
    return url


def _pull_request_number(pr: Any) -> int:
    if isinstance(pr, bool) or not isinstance(pr, int):
        raise PreviewError(f"pull request number {pr!r} is not a number")
    if pr < 1:
        raise PreviewError(f"pull request number {pr} is not positive")
    return pr


# --------------------------------------------------------------------------
# State
# --------------------------------------------------------------------------


@dataclass
class PreviewState:
    """What the host remembers about one preview."""

    pr: int
    instance: str
    sha: str = ""
    port_offset: int = 0
    url: str = ""
    updated_at: str = ""

    def to_json(self) -> str:
        return json.dumps(
            {
                "pr": self.pr,
                "instance": self.instance,
                "sha": self.sha,
                "port_offset": self.port_offset,
                "url": self.url,
                "updated_at": self.updated_at,
            },
            indent=2,
            sort_keys=True,
        )

    @classmethod
    def from_json(cls, raw: str) -> "PreviewState":
        data = json.loads(raw)
        return cls(
            pr=int(data["pr"]),
            instance=str(data["instance"]),
            sha=str(data.get("sha", "")),
            port_offset=int(data.get("port_offset", 0)),
            url=str(data.get("url", "")),
            updated_at=str(data.get("updated_at", "")),
        )


def state_directory(root: str | os.PathLike[str] | None = None) -> pathlib.Path:
    """The directory that holds one state file per preview."""
    if root is not None:
        return pathlib.Path(root)
    configured = os.environ.get(STATE_DIRECTORY_VARIABLE)
    if configured:
        return pathlib.Path(configured)
    return DEFAULT_STATE_DIRECTORY


def state_path(root: str | os.PathLike[str] | None, pr: int) -> pathlib.Path:
    """The state file of one preview."""
    number = _pull_request_number(pr)
    return state_directory(root) / f"pr-{number}.json"


def read_state(root: str | os.PathLike[str] | None, pr: int) -> PreviewState | None:
    """The recorded state of a preview, or `None` when it has none."""
    path = state_path(root, pr)
    try:
        raw = path.read_text(encoding="utf-8")
    except FileNotFoundError:
        return None
    except OSError as error:
        raise PreviewError(f"reading {path}: {error}") from error
    try:
        return PreviewState.from_json(raw)
    except (json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
        raise PreviewError(f"{path} is not a valid preview state: {error}") from error


def write_state(root: str | os.PathLike[str] | None, state: PreviewState) -> pathlib.Path:
    """Records the state of a preview."""
    path = state_path(root, state.pr)
    path.parent.mkdir(parents=True, exist_ok=True)
    state.updated_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
    temporary = path.with_suffix(".json.pending")
    temporary.write_text(state.to_json(), encoding="utf-8")
    os.replace(temporary, path)
    return path


def clear_state(root: str | os.PathLike[str] | None, pr: int) -> bool:
    """Forgets a preview. Returns whether there was anything to forget."""
    path = state_path(root, pr)
    try:
        path.unlink()
    except FileNotFoundError:
        return False
    except OSError as error:
        raise PreviewError(f"removing {path}: {error}") from error
    return True


# --------------------------------------------------------------------------
# Commands and approvals
# --------------------------------------------------------------------------


def parse_command(body: str) -> tuple[str, str] | None:
    """The preview command in a comment, if it carries one.

    A command has to start the comment, so quoted text that merely mentions
    `/preview approve` never counts.
    """
    if not isinstance(body, str):
        return None
    match = COMMAND_PATTERN.match(body)
    if match is None:
        return None
    command = match.group("command").lower()
    if command not in COMMANDS:
        return None
    return command, match.group("arguments").strip()


def is_maintainer(association: str) -> bool:
    """Whether a comment author may approve a preview."""
    return str(association).lower() in MAINTAINER_ASSOCIATIONS


@dataclass(frozen=True)
class Approval:
    """A maintainer's approval of one commit."""

    sha: str
    by: str
    at: str = ""


def find_status_comment(
    comments: Iterable[dict[str, Any]],
    account: str = STATUS_ACCOUNT,
) -> dict[str, Any] | None:
    """The workflow's own status comment, if it posted one."""
    for comment in comments:
        if comment.get("user", {}).get("login") != account:
            continue
        if STATUS_MARKER in str(comment.get("body", "")):
            return comment
    return None


def read_status_blob(comment: dict[str, Any] | None) -> dict[str, Any]:
    """The machine-readable state inside a status comment."""
    if not comment:
        return {}
    body = str(comment.get("body", ""))
    start = body.find(STATUS_MARKER)
    if start < 0:
        return {}
    start += len(STATUS_MARKER)
    end = body.find("-->", start)
    if end < 0:
        return {}
    try:
        blob = json.loads(body[start:end].strip())
    except json.JSONDecodeError:
        return {}
    return blob if isinstance(blob, dict) else {}


def approval_from_blob(blob: dict[str, Any]) -> Approval | None:
    """The approval recorded in a status comment."""
    approved = blob.get("approved")
    if not isinstance(approved, dict):
        return None
    sha = str(approved.get("sha", ""))
    if not sha:
        return None
    return Approval(sha=sha, by=str(approved.get("by", "")), at=str(approved.get("at", "")))


# --------------------------------------------------------------------------
# GitHub
# --------------------------------------------------------------------------


class GitHubClient:
    """The small part of the GitHub API the preview workflow needs."""

    def __init__(
        self,
        repository: str,
        token: str,
        base_url: str = "https://api.github.com",
        transport: Callable[[str, str, Any], Any] | None = None,
    ) -> None:
        self.repository = repository
        self.token = token
        self.base_url = base_url.rstrip("/")
        self._transport = transport

    def _call(self, method: str, path: str, payload: Any = None) -> Any:
        if self._transport is not None:
            return self._transport(method, path, payload)
        url = f"{self.base_url}{path}"
        data = None
        headers = {
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "thus-spoke-zakura-preview",
        }
        if payload is not None:
            data = json.dumps(payload).encode("utf-8")
            headers["Content-Type"] = "application/json"
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        request = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(request) as response:
                body = response.read().decode("utf-8")
        except urllib.error.HTTPError as error:
            detail = error.read().decode("utf-8", "replace")
            raise PreviewError(f"{method} {path} failed with {error.code}: {detail}") from error
        except urllib.error.URLError as error:
            raise PreviewError(f"{method} {path} failed: {error.reason}") from error
        if not body.strip():
            return None
        return json.loads(body)

    def pull_request(self, number: int) -> dict[str, Any]:
        """One pull request, including its current head commit."""
        data = self._call("GET", f"/repos/{self.repository}/pulls/{number}")
        if not isinstance(data, dict):
            raise PreviewError(f"pull request {number} was not found")
        return data

    def comments(self, number: int) -> list[dict[str, Any]]:
        """Every comment on a pull request."""
        data = self._call("GET", f"/repos/{self.repository}/issues/{number}/comments?per_page=100")
        if not isinstance(data, list):
            return []
        return [comment for comment in data if isinstance(comment, dict)]

    def create_comment(self, number: int, body: str) -> dict[str, Any]:
        """Posts the status comment."""
        data = self._call(
            "POST",
            f"/repos/{self.repository}/issues/{number}/comments",
            {"body": body},
        )
        return data if isinstance(data, dict) else {}

    def update_comment(self, comment_id: int, body: str) -> dict[str, Any]:
        """Replaces the body of the status comment."""
        data = self._call(
            "PATCH",
            f"/repos/{self.repository}/issues/comments/{comment_id}",
            {"body": body},
        )
        return data if isinstance(data, dict) else {}


# --------------------------------------------------------------------------
# Plan
# --------------------------------------------------------------------------


def plan_for_event(
    event_name: str,
    event: dict[str, Any],
    client: GitHubClient | None = None,
    state_root: str | os.PathLike[str] | None = None,
) -> Plan:
    """The action a webhook payload asks for.

    Every branch answers with a plan, never with an exception for ordinary
    payloads: a comment on an issue, a pull request from a fork, and a closed
    pull request all have a defined answer.
    """
    if event_name in PULL_REQUEST_EVENTS:
        return _plan_for_pull_request(event, client, state_root)
    if event_name == "issue_comment":
        return _plan_for_issue_comment(event, client, state_root)
    if event_name == "workflow_dispatch":
        return _plan_for_dispatch(event, client, state_root)
    return Plan(action="ignore", reason=f"no preview is defined for {event_name}")


def _plan_for_pull_request(
    event: dict[str, Any],
    client: GitHubClient | None,
    state_root: str | os.PathLike[str] | None,
) -> Plan:
    pull_request = event.get("pull_request") or {}
    number = pull_request.get("number")
    action = str(event.get("action", ""))
    if not isinstance(number, int):
        return Plan(action="ignore", reason="the payload carries no pull request number")
    if action == "closed":
        return Plan(
            action="teardown",
            pr=number,
            sha=str(pull_request.get("head", {}).get("sha", "")),
            instance=instance_name(number),
            reason="the pull request was closed",
        )
    if action not in PREVIEW_ACTIONS:
        return Plan(
            action="ignore",
            pr=number,
            reason=f"a {action} pull request event does not change the preview",
        )
    sha = str(pull_request.get("head", {}).get("sha", ""))
    approval = _recorded_approval(number, sha, client, state_root)
    if approval is None:
        return Plan(
            action="await-approval",
            pr=number,
            sha=sha,
            instance=instance_name(number),
            reason="no maintainer has approved this commit yet",
        )
    return Plan(
        action="deploy",
        pr=number,
        sha=sha,
        instance=instance_name(number),
        approved_by=approval.by,
        reason=f"approved by {approval.by}",
    )


def _plan_for_issue_comment(
    event: dict[str, Any],
    client: GitHubClient | None,
    state_root: str | os.PathLike[str] | None,
) -> Plan:
    issue = event.get("issue") or {}
    comment = event.get("comment") or {}
    if not issue.get("pull_request"):
        return Plan(action="ignore", reason="the comment is not on a pull request")
    number = issue.get("number")
    if not isinstance(number, int):
        return Plan(action="ignore", reason="the payload carries no pull request number")
    parsed = parse_command(str(comment.get("body", "")))
    if parsed is None:
        return Plan(action="ignore", pr=number, reason="the comment is not a preview command")
    command, arguments = parsed
    author = str(comment.get("user", {}).get("login", ""))
    association = str(comment.get("author_association", ""))
    if not is_maintainer(association):
        return Plan(
            action="ignore",
            pr=number,
            reason=f"{author or 'the author'} is not a maintainer, so the command is ignored",
        )
    if command == "teardown":
        return Plan(
            action="teardown",
            pr=number,
            instance=instance_name(number),
            reason=f"teardown requested by {author}",
        )
    if command == "deny":
        return Plan(
            action="deny",
            pr=number,
            instance=instance_name(number),
            reason=f"denied by {author}",
        )
    if command == "status":
        return Plan(
            action="status",
            pr=number,
            instance=instance_name(number),
            reason=f"status requested by {author}",
        )
    if command == "approve":
        sha = _current_head_sha(number, client)
        if not sha:
            return Plan(
                action="await-approval",
                pr=number,
                instance=instance_name(number),
                reason="the pull request head could not be read",
            )
        return Plan(
            action="deploy",
            pr=number,
            sha=sha,
            instance=instance_name(number),
            approved_by=author,
            reason=f"approved by {author}",
        )
    return Plan(action="ignore", pr=number, reason=f"unknown command {command!r}")


def _plan_for_dispatch(
    event: dict[str, Any],
    client: GitHubClient | None,
    state_root: str | os.PathLike[str] | None,
) -> Plan:
    inputs = event.get("inputs") or {}
    number = inputs.get("pr")
    try:
        pr = int(str(number).strip())
    except (TypeError, ValueError):
        return Plan(action="ignore", reason=f"{number!r} is not a pull request number")
    action = str(inputs.get("action", "deploy")).strip().lower()
    if action == "teardown":
        return Plan(
            action="teardown",
            pr=pr,
            instance=instance_name(pr),
            reason="teardown requested from the workflow inputs",
        )
    if action == "status":
        return Plan(
            action="status",
            pr=pr,
            instance=instance_name(pr),
            reason="status requested from the workflow inputs",
        )
    if action != "deploy":
        return Plan(action="ignore", pr=pr, reason=f"unknown action {action!r}")
    sha = _current_head_sha(pr, client)
    if not sha:
        return Plan(
            action="await-approval",
            pr=pr,
            instance=instance_name(pr),
            reason="the pull request head could not be read",
        )
    approval = _recorded_approval(pr, sha, client, state_root)
    if approval is None:
        return Plan(
            action="await-approval",
            pr=pr,
            sha=sha,
            instance=instance_name(pr),
            reason="no maintainer has approved this commit yet",
        )
    return Plan(
        action="deploy",
        pr=pr,
        sha=sha,
        instance=instance_name(pr),
        approved_by=approval.by,
        reason=f"approved by {approval.by}",
    )


def _current_head_sha(number: int, client: GitHubClient | None) -> str:
    if client is None:
        return ""
    try:
        pull_request = client.pull_request(number)
    except PreviewError:
        return ""
    return str(pull_request.get("head", {}).get("sha", ""))


def _recorded_approval(
    number: int,
    sha: str,
    client: GitHubClient | None,
    state_root: str | os.PathLike[str] | None,
) -> Approval | None:
    """The approval for one commit, from the status comment or the state file.

    The status comment is the record a reviewer can see. The state file is the
    fallback for a host that lost the comment, and it is only trusted when it
    names the same commit.
    """
    if client is not None:
        try:
            comment = find_status_comment(client.comments(number))
        except PreviewError:
            comment = None
        approval = approval_from_blob(read_status_blob(comment))
        if approval is not None and approval.sha == sha:
            return approval
    try:
        state = read_state(state_root, number)
    except PreviewError:
        state = None
    if state is not None and state.sha and state.sha == sha:
        return Approval(sha=state.sha, by="the preview host", at=state.updated_at)
    return None


# --------------------------------------------------------------------------
# Status comment
# --------------------------------------------------------------------------


def status_comment_body(
    plan: Plan,
    url: str = "",
    error: str = "",
    state: str = "",
    approval: Approval | None = None,
) -> str:
    """The status comment reviewers read.

    The first line is a machine-readable record. Only the workflow's own
    account is read back, so nothing a pull request author writes can change
    the recorded state.
    """
    resolved = url or plan.url
    approval = approval or (
        Approval(sha=plan.sha, by=plan.approved_by) if plan.approved_by else None
    )
    blob = {
        "pr": plan.pr,
        "sha": plan.sha,
        "state": state or plan.action,
        "url": resolved,
        "approved": None
        if approval is None
        else {"sha": approval.sha, "by": approval.by, "at": approval.at},
    }
    marker = f"{STATUS_MARKER} {json.dumps(blob, sort_keys=True)} -->"
    rows = [("State", state or plan.action)]
    if plan.sha:
        rows.append(("Commit", f"`{plan.sha[:12]}`"))
    if resolved:
        rows.append(("Preview", f"[{resolved}]({resolved})"))
    if approval is not None and approval.by:
        rows.append(("Approved by", f"@{approval.by}"))
    if error:
        rows.append(("Failure", error))
    table = "\n".join(f"| {name} | {value} |" for name, value in rows)
    body = f"{marker}\n## Pull request preview\n\n| | |\n| --- | --- |\n{table}\n"
    if error:
        body += f"\nThe preview could not be deployed.\n\n```\n{error}\n```\n"
    elif state == "await-approval":
        body += (
            "\nA maintainer has to comment `/preview approve` before this commit is "
            "deployed. A later commit needs a later approval.\n"
        )
    elif state == "denied":
        body += "\nThe approval was withdrawn, so nothing is deployed.\n"
    else:
        body += (
            "\nThis preview is a throwaway Regtest environment. It is isolated from "
            "production and from other previews, and it is removed when the pull "
            "request closes.\n"
        )
    return body


def upsert_status_comment(
    client: GitHubClient,
    number: int,
    body: str,
    account: str = STATUS_ACCOUNT,
) -> dict[str, Any]:
    """Writes the status comment, replacing the previous one."""
    existing = find_status_comment(client.comments(number), account)
    if existing is None:
        return client.create_comment(number, body)
    comment_id = existing.get("id")
    if not isinstance(comment_id, int):
        return client.create_comment(number, body)
    return client.update_comment(comment_id, body)


# --------------------------------------------------------------------------
# Teardown
# --------------------------------------------------------------------------


@dataclass
class Removal:
    """One resource the teardown may remove."""

    kind: str
    name: str
    removed: bool = False
    failed: bool = False
    reason: str = ""


def _run_docker(arguments: Sequence[str]) -> str:
    try:
        completed = subprocess.run(
            list(arguments),
            check=False,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        raise PreviewError(f"running {arguments[0]} failed: {error}") from error
    if completed.returncode != 0:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise PreviewError(f"{' '.join(arguments)} failed: {detail}")
    return completed.stdout


def docker_label_of(
    kind: str,
    name: str,
    runner: Callable[[Sequence[str]], str] | None = None,
) -> str | None:
    """The instance label on a Docker resource.

    `None` means the resource is not on this host. A Docker failure that is
    not a missing object is raised, so a broken daemon never looks like an
    empty host.
    """
    runner = runner or _run_docker
    template = (
        '{{index .Config.Labels "' + INSTANCE_LABEL + '"}}'
        if kind == "container"
        else '{{index .Labels "' + INSTANCE_LABEL + '"}}'
    )
    try:
        output = runner(["docker", "inspect", "--format", template, name])
    except PreviewError as error:
        if "no such" in str(error).lower():
            return None
        raise
    label = output.strip()
    return label or None


def launcher_metadata_directory(pr: int) -> pathlib.Path:
    """Where the launcher keeps the metadata of one instance.

    The launcher removes this directory itself when it stops an instance.
    The fallback cleanup removes it too, so a preview leaves nothing behind
    on the host.
    """
    base = os.environ.get("XDG_CONFIG_HOME")
    root = pathlib.Path(base) if base else pathlib.Path.home() / ".config"
    return root / "thus-spoke-zakura" / instance_name(pr)


def remove_launcher_metadata(pr: int) -> bool:
    """Removes the metadata of one preview instance."""
    directory = launcher_metadata_directory(pr)
    if not directory.is_dir():
        return False
    shutil.rmtree(directory, ignore_errors=False)
    return True


def teardown_resources(
    pr: int,
    runner: Callable[[Sequence[str]], str] | None = None,
    apply: bool = False,
) -> list[Removal]:
    """The preview resources of one pull request, and what happened to them.

    A resource is only removed when it still carries the label of this
    preview. A resource of another instance keeps its label, so it is never
    touched.
    """
    runner = runner or _run_docker
    instance = instance_name(pr)
    names = resource_names(pr)
    removals: list[Removal] = []
    for kind, targets in (
        ("container", names["containers"]),
        ("volume", names["volumes"]),
        ("network", [names["network"]]),
    ):
        for target in targets:
            try:
                label = docker_label_of(kind, target, runner)
            except PreviewError as error:
                removals.append(
                    Removal(kind=kind, name=target, failed=True, reason=str(error))
                )
                continue
            if label is None:
                removals.append(
                    Removal(kind=kind, name=target, reason="it is not present")
                )
                continue
            if label != instance:
                removals.append(
                    Removal(
                        kind=kind,
                        name=target,
                        reason=f"it belongs to {label!r}, so it was kept",
                    )
                )
                continue
            if not apply:
                removals.append(
                    Removal(kind=kind, name=target, reason="it would be removed")
                )
                continue
            command = {
                "container": ["docker", "rm", "-f", target],
                "volume": ["docker", "volume", "rm", target],
                "network": ["docker", "network", "rm", target],
            }[kind]
            try:
                runner(command)
            except PreviewError as error:
                removals.append(
                    Removal(kind=kind, name=target, failed=True, reason=str(error))
                )
                continue
            removals.append(
                Removal(kind=kind, name=target, removed=True, reason="it was removed")
            )
    return removals


def proxy_route_path(directory: str | os.PathLike[str] | None, pr: int) -> pathlib.Path | None:
    """The ingress route file of one preview, when the ingress is configured."""
    if not directory:
        return None
    return pathlib.Path(directory) / f"ths-{instance_name(pr)}.conf"


def ensure_preview(
    pr: int,
    template: str,
    state_root: str | os.PathLike[str] | None = None,
    sha: str = "",
    is_free: Callable[[int], bool] = port_is_free,
) -> PreviewState:
    """The state of a preview, allocating its port block the first time.

    The port block is recorded, so a later commit reuses it and the preview
    keeps the address reviewers already have.
    """
    number = _pull_request_number(pr)
    state = read_state(state_root, number)
    if state is None:
        state = PreviewState(
            pr=number,
            instance=instance_name(number),
            port_offset=allocate_offset(number, is_free),
        )
    if sha:
        state.sha = sha
    state.url = preview_url(template, number)
    write_state(state_root, state)
    return state


def render_route(template: str, *, host: str, port: int, pr: int, instance: str) -> str:
    """The ingress route of one preview.

    The template carries `{host}`, `{port}`, `{pr}`, and `{instance}`. An
    unknown placeholder is an error, so a mistyped template fails loudly
    instead of writing a broken route.
    """
    values = {"host": host, "port": str(port), "pr": str(pr), "instance": instance}
    rendered = template
    for name, value in values.items():
        rendered = rendered.replace("{" + name + "}", value)
    leftover = re.search(r"\{(\w+)\}", rendered)
    if leftover is not None:
        raise PreviewError(f"the route template has an unknown placeholder {leftover.group(0)}")
    return rendered


def write_route(directory: str | os.PathLike[str], pr: int, content: str) -> pathlib.Path:
    """Writes the ingress route of one preview."""
    path = proxy_route_path(directory, pr)
    if path is None:
        raise PreviewError("no ingress directory is configured")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    return path


def remove_route(directory: str | os.PathLike[str] | None, pr: int) -> bool:
    """Removes the ingress route of one preview."""
    path = proxy_route_path(directory, pr)
    if path is None:
        return False
    try:
        path.unlink()
    except FileNotFoundError:
        return False
    except OSError as error:
        raise PreviewError(f"removing {path}: {error}") from error
    return True


# --------------------------------------------------------------------------
# Command line
# --------------------------------------------------------------------------


def _read_event(path: str) -> dict[str, Any]:
    if path == "-":
        raw = sys.stdin.read()
    else:
        # `utf-8-sig` also reads a payload that was written with a byte order
        # mark, which some editors add.
        raw = pathlib.Path(path).read_text(encoding="utf-8-sig")
    try:
        event = json.loads(raw)
    except json.JSONDecodeError as error:
        raise PreviewError(f"{path} is not valid JSON: {error}") from error
    if not isinstance(event, dict):
        raise PreviewError(f"{path} is not a webhook payload")
    return event


def _client() -> GitHubClient:
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    token = os.environ.get("GITHUB_TOKEN", "")
    if not repository:
        raise PreviewError("GITHUB_REPOSITORY is not set")
    return GitHubClient(repository, token)


def _write_outputs(plan: Plan, destination: str | None) -> None:
    if not destination:
        return
    with open(destination, "a", encoding="utf-8") as handle:
        for key, value in plan.outputs().items():
            handle.write(f"{key}={value}\n")


def command_plan(arguments: argparse.Namespace) -> int:
    event = _read_event(arguments.event)
    client = _client() if arguments.api else None
    plan = plan_for_event(arguments.event_name, event, client, arguments.state_dir)
    if arguments.github_output:
        _write_outputs(plan, arguments.github_output)
    print(json.dumps(plan.outputs(), indent=2, sort_keys=True))
    return 0


def command_teardown(arguments: argparse.Namespace) -> int:
    removals = teardown_resources(arguments.pr, apply=not arguments.dry_run)
    if arguments.state_dir is not None:
        clear_state(arguments.state_dir, arguments.pr)
    if not arguments.dry_run:
        try:
            remove_launcher_metadata(arguments.pr)
        except OSError as error:
            print(f"the launcher metadata was not removed: {error}", file=sys.stderr)
            return 1
    for removal in removals:
        print(f"{removal.kind} {removal.name}: {removal.reason}")
    return 1 if any(removal.failed for removal in removals) else 0


def verify_approval(
    number: int,
    sha: str,
    client: GitHubClient | None = None,
) -> str:
    """Checks that an approval still holds before anything is deployed.

    The pull request may have been closed or moved on while the build was
    running. The deployment is only allowed to continue when the pull
    request is still open and still points at the approved commit.
    """
    if client is None:
        raise PreviewError("no GitHub client is available to verify the approval")
    pull_request = client.pull_request(number)
    state = str(pull_request.get("state", ""))
    if state != "open":
        raise PreviewError(f"pull request {number} is {state or 'not open'}, so it is not deployed")
    head = str(pull_request.get("head", {}).get("sha", ""))
    if sha and head and head != sha:
        raise PreviewError(
            f"pull request {number} now points at {head[:12]}, but {sha[:12]} was approved"
        )
    return head


def command_verify(arguments: argparse.Namespace) -> int:
    """Checks that the approval still holds."""
    client = _client()
    head = verify_approval(arguments.pr, arguments.sha, client)
    print(head)
    return 0


def command_state(arguments: argparse.Namespace) -> int:
    state = read_state(arguments.state_dir, arguments.pr)
    if state is None:
        print(f"no preview state for pull request {arguments.pr}")
        return 1
    print(state.to_json())
    return 0


def command_configure(arguments: argparse.Namespace) -> int:
    """Records the preview state and prints what the host needs."""
    state = ensure_preview(
        arguments.pr,
        arguments.template,
        arguments.state_dir,
        sha=arguments.sha or "",
    )
    ports = instance_ports(state.port_offset)
    values = {
        "instance": state.instance,
        "port_offset": str(state.port_offset),
        "dashboard_port": str(ports["dashboard"]),
        "url": state.url,
    }
    if arguments.github_output:
        with open(arguments.github_output, "a", encoding="utf-8") as handle:
            for key, value in values.items():
                handle.write(f"{key}={value}\n")
    for key, value in values.items():
        print(f"{key}={value}")
    return 0


def _carried_url(blob: dict[str, Any], state: str) -> str:
    """The address of a preview that is still the one a comment talks about.

    A preview that waits for an approval, that was denied, or that was
    removed has no address a reviewer should open, so the old one is not
    carried over.
    """
    if state in ("await-approval", "denied", "removed"):
        return ""
    return str(blob.get("url", ""))


def command_report(arguments: argparse.Namespace) -> int:
    """Writes the status comment of a preview."""
    client = _client()
    number = _pull_request_number(arguments.pr)
    existing = find_status_comment(client.comments(number))
    blob = read_status_blob(existing)
    state = arguments.state
    if state == "status":
        # A status request repeats what the comment already says.
        state = str(blob.get("state", "unknown"))
    approval = None
    recorded = blob.get("approved")
    if isinstance(recorded, dict) and recorded.get("sha"):
        same_commit = not arguments.sha or str(recorded["sha"]) == arguments.sha
        if same_commit and state in KEEPS_APPROVAL:
            approval = Approval(
                sha=str(recorded["sha"]),
                by=str(recorded.get("by", "")),
                at=str(recorded.get("at", "")),
            )
    if approval is None and arguments.approved_by and arguments.sha:
        approval = Approval(sha=arguments.sha, by=arguments.approved_by)
    plan = Plan(
        action=state,
        pr=number,
        sha=arguments.sha or str(blob.get("sha", "")),
        instance=instance_name(number),
        approved_by=approval.by if approval else "",
        url=arguments.url or _carried_url(blob, state),
    )
    body = status_comment_body(
        plan, url=plan.url, error=arguments.error or "", state=state, approval=approval
    )
    upsert_status_comment(client, number, body)
    print(f"reported the preview state of pull request {number}")
    return 0


def command_route(arguments: argparse.Namespace) -> int:
    """Writes or removes the ingress route of a preview."""
    if arguments.remove:
        removed = remove_route(arguments.directory, arguments.pr)
        print("removed the ingress route" if removed else "there was no ingress route")
        return 0
    state = read_state(arguments.state_dir, arguments.pr)
    if state is None:
        raise PreviewError(f"pull request {arguments.pr} has no preview state")
    ports = instance_ports(state.port_offset)
    content = render_route(
        arguments.template or DEFAULT_ROUTE_TEMPLATE,
        host=state.url.split("//", 1)[-1].split("/", 1)[0],
        port=ports["dashboard"],
        pr=arguments.pr,
        instance=state.instance,
    )
    path = write_route(arguments.directory, arguments.pr, content)
    print(f"wrote {path}")
    return 0


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    plan = commands.add_parser("plan", help="decide what a webhook payload asks for")
    plan.add_argument("--event-name", required=True)
    plan.add_argument("--event", default="-")
    plan.add_argument("--github-output")
    plan.add_argument("--state-dir")
    plan.add_argument(
        "--no-api",
        dest="api",
        action="store_false",
        help="decide without reading the pull request from the API",
    )
    plan.set_defaults(api=True, function=command_plan)

    teardown = commands.add_parser("teardown", help="remove the resources of a preview")
    teardown.add_argument("--pr", type=int, required=True)
    teardown.add_argument("--dry-run", action="store_true")
    teardown.add_argument("--state-dir")
    teardown.set_defaults(function=command_teardown)

    state = commands.add_parser("state", help="show the recorded state of a preview")
    state.add_argument("--pr", type=int, required=True)
    state.add_argument("--state-dir")
    state.set_defaults(function=command_state)

    verify = commands.add_parser(
        "verify", help="check that the approval still holds before a deployment"
    )
    verify.add_argument("--pr", type=int, required=True)
    verify.add_argument("--sha", default="")
    verify.set_defaults(function=command_verify)

    configure = commands.add_parser(
        "configure", help="record the preview state and print what the host needs"
    )
    configure.add_argument("--pr", type=int, required=True)
    configure.add_argument("--template", required=True)
    configure.add_argument("--sha", default="")
    configure.add_argument("--state-dir")
    configure.add_argument("--github-output")
    configure.set_defaults(function=command_configure)

    report = commands.add_parser("report", help="write the status comment of a preview")
    report.add_argument("--pr", type=int, required=True)
    report.add_argument("--state", default="deployed")
    report.add_argument("--sha", default="")
    report.add_argument("--url", default="")
    report.add_argument("--approved-by", default="")
    report.add_argument("--error", default="")
    report.set_defaults(function=command_report)

    route = commands.add_parser("route", help="write or remove the ingress route")
    route.add_argument("--pr", type=int, required=True)
    route.add_argument("--template", default="")
    route.add_argument("--directory", required=True)
    route.add_argument("--state-dir")
    route.add_argument("--remove", action="store_true")
    route.set_defaults(function=command_route)

    arguments = parser.parse_args(argv)
    try:
        return int(arguments.function(arguments))
    except PreviewError as error:
        print(f"preview: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
