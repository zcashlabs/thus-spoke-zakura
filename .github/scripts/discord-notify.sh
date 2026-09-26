#!/usr/bin/env bash
# Posts a Discord embed describing the current GitHub event.
#
# Invoked by .github/workflows/discord-notifications.yml; see
# docs/discord-notifications.md. Reads the event payload from
# GITHUB_EVENT_PATH. workflow_run events carry no usable pull request for
# forks, so those are resolved through the GitHub API (needs GH_TOKEN).
#
# DISCORD_DRY_RUN=1 prints the payload instead of sending it.
# Never echo DISCORD_WEBHOOK or run curl verbosely: the URL is the credential.
set -euo pipefail

: "${GITHUB_EVENT_NAME:?}" "${GITHUB_EVENT_PATH:?}" "${GITHUB_REPOSITORY:?}"

COLOR_BLURPLE=5793266
COLOR_BLUE=3447003
COLOR_GREEN=5763719
COLOR_PURPLE=10181046
COLOR_RED=15548997

skip() {
  echo "No notification: $*"
  exit 0
}

event() { jq -r "$1 // empty" "$GITHUB_EVENT_PATH"; }
pr() { jq -r "$1 // empty" <<<"$pr_json"; }

reviews_enabled() { [ "${DISCORD_REVIEW_NOTIFICATIONS:-}" = "true" ]; }

# Open pull request whose head is owner:branch, preferring an exact SHA match.
find_pr() {
  gh api -X GET "repos/$GITHUB_REPOSITORY/pulls" \
    -f state=open -f head="$1:$2" -F per_page=100 |
    jq -c --arg sha "$3" '(map(select(.head.sha == $sha)) + .) | first // empty'
}

# Markdown list of reviewers and teams requested on the pull request.
requested_reviewers() {
  jq -r '[(.requested_reviewers // [])[] | "`\(.login)`"]
    + [(.requested_teams // [])[] | "`team:\(.name)`"] | join(", ")' <<<"$pr_json"
}

pr_json=""
title=""
color=0
url=""
description=""
fields='[]'

add_field() {
  fields=$(jq -c --arg name "$1" --arg value "$2" --argjson inline "${3:-true}" \
    '. + [{name: $name, value: $value, inline: $inline}]' <<<"$fields")
}

add_pr_summary() {
  local number pr_title pr_url author head base
  number=$(pr .number)
  pr_title=$(pr .title)
  pr_url=$(pr .html_url)
  author=$(pr .user.login)
  base=$(pr .base.ref)
  if [ "$(pr .head.repo.full_name)" = "$GITHUB_REPOSITORY" ]; then
    head=$(pr .head.ref)
  else
    head=$(pr .head.label)
  fi
  [ "${#pr_title}" -le 200 ] || pr_title="${pr_title:0:197}..."

  description="**#${number} — ${pr_title}**"$'\n'"[View Pull Request](${pr_url})"
  add_field "Author" "👤 [${author}](https://github.com/${author})"
  add_field "Branches" "🌿 \`${head}\` → \`${base}\`"
}

handle_pull_request() {
  pr_json=$(jq -c '.pull_request' "$GITHUB_EVENT_PATH")
  url=$(pr .html_url)
  local draft action reviewers
  draft=$(pr .draft)
  action=$(event .action)

  case "$action" in
    opened | ready_for_review | reopened)
      [ "$draft" != "true" ] || skip "draft pull request"
      case "$action" in
        opened) title="🚀 New Pull Request" color=$COLOR_BLURPLE ;;
        ready_for_review) title="🚀 Pull Request Ready for Review" color=$COLOR_BLURPLE ;;
        reopened) title="🔁 Pull Request Reopened" color=$COLOR_BLURPLE ;;
      esac
      add_pr_summary
      reviewers=$(requested_reviewers)
      if reviews_enabled && [ -n "$reviewers" ]; then
        add_field "Reviewers" "👀 ${reviewers}" false
      fi
      ;;
    closed)
      if [ "$(pr .merged)" = "true" ]; then
        title="🟣 Pull Request Merged" color=$COLOR_PURPLE
      else
        [ "$draft" != "true" ] || skip "closed draft pull request"
        title="🔴 Pull Request Closed" color=$COLOR_RED
      fi
      add_pr_summary
      ;;
    review_requested)
      reviews_enabled || skip "review notifications disabled"
      [ "$draft" != "true" ] || skip "draft pull request"
      # Reviewers picked while creating the PR are already listed in the
      # "opened" embed; GitHub sends these events in the same instant.
      if jq -e '(.updated_at | fromdate) - (.created_at | fromdate) <= 5' \
        <<<"$pr_json" >/dev/null; then
        skip "reviewer requested at creation"
      fi
      local reviewer
      reviewer=$(event .requested_reviewer.login)
      if [ -z "$reviewer" ]; then
        reviewer=$(event .requested_team.name)
        [ -z "$reviewer" ] || reviewer="team:${reviewer}"
      fi
      title="👀 Review Requested" color=$COLOR_BLUE
      add_pr_summary
      add_field "Reviewer" "👀 \`${reviewer:-unknown}\`"
      ;;
    *) skip "unhandled pull_request action: $action" ;;
  esac
}

handle_ci_run() {
  [ "$(event .workflow_run.conclusion)" = "failure" ] || skip "CI did not fail"
  local run_url branch sha run_event
  run_url=$(event .workflow_run.html_url)
  branch=$(event .workflow_run.head_branch)
  sha=$(event .workflow_run.head_sha)
  run_event=$(event .workflow_run.event)
  url=$run_url
  title="❌ CI Failed" color=$COLOR_RED

  case "$run_event" in
    pull_request)
      pr_json=$(find_pr "$(event .workflow_run.head_repository.owner.login)" "$branch" "$sha")
      [ -n "$pr_json" ] || skip "no open pull request for $branch"
      add_pr_summary
      description="${description}"$'\n'"[View failed run](${run_url})"
      ;;
    push)
      # Branch pushes are covered by their pull_request run; only a broken
      # default branch is worth a separate alert.
      [ "$branch" = "$(event .repository.default_branch)" ] || skip "push to non-default branch $branch"
      local message
      message=$(event .workflow_run.head_commit.message | head -n 1)
      title="❌ CI Failed on ${branch}"
      description="**${message:-Commit ${sha:0:7}}**"$'\n'"[View failed run](${run_url})"
      add_field "Commit" "[\`${sha:0:7}\`](${GITHUB_SERVER_URL:-https://github.com}/${GITHUB_REPOSITORY}/commit/${sha})"
      add_field "Author" "👤 $(event .workflow_run.head_commit.author.name)"
      ;;
    *) skip "CI triggered by $run_event" ;;
  esac
}

handle_approval_relay() {
  reviews_enabled || skip "review notifications disabled"
  [ "$(event .workflow_run.conclusion)" = "success" ] || skip "relay did not run"
  local reviewer review
  reviewer=$(event .workflow_run.actor.login)
  pr_json=$(find_pr "$(event .workflow_run.head_repository.owner.login)" \
    "$(event .workflow_run.head_branch)" "$(event .workflow_run.head_sha)")
  [ -n "$pr_json" ] || skip "no open pull request for relay run"

  # The relay run is untrusted fork context: confirm via the API that the
  # reviewer's latest decisive review is really an approval.
  review=$(gh api "repos/$GITHUB_REPOSITORY/pulls/$(pr .number)/reviews" --paginate |
    jq -cs --arg login "$reviewer" 'add
      | map(select(.user.login == $login
          and (.state | IN("APPROVED", "CHANGES_REQUESTED", "DISMISSED"))))
      | last // empty')
  [ "$(jq -r '.state // empty' <<<"$review")" = "APPROVED" ] || skip "no current approval from $reviewer"

  title="🟢 Pull Request Approved" color=$COLOR_GREEN
  url=$(jq -r '.html_url' <<<"$review")
  add_pr_summary
  add_field "Reviewer" "👤 [${reviewer}](https://github.com/${reviewer})"
}

case "$GITHUB_EVENT_NAME" in
  pull_request | pull_request_target) handle_pull_request ;;
  workflow_run)
    case "$(event .workflow_run.name)" in
      CI) handle_ci_run ;;
      "Discord Review Relay") handle_approval_relay ;;
      *) skip "unhandled workflow: $(event .workflow_run.name)" ;;
    esac
    ;;
  *) skip "unhandled event: $GITHUB_EVENT_NAME" ;;
esac

add_field "Repository" "\`${GITHUB_REPOSITORY}\`" false

payload=$(jq -n \
  --arg title "$title" \
  --arg description "$description" \
  --arg url "$url" \
  --argjson color "$color" \
  --argjson fields "$fields" \
  '{
    username: "GitHub",
    allowed_mentions: {parse: []},
    embeds: [{
      title: $title,
      description: $description,
      url: $url,
      color: $color,
      fields: $fields,
      timestamp: (now | todate)
    }]
  }')

if [ "${DISCORD_DRY_RUN:-}" = "1" ]; then
  echo "$payload"
  exit 0
fi

if [ -z "${DISCORD_WEBHOOK:-}" ]; then
  echo "::warning::DISCORD_WEBHOOK secret is not set; skipping Discord notification"
  exit 0
fi

response=$(mktemp)
http_code=$(curl --silent --show-error --retry 3 --max-time 30 \
  --output "$response" --write-out "%{http_code}" \
  -H "Content-Type: application/json" \
  --data "$payload" \
  "$DISCORD_WEBHOOK")

if [ "$http_code" -lt 200 ] || [ "$http_code" -ge 300 ]; then
  echo "::error::Discord webhook returned HTTP $http_code"
  cat "$response"
  exit 1
fi

echo "Discord notification sent (HTTP $http_code): $title"
