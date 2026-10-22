# Appendix D: Pull request previews

A **preview** is a throwaway Regtest environment for one pull request. It
lets a reviewer open the dashboard of a branch, mine a block, fund an
account, and send test ZEC without installing anything.

The preview is disposable. It runs on a separate host, it holds no value, and
it is removed when the pull request closes.

This appendix describes how a preview is reached, how it is isolated, what it
is named, who may approve it, and how it is removed. The workflow that
implements it is `.github/workflows/preview.yml`, and the rules it follows
live in `tools/preview/preview.py`.

## The access path

Every host-published service of a `ths` instance stays on `127.0.0.1`. A
preview keeps that rule: the launcher publishes the dashboard, the Zakura RPC
endpoint, the P2P port, and lightwalletd on the loopback interface of the
preview host, and on nothing else.

Reviewers therefore do not reach the preview directly. They reach an **ingress**
that the maintainers operate on the same host:

```
reviewer ──https──▶ ingress (preview host) ──http──▶ 127.0.0.1:<dashboard port>
```

The ingress is a reverse proxy with a certificate for the preview domain. It
listens on the public interface of the preview host and forwards to the
loopback port of the preview. Nothing in the repository changes that
arrangement: the workflow only writes a route for the ingress and asks it to
reload.

The address a reviewer opens is built from the `PREVIEW_BASE_URL` repository
variable, for example `https://pr-412.preview.example.com`. The `{pr}`
placeholder is replaced with the pull request number.

The ingress decides who may open a preview. Keep it behind the network or the
authentication the maintainers already use. A preview is not a place for
anything private, and it is not a security boundary.

## The isolation

A preview is its own `ths` instance, so it is isolated in the same way a
developer's instance is:

| Part | Preview |
| --- | --- |
| Chain | Its own Regtest chain in its own volume. |
| Wallet | Its own wallet volume with its own development accounts. |
| Network | Its own Docker network. |
| Ports | Its own loopback ports, offset from the defaults. |
| Host | A runner that the maintainers control, separate from production. |
| Secrets | None. A preview holds no key, no token, and no production setting. |

Nothing is shared with production, and nothing is shared with another
preview. Two previews of two pull requests use two instances, two networks,
two sets of volumes, and two port blocks.

The pull request code does run on the preview host, because that is what a
preview is for. Treat the preview host as untrusted: keep it away from
production credentials, keep it off any network that reaches production, and
do not give it cloud instance metadata.

## The names

Every resource of a preview is named after the pull request, so a reviewer
can tell whose preview it is and a cleanup can never touch another
instance's data.

| Thing | Name for pull request 412 |
| --- | --- |
| Launcher instance | `preview-pr-412` |
| Containers | `ths-preview-pr-412-app`, `-zakura`, `-lightwalletd`, `-init` |
| Volumes | `ths-preview-pr-412-chain`, `-wallet`, `-lightwalletd`, `-config` |
| Docker network | `ths-preview-pr-412` |
| Docker label | `com.zakura.ths.instance=preview-pr-412` |
| Ingress route | `ths-preview-pr-412.conf` |
| Host state | `pr-412.json` |

The instance name fits the launcher's rules: one to forty lowercase letters,
digits, or internal hyphens.

The ports are the launcher's default ports plus a **port offset** that is a
multiple of ten. The offset is chosen from the pull request number and then
recorded on the preview host, so a later commit reuses it and the address a
reviewer already has keeps working. When the recorded ports are busy, the
workflow picks the next free block and says so in the status comment.

## The approval

Pull request code never runs before a maintainer approved it, and it never
runs with a secret.

1. A pull request is opened, or a commit is pushed to it. The workflow
   decides what to do and posts a status comment that says the preview waits
   for approval.
2. A maintainer comments `/preview approve` on the pull request. The
   workflow checks the comment author's repository association, so only an
   owner, a member, or a collaborator can approve.
3. The approval is bound to the head commit at the moment of the comment.
   The workflow builds that exact commit, checks that the checkout still
   matches it, and deploys it.
4. A later commit needs a later approval. The recorded approval names one
   commit, and a new commit does not inherit it.

Other commands on the same pull request:

| Command | Effect |
| --- | --- |
| `/preview approve` | Approve the current head commit and deploy it. |
| `/preview deny` | Withdraw the approval. |
| `/preview teardown` | Remove the preview now. |
| `/preview status` | Repeat the current state of the preview. |

A comment from somebody without write access is ignored, and the workflow
says so in its log. A pull request author cannot approve their own pull
request.

The `preview` GitHub environment holds the deployment secrets. Only the
deploy and teardown jobs name that environment, so the build job that runs
the pull request code never receives them. The environment may also carry
required reviewers, which adds a second, independent approval in the GitHub
interface.

## The permissions

Every job asks for the least it needs, and the workflow starts with read
access to the repository only.

| Job | Runs | Permissions | Sees a secret |
| --- | --- | --- | --- |
| `plan` | The workflow's own code. | `contents: read`, `pull-requests: write` | No |
| `build` | The pull request code. | `contents: read` | No |
| `deploy` | The workflow's own code and the built image. | `contents: read`, `pull-requests: write` | Only the `preview` environment |
| `teardown` | The workflow's own code. | `contents: read`, `pull-requests: write` | Only the `preview` environment |
| `status` | The workflow's own code. | `contents: read`, `pull-requests: write` | No |

The `plan`, `deploy`, `teardown`, and `status` jobs check out the base
branch, so the code that decides and that cleans up is never the code of the
pull request. The `build` job is the only job that checks out the pull
request, and it has no secret and no write scope.

The credentials are short-lived: the workflow uses the automatic
`GITHUB_TOKEN` of the run, which expires with the run.

## The settings

These repository variables describe the preview host. They are not secrets,
and a maintainer sets them once.

| Variable | Meaning |
| --- | --- |
| `PREVIEW_BASE_URL` | The address template, with `{pr}` for the pull request number. Required. |
| `PREVIEW_PROXY_DIR` | The directory the ingress reads its route files from. |
| `PREVIEW_ROUTE_TEMPLATE` | The route file body, with `{host}`, `{port}`, `{pr}`, and `{instance}`. The default suits Caddy. |
| `PREVIEW_PROXY_RELOAD` | The command that asks the ingress to reload. It runs as a shell command on the preview host, so only a maintainer may set it. |

`THS_PREVIEW_STATE_DIR` on the preview host names the directory that holds
one state file per preview. It defaults to
`~/.local/state/ths-preview`.

The workflow fails with a clear message when `PREVIEW_BASE_URL` is missing,
rather than deploying a preview nobody can reach.

## The cleanup

A preview is removed when the pull request closes or is merged. The removal
has two parts:

1. The launcher is asked to stop the instance, which deletes the containers,
   the volumes, the network, and the instance metadata the way a local
   `ths stop` does.
2. Anything the launcher did not remove is removed by label. Every resource
   that still carries `com.zakura.ths.instance=preview-pr-412` is deleted,
   and a resource that carries a different label is kept.

The second part matters when the launcher is gone, for example after the
preview host restarted. It is also the reason a cleanup can never remove
another instance's data.

The ingress route is removed with the instance, and the ingress is reloaded.

When the automatic removal fails, run it again by hand from the
**Actions** tab: run the **Pull request preview** workflow with the pull
request number and the action `teardown`. The same workflow deploys a preview
again with the action `deploy`, which needs the approval of the current head
commit.

## The cost

A preview needs one host that the maintainers control, with Docker and the
GitHub Actions runner installed, and with the ingress. That host runs
whether or not a preview is open. Pull request builds and the workflow
itself run on GitHub-hosted runners, so they use the included minutes.

The alternative, a preview per pull request on a hosted platform, costs
money per preview and needs a place to keep the ingress credentials. The
design here spends one host instead.

## What a preview is not

- It is not a security boundary. The pull request code runs on the preview
  host.
- It is not a production environment. It is Regtest, it is disposable, and
  it holds no value.
- It is not a replacement for the checks in [Appendix B](development.md). A
  green preview does not mean the branch is correct.
- It does not show a change to the launcher itself unless the launcher is
  built from the branch, which the workflow does.

**Next:** [troubleshoot a preview](troubleshooting.md), or go back to
[Appendix B](development.md).
