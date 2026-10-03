# Discord notifications

Pull request activity is posted to the project Discord through an incoming
webhook. The goal is high signal and low noise for a community channel.

| Event | Message |
| --- | --- |
| PR opened | 🚀 New Pull Request |
| Draft marked ready for review | 🚀 Pull Request Ready for Review |
| PR reopened | 🔁 Pull Request Reopened |
| PR merged, including a merged draft | 🟣 Pull Request Merged |
| PR closed without merging | 🔴 Pull Request Closed |
| `CI` failed on a PR | ❌ CI Failed |
| `CI` failed on the default branch | ❌ CI Failed on `<branch>` |
| Reviewer requested after the PR was opened | 👀 Review Requested (opt-in) |
| Review approved | 🟢 Pull Request Approved (opt-in) |

Deliberately not posted: open or closed drafts, new commits, comments,
non-approving reviews, passing CI, and CI on branch pushes (covered by the
PR run). Review and approval messages are off by default. When they are on,
reviewers named in the first few seconds are listed on the open message
instead of a separate Review Requested post.

## Setup

1. In Discord, open the channel settings → Integrations → Webhooks and create
   a webhook. Copy its URL.
2. In GitHub, go to Settings → Secrets and variables → Actions and add a
   repository secret named `DISCORD_WEBHOOK` containing that URL.
3. Optional: to also post review requested and approved messages, add a
   repository variable `DISCORD_REVIEW_NOTIFICATIONS` set to `true`.

Without the secret the workflow logs a warning and posts nothing.

## How it works

- [`discord-notifications.yml`](../.github/workflows/discord-notifications.yml)
  runs [`discord-notify.sh`](../.github/scripts/discord-notify.sh), which builds
  the embed from the event payload and posts it.
- PR events use `pull_request_target` so the secret is available for PRs from
  forks. The job checks out `.github/scripts` from the base ref and reads the
  event payload. It must never check out or run pull request code. CI and
  approval events also call the GitHub API.
- CI failures use `workflow_run` on the `CI` workflow. `check_suite` events are
  not delivered for GitHub Actions check suites.
- Fork PR reviews run without secrets, so
  [`discord-review-relay.yml`](../.github/workflows/discord-review-relay.yml)
  succeeds on approvals and the notifier reacts to it via `workflow_run`,
  confirming the approval through the GitHub API before posting. That relay
  runs only after GitHub allows the fork's workflow to run. If a maintainer
  must approve the workflow run first, the relay never starts and no approval
  notification is posted.
- `workflow_run` triggers only take effect once the workflows are on the
  default branch.
- Messages are sent with `allowed_mentions` disabled, so `@everyone` in a PR
  title cannot ping the channel.

## Testing changes

The script prints the payload instead of sending it when `DISCORD_DRY_RUN=1`:

```sh
DISCORD_DRY_RUN=1 GITHUB_EVENT_NAME=pull_request_target \
  GITHUB_EVENT_PATH=event.json GITHUB_REPOSITORY=zcashlabs/thus-spoke-zakura \
  .github/scripts/discord-notify.sh
```

`workflow_run` events also call `gh api`, which needs `GH_TOKEN`.
