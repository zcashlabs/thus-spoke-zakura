# Maintainer triage bot

The maintainer triage workflow reacts to new and updated issues and pull requests. It combines the item with relevant source excerpts, recent file history, comments, reviews, changed-file patches, and up to 200 recent issues and pull requests. Its report covers:

- priority and affected areas;
- whether a proposed solution addresses the root cause or is partial, risky, or a band-aid;
- related issues and pull requests that may point to a systemic problem;
- an immediate safe action, a durable solution, and regression tests;
- for pull requests, a change summary, correctness assessment, blockers, and merge readiness.

Every report is written to the GitHub Actions job summary and sent to Telegram. GitHub comments and labels are disabled by default.

## Setup

Create a Telegram bot with [BotFather](https://t.me/BotFather), start a chat with the bot (or add it to the destination group), and determine the numeric chat ID from the Bot API `getUpdates` response. Then add these Actions secrets in the repository settings:

- `OPENAI_API_KEY`: an OpenAI API project key;
- `TELEGRAM_BOT_TOKEN`: the token issued by BotFather;
- `TELEGRAM_CHAT_ID`: the destination user, group, or channel ID.

The GitHub CLI prompts for secret values without putting them on the command line:

```console
gh secret set OPENAI_API_KEY -R zcashlabs/thus-spoke-zakura
gh secret set TELEGRAM_BOT_TOKEN -R zcashlabs/thus-spoke-zakura
gh secret set TELEGRAM_CHAT_ID -R zcashlabs/thus-spoke-zakura
```

The default model is `gpt-6-luna` at medium reasoning effort. It is selected for a frequent triage workflow; evaluate report quality on representative issues before changing it. Optional Actions variables are:

| Variable | Default | Purpose |
| --- | --- | --- |
| `TRIAGE_MODEL` | `gpt-6-luna` | OpenAI Responses API model |
| `TRIAGE_REASONING_EFFORT` | `medium` | `low`, `medium`, `high`, or `xhigh` |
| `TRIAGE_GITHUB_COMMENT` | `false` | Create or update one bot comment per item |
| `TRIAGE_APPLY_LABELS` | `false` | Apply suggested labels only when they already exist |
| `TRIAGE_ALLOWED_ASSOCIATIONS` | empty | Optional comma-separated allowlist such as `OWNER,MEMBER,COLLABORATOR` |
| `TRIAGE_HISTORY_PAGES` | `2` | Number of 100-item history pages to correlate, from 1 to 5 |

For example:

```console
gh variable set TRIAGE_GITHUB_COMMENT -R zcashlabs/thus-spoke-zakura --body true
```

Leave `TRIAGE_ALLOWED_ASSOCIATIONS` empty to analyze every human-authored issue and pull request. On a public repository, setting an allowlist limits API-cost abuse but skips first-time contributors until a maintainer manually dispatches the workflow.

## Security and behavior

Pull requests use two workflows for privilege separation. `Triage event relay` runs on the unprivileged `pull_request` event without secrets and emits only a completed-run signal containing the pull request number. `Maintainer triage` runs from the trusted default branch on `workflow_run`, where secrets are available, and fetches pull request metadata and patches through the GitHub API. No artifact crosses the boundary. Neither workflow checks out, imports, builds, or executes contributor code.

Repository Actions approval rules can delay the relay for a first-time fork contributor. Once the relay is allowed to run, the analysis starts immediately after it completes. This design avoids `pull_request_target`, which GitHub plans to block by default for public repositories beginning November 2, 2026.

Issues run `Maintainer triage` directly. Titles, bodies, comments, reviews, and patches are always treated as untrusted text.

The evidence bundle contains public repository code and GitHub discussion text and is sent to the configured OpenAI API project with response storage disabled. Reassess that data flow before reusing this workflow in a private repository or for sensitive reports.

The model has no tools and cannot mutate GitHub. The wrapper performs only the configured actions: Telegram delivery, an optional sticky comment, and optional application of labels that already exist in the repository. Generated conclusions remain maintainer advice and must be verified before merging.

The workflow grants issue and pull-request write access so the two opt-in features can be enabled without editing the workflow. If those features will never be enabled, reduce both permissions to `read`.

Public events can consume OpenAI API quota. Keep project spend limits enabled, use `TRIAGE_ALLOWED_ASSOCIATIONS` if abuse becomes a concern, and monitor Actions failures for rate limits or delivery errors.

## Test and operate

Run the dependency-free unit suite with:

```console
python3 -m unittest discover -s .github/triage -p 'test_*.py'
```

After adding the secrets, use **Actions → Maintainer triage → Run workflow** with an existing issue or pull request number. Manual dispatch uses the same data collection, Telegram delivery, and optional publishing path as event-triggered runs.

Failures stop the workflow and are visible in the job log. API tokens are never printed; Telegram errors are reported without echoing the token-bearing endpoint.
