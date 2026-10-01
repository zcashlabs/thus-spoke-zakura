#!/usr/bin/env python3
"""Regenerates book/images/*.svg from the specs in this file.

    python3 diagrams/build.py

Layout is hand-placed, not solved: every coordinate here was chosen so the
figures share a grid, keep their gaps even, and never route an arrow through a
box. Labels stay to a few words — the sentences belong in the numbered list
under each figure in the chapter, not inside the drawing.
"""

import pathlib
import sys

OUT = pathlib.Path(__file__).resolve().parent.parent / "book" / "images"

# One palette, two themes. Colour means something here: the accent marks the
# path the chapter is about, and nothing else uses it.
THEME = """
svg {
  --bg: #fdfaf8;
  --ink: #2c2430;
  --muted: #7a6a7a;
  --rule: #e0d3da;
  --surface: #ffffff;
  --zone: #f7f0f3;
  --accent: #b24a6e;
  --accent-soft: #fceef3;
}
@media (prefers-color-scheme: dark) {
  svg {
    --bg: #1b171f;
    --ink: #efe8f0;
    --muted: #a695a6;
    --rule: #3d3442;
    --surface: #252030;
    --zone: #201b27;
    --accent: #e79ab7;
    --accent-soft: #3a2331;
  }
}
text { font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto,
       "Helvetica Neue", Arial, sans-serif; fill: var(--ink); }
.t { font-size: 16px; font-weight: 600; }
.k { font-size: 11px; font-weight: 600; letter-spacing: .1em; fill: var(--muted); }
.l { font-size: 14px; }
.s { font-size: 12px; fill: var(--muted); }
.a { font-size: 13px; font-weight: 600; fill: var(--accent); }
.card { fill: var(--surface); stroke: var(--rule); stroke-width: 1; }
.card-a { fill: var(--accent-soft); stroke: var(--accent); stroke-width: 1; }
.zone { fill: var(--zone); stroke: none; }
.note { fill: none; stroke: var(--rule); stroke-width: 1; stroke-dasharray: 3 3; }
.flow { stroke: var(--accent); stroke-width: 2; fill: none; }
.thin { stroke: var(--rule); stroke-width: 1.5; fill: none; }
.lead { stroke: var(--rule); stroke-width: 1; fill: none; stroke-dasharray: 2 4; }
"""

CHAR_L, CHAR_S = 7.1, 6.0  # rough advance widths, used only to catch overflow


class D:
    def __init__(self, name, w, h, title=None, aria=""):
        self.name, self.w, self.h, self.title, self.aria = name, w, h, title, aria
        self.body, self.warnings = [], []

    # --- primitives -----------------------------------------------------
    def zone(self, x, y, w, h, label=None):
        self.body.append(f'<rect class="zone" x="{x}" y="{y}" width="{w}" height="{h}" rx="12"/>')
        if label:
            self.body.append(f'<text class="k" x="{x + 16}" y="{y + 22}">{label}</text>')

    def card(self, x, y, w, h, title, sub=None, kind="plain"):
        cls = {"plain": "card", "accent": "card-a", "note": "note"}[kind]
        self.body.append(f'<rect class="{cls}" x="{x}" y="{y}" width="{w}" height="{h}" rx="8"/>')
        cx = x + w / 2
        if sub:
            self.body.append(f'<text class="l" x="{cx}" y="{y + h / 2 - 3}" text-anchor="middle">{title}</text>')
            self.body.append(f'<text class="s" x="{cx}" y="{y + h / 2 + 15}" text-anchor="middle">{sub}</text>')
            self._fit(sub, w, CHAR_S)
        else:
            self.body.append(f'<text class="l" x="{cx}" y="{y + h / 2 + 4}" text-anchor="middle">{title}</text>')
        self._fit(title, w, CHAR_L)
        return (x, y, w, h)

    def row(self, n, gap=28, pad=48):
        """Even x positions and a shared width for n cards across the figure."""
        w = (self.w - 2 * pad - gap * (n - 1)) / n
        return [pad + i * (w + gap) for i in range(n)], w

    def text(self, x, y, s, cls="s", anchor="middle"):
        self.body.append(f'<text class="{cls}" x="{x}" y="{y}" text-anchor="{anchor}">{s}</text>')

    def arrow(self, pts, label=None, cls="flow", lxy=None, head=True):
        d = "M" + " L".join(f"{x} {y}" for x, y in pts)
        m = f' marker-end="url(#ar-{ "a" if cls == "flow" else "n" })"' if head else ""
        self.body.append(f'<path class="{cls}" d="{d}"{m}/>')
        if label:
            (x1, y1), (x2, y2) = pts[0], pts[-1]
            if lxy is None and x1 == x2:
                self.text(x1 + 12, (y1 + y2) / 2 + 4, label, "s", "start")
                return
            if lxy is None:
                lxy = ((x1 + x2) / 2, min(y1, y2) - 14)
            self.text(lxy[0], lxy[1], label, "s")

    # --- checks and output ----------------------------------------------
    def _fit(self, s, w, adv):
        if len(s) * adv > w - 18:
            self.warnings.append(f"{self.name}: {s!r} may overflow {w}px")

    def save(self):
        head = (
            f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="44 0 {self.w - 88} {self.h}" '
            f'width="{self.w - 88}" height="{self.h}" role="img" aria-label="{self.aria}">'
        )
        defs = (
            '<defs>'
            '<marker id="ar-a" viewBox="0 0 10 8" refX="9" refY="4" markerWidth="8" markerHeight="7" '
            'orient="auto"><path d="M0 0 L10 4 L0 8 z" fill="var(--accent)"/></marker>'
            '<marker id="ar-n" viewBox="0 0 10 8" refX="9" refY="4" markerWidth="7" markerHeight="6" '
            'orient="auto"><path d="M0 0 L10 4 L0 8 z" fill="var(--rule)"/></marker>'
            '</defs>'
        )
        parts = [head, f"<style>{THEME}</style>", defs]
        if self.title:
            parts.append(f'<text class="t" x="48" y="46">{self.title}</text>')
        parts += self.body + ["</svg>", ""]
        (OUT / f"{self.name}.svg").write_text("\n".join(parts))
        return self.warnings




# ---------------------------------------------------------------------------
# The figures. One claim each; the chapter's numbered list carries the detail.
# Every figure is 1000 wide or less so it renders near full size in the page.
# ---------------------------------------------------------------------------

def instance_lifecycle():
    d = D("instance-lifecycle", 1000, 412, "Starting an instance, and throwing it away",
          "Five startup stages left to right: ths start, checking Docker and images, claiming the "
          "fixed loopback ports, starting Zakura then lightwalletd then the app, and ready. A note "
          "records that a fresh chain opens near height 103. Below, Ctrl+C or any failure deletes "
          "only this instance.")
    xs, w = d.row(5, gap=24)
    y, h = 96, 64
    for x, (t, s) in zip(xs, [("ths start", "--name, --port-offset"), ("Check Docker", "all three images"),
                              ("Claim ports", "fixed, or +offset"), ("Start services", "node, index, app"),
                              ("Ready", "endpoints printed")]):
        d.card(x, y, w, h, t, s, kind="accent" if t == "Ready" else "plain")
    for a, b in zip(xs, xs[1:]):
        d.arrow([(a + w, y + h / 2), (b - 8, y + h / 2)])
    d.card(516, 202, 436, 54, "The chain opens near height 103, not 1",
           "coinbase maturity first, then Account 1 is funded", kind="note")
    for x in (xs[3] + w / 2, xs[4] + w / 2):
        d.arrow([(x, y + h), (x, 202)], cls="lead", head=False)
    b2x, b2w = d.row(2, gap=32)[0], d.row(2, gap=32)[1]
    d.card(b2x[0], 304, b2w, 64, "Ctrl+C, or any failure", "at any stage above")
    d.card(b2x[1], 304, b2w, 64, "Deletes this instance only",
           "its containers, its four volumes, its network")
    d.arrow([(b2x[0] + b2w, 336), (b2x[1] - 8, 336)])
    d.arrow([(xs[2] + w / 2, y + h), (xs[2] + w / 2, 276), (b2x[0] + 80, 276), (b2x[0] + 80, 296)],
            cls="lead", head=False)
    return d


def architecture_topology():
    d = D("architecture-topology", 1000, 520, "One instance: three containers, four volumes",
          "Your machine holds the launcher, a browser and your own code; every published port binds "
          "to loopback. Inside a private Docker network sit the app, lightwalletd and Zakura "
          "containers, each backed by its own volume.")
    xs, w = d.row(3)
    d.zone(48, 72, 904, 116, "YOUR MACHINE")
    for x, t, s in zip(xs, ["ths launcher", "Browser", "Your app or scripts"],
                       ["starts and deletes it", "the dashboard", "HTTP, RPC, gRPC"]):
        d.card(x, 104, w, 60, t, s)
    d.text(500, 220, "every published port binds 127.0.0.1 only", "s")
    for x in (xs[0] + w / 2, xs[2] + w / 2):
        d.arrow([(x, 188), (x, 236)], cls="lead", head=False)
    d.zone(48, 248, 904, 152, "PRIVATE DOCKER NETWORK  tsz-&lt;name&gt;")
    for x, t, s in zip(xs, ["app", "lightwalletd", "zakura"],
                       ["tsz-server + React · :8080", "compact blocks · :9067", "Regtest node · :18232"]):
        d.card(x, 284, w, 64, t, s, kind="accent" if t == "app" else "plain")
    d.arrow([(xs[0] + w, 316), (xs[1] - 8, 316)])
    d.arrow([(xs[1] + w, 316), (xs[2] - 8, 316)])
    vx, vw = d.row(4, gap=20)
    d.text(500, 428, "VOLUMES — DELETED WITH THE INSTANCE", "k")
    for x, t, s in zip(vx, ["wallet", "lightwalletd", "chain", "config"],
                       ["tsz.db + wallet.db", "block cache", "Zakura state", "zakurad.toml"]):
        d.card(x, 444, vw, 52, t, s, kind="note")
    return d


def state_ownership():
    d = D("state-ownership", 1000, 372, "One change, and who owns each answer",
          "The chain flows left to right from Zakura through lightwalletd's index into the server "
          "wallet's scan, which publishes an account snapshot the dashboard reads. SQLite activity "
          "is a separate path, written when the server sends rather than when it scans.")
    xs, w = d.row(5, gap=24)
    y, h = 104, 68
    for x, (t, s) in zip(xs, [("Zakura", "chain, tip hash"), ("lightwalletd", "compact index"),
                              ("wallet.db", "scan + rewind"), ("Snapshot", "balances, state"),
                              ("Dashboard", "query hooks")]):
        d.card(x, y, w, h, t, s, kind="accent" if t == "Snapshot" else "plain")
    for a, b, lab in zip(xs, xs[1:], ["blocks", "compact blocks", "canonical check", "/api/v1/accounts"]):
        d.arrow([(a + w, y + h / 2), (b - 8, y + h / 2)], lab, lxy=((a + w + b) / 2, y - 12))
    hx, hw = d.row(2, gap=32)
    d.card(hx[0], 250, hw, 56, "tsz.db activity", "txid + key, written on send")
    d.card(hx[1], 250, hw, 56, "GET /api/v1/activity", "what this server did")
    d.arrow([(hx[0] + hw, 278), (hx[1] - 8, 278)], cls="thin")
    d.text(500, 340, "a separate path — activity is not a balance ledger", "s")
    return d


def account_pools():
    d = D("account-pools", 1000, 468, "Where faucet money comes from",
          "Mining pays every block reward to hidden Account 6's transparent balance; shielding a "
          "matured reward moves it to Account 6's Orchard balance, which is the pool every faucet "
          "payment spends from into accounts 1 to 5.")
    d.card(48, 80, 904, 48, "One fixed development seed derives all six accounts", kind="note")
    for y, t, s in [(160, "Mining a block", "zakurad.toml names the miner"),
                    (262, "Account 6 · transparent", "every coinbase reward"),
                    (364, "Account 6 · Orchard", "the faucet's spending pool")]:
        d.card(48, y, 300, 62, t, s)
    d.arrow([(198, 222), (198, 254)], "reward")
    d.arrow([(198, 324), (198, 356)], "shield at 100 confs")
    d.zone(392, 148, 560, 196, "ACCOUNTS 1–5 — THE ONLY ONES THE API RETURNS")
    ax = [412, 596, 780]
    d.card(ax[0], 184, 156, 58, "Account 1", "opens with 5 ZEC", kind="accent")
    for i, x in enumerate(ax[1:]):
        d.card(x, 184, 156, 58, f"Account {i + 2}", "transparent | Orchard")
    for i, x in enumerate(ax[:2]):
        d.card(x, 266, 156, 58, f"Account {i + 4}", "transparent | Orchard")
    d.arrow([(348, 395), (674, 395), (674, 352)], "faucet", lxy=(511, 385))
    d.text(500, 450, "Account 6 never appears in /api/v1/accounts or any displayed balance", "s")
    return d


def treasury_funding():
    d = D("treasury-funding", 1000, 392, "A faucet payment, and the detour when funds are short",
          "A faucet request proposes a payment from the treasury's Orchard pool. With enough "
          "spendable funds it broadcasts, records and confirms. Otherwise it mines for maturity, "
          "discovers and shields a reward, and retries the proposal.")
    xs, w = d.row(4)
    y, h = 92, 64
    for x, (t, s) in zip(xs, [("Faucet request", "account, pool, amount"),
                              ("Propose from treasury", "the SDK decides"),
                              ("Broadcast", "through lightwalletd"),
                              ("Record + confirm", "txid, then a block")]):
        d.card(x, y, w, h, t, s)
    d.arrow([(xs[0] + w, y + 32), (xs[1] - 8, y + 32)])
    d.arrow([(xs[1] + w, y + 32), (xs[2] - 8, y + 32)], "enough funds",
            lxy=((xs[1] + w + xs[2]) / 2, y - 12))
    d.arrow([(xs[2] + w, y + 32), (xs[3] - 8, y + 32)])
    bx, bw = d.row(3)
    for x, (t, s) in zip(bx, [("Mine for maturity", "100 confirmations"),
                              ("Discover the reward", "match the coinbase output"),
                              ("Shield it", "into the treasury's Orchard")]):
        d.card(x, 244, bw, h, t, s)
    d.arrow([(bx[0] + bw, 276), (bx[1] - 8, 276)])
    d.arrow([(bx[1] + bw, 276), (bx[2] - 8, 276)])
    d.arrow([(xs[1] + 60, y + h), (xs[1] + 60, 202), (bx[0] + bw / 2, 202), (bx[0] + bw / 2, 236)],
            "insufficient spendable Orchard funds", lxy=(360, 194))
    d.arrow([(bx[2] + bw / 2, 244), (bx[2] + bw / 2, 218), (xs[1] + w - 60, 218),
             (xs[1] + w - 60, y + h + 6)], "retry the proposal", lxy=(700, 210))
    d.text(500, 372, "so one faucet call can move the chain by more than one block", "s")
    return d


def send_choices():
    d = D("send-choices", 940, 372, "One send, two account + pool pairs",
          "A send picks a source account and pool on the left and a destination account and pool "
          "on the right, with the amount flowing between the pools. Three notes: transparent "
          "change returns as an Orchard note, the fee leaves the source on top of the amount, and "
          "a memo needs an Orchard destination.")
    d.text(48, 74, "SOURCE", "k", "start")
    d.text(668, 74, "DESTINATION", "k", "start")
    d.card(48, 88, 224, 56, "Source account", "one of 1–5")
    d.card(668, 88, 224, 56, "Destination account", "one of 1–5")
    d.arrow([(272, 116), (404, 116)], cls="thin", head=False)
    d.arrow([(536, 116), (668, 116)], cls="thin", head=False)
    d.text(470, 120, "must differ", "s")
    d.card(48, 174, 224, 56, "Source pool", "transparent · orchard")
    d.card(668, 174, 224, 56, "Destination pool", "orchard · transparent")
    d.arrow([(272, 202), (658, 202)])
    d.text(470, 190, "amount in zatoshi", "a")
    for x, t, s in [(48, "Transparent change", "returns as Orchard"),
                    (358, "Fee leaves the source", "/send/quote has the max"),
                    (668, "Memo", "Orchard destination only")]:
        d.card(x, 290, 224, 50, t, s, kind="note")
    d.arrow([(160, 230), (160, 290)], cls="lead", head=False)
    d.arrow([(470, 206), (470, 290)], cls="lead", head=False)
    d.arrow([(780, 230), (780, 290)], cls="lead", head=False)
    return d


def payment_flow():
    d = D("payment-flow", 1000, 420, "A payment, and the gap that makes a lost response ambiguous",
          "A send or faucet request is validated, then its idempotency key is looked up. A "
          "recorded key returns the stored activity. A new key is built and broadcast, then "
          "recorded, then confirmed. The broadcast happens before the record, so a crash in "
          "between leaves a live transaction with no row.")
    xs, w = d.row(4)
    y, h = 92, 64
    for x, (t, s), kind in zip(xs, [("POST /send", "or POST /faucet"),
                                    ("Validate", "accounts, pools, key"),
                                    ("Look up the key", "in tsz.db"),
                                    ("Return the stored row", "nothing is built")],
                               ["plain", "plain", "plain", "note"]):
        d.card(x, y, w, h, t, s, kind=kind)
    d.arrow([(xs[0] + w, y + 32), (xs[1] - 8, y + 32)])
    d.arrow([(xs[1] + w, y + 32), (xs[2] - 8, y + 32)])
    d.arrow([(xs[2] + w, y + 32), (xs[3] - 8, y + 32)], "recorded",
            lxy=((xs[2] + w + xs[3]) / 2, y - 12))
    bx, bw = d.row(3)
    d.arrow([(xs[2] + w / 2, y + h), (xs[2] + w / 2, 196), (bx[0] + bw / 2, 196),
             (bx[0] + bw / 2, 232)], "new key", lxy=(500, 188))
    for x, (t, s) in zip(bx, [("Build + broadcast", "through lightwalletd"),
                              ("Record in tsz.db", "txid + key, status broadcast"),
                              ("Confirm", "mine one block if needed")]):
        d.card(x, 240, bw, h, t, s)
    d.arrow([(bx[0] + bw, 272), (bx[1] - 8, 272)])
    d.arrow([(bx[1] + bw, 272), (bx[2] - 8, 272)])
    gap_x = (bx[0] + bw + bx[1]) / 2
    d.card(gap_x - 150, 330, 300, 50, "Ambiguous gap", "the broadcast happens first", kind="accent")
    d.arrow([(gap_x, 304), (gap_x, 330)], cls="lead", head=False)
    d.text(500, 400, "a missing activity row does not prove nothing was sent", "s")
    return d


def mining_flow():
    d = D("mining-flow", 1000, 308, "An explicit mine request",
          "ths mine N posts to the mine endpoint, which validates the count, asks Zakura to "
          "generate the blocks, waits for lightwalletd to index and the wallet to scan, then "
          "returns the block hashes. A failure after generate is not a rollback.")
    xs, w = d.row(5, gap=24)
    y, h = 96, 68
    for x, (t, s) in zip(xs, [("ths mine N", "or the Mine dialog"), ("Validate", "1 to 10,000"),
                              ("Zakura generates", "blocks + hashes"), ("Index + scan", "then reconcile"),
                              ("Return", "blocks and hashes")]):
        d.card(x, y, w, h, t, s, kind="accent" if t == "Zakura generates" else "plain")
    for a, b in zip(xs, xs[1:]):
        d.arrow([(a + w, y + h / 2), (b - 8, y + h / 2)])
    d.card(240, 218, 520, 54, "A failure after generate is not a rollback",
           "the blocks stay on the chain; the call returns an error", kind="note")
    d.arrow([(xs[2] + w / 2, y + h), (xs[2] + w / 2, 218)], cls="lead", head=False)
    return d


def wallet_sync():
    d = D("wallet-sync", 1000, 412, "How the wallet catches up with the node",
          "About every two seconds the server asks Zakura for its height and hash and compares "
          "them with the wallet's scanned checkpoint. When they match it stays ready; when either "
          "moved it waits for the index, rewinds if the stored hashes disagree, rescans and "
          "publishes. On error the last good snapshot stays visible.")
    xs, w = d.row(3)
    y, h = 92, 64
    for x, (t, s), kind in zip(xs, [("Ask Zakura for its tip", "height and hash, every 2s"),
                                    ("Compare with the wallet", "fully scanned checkpoint"),
                                    ("Stay ready", "nothing to do")],
                               ["plain", "plain", "note"]):
        d.card(x, y, w, h, t, s, kind=kind)
    d.arrow([(xs[0] + w, y + 32), (xs[1] - 8, y + 32)])
    d.arrow([(xs[1] + w, y + 32), (xs[2] - 8, y + 32)], "same pair",
            lxy=((xs[1] + w + xs[2]) / 2, y - 12))
    d.arrow([(xs[1] + w / 2, y + h), (xs[1] + w / 2, 196), (xs[0] + w / 2, 196),
             (xs[0] + w / 2, 232)], "height or hash moved", lxy=(400, 188))
    for x, (t, s) in zip(xs, [("Wait for the index", "lightwalletd reaches it"),
                              ("Rewind if hashes differ", "back to the last match"),
                              ("Rescan and publish", "then report ready")]):
        d.card(x, 240, w, h, t, s, kind="accent" if t == "Rescan and publish" else "plain")
    d.arrow([(xs[0] + w, 272), (xs[1] - 8, 272)])
    d.arrow([(xs[1] + w, 272), (xs[2] - 8, 272)])
    d.card(290, 330, 420, 50, "On error the last good snapshot stays",
           "state reports error until a scan succeeds", kind="note")
    d.arrow([(500, 304), (500, 330)], cls="lead", head=False)
    d.text(500, 400, "then the loop comes round again, about two seconds later", "s")
    return d


def retry_decisions():
    d = D("retry-decisions", 1000, 412, "What a lost response lets you conclude",
          "If the call was a send or faucet you chose the idempotency key, so look at the "
          "activity: a row with your key can be resent safely, while no row does not prove "
          "nothing was broadcast. Mine and address-faucet take no key, so compare the chain "
          "instead.")
    d.card(290, 80, 420, 54, "The response never arrived", "which call was it?", kind="accent")
    hx, hw = d.row(2, gap=32)
    d.arrow([(500, 134), (500, 158), (hx[0] + hw / 2, 158), (hx[0] + hw / 2, 178)])
    d.arrow([(500, 134), (500, 158), (hx[1] + hw / 2, 158), (hx[1] + hw / 2, 178)])
    d.card(hx[0], 186, hw, 60, "send or faucet", "you chose the idempotency key")
    d.card(hx[1], 186, hw, 60, "mine or faucet/address", "no key, nothing to replay")
    qx, qw = d.row(4)
    d.card(qx[0], 290, qw, 64, "Row with your key", "resend it — no second pay")
    d.card(qx[1], 290, qw, 64, "No row", "not proof nothing was sent", kind="note")
    d.card(hx[1], 290, hw, 64, "Compare the chain", "tip height, or the destination balance")
    for x in (qx[0] + qw / 2, qx[1] + qw / 2, hx[1] + hw / 2):
        d.arrow([(x, 246), (x, 282)], cls="lead", head=False)
    d.text(500, 392, "look at GET /api/v1/activity and the chain before calling again", "s")
    return d


def interface_selection():
    d = D("interface-selection", 1000, 368, "Pick the interface that owns the answer",
          "Wallet facts come from the dashboard HTTP API, node facts from Zakura's JSON-RPC, and "
          "compact blocks from lightwalletd's plaintext gRPC. Read the addresses from ths "
          "endpoints --json rather than hard-coding the ports.")
    d.card(290, 76, 420, 50, "Who owns the answer you need?", kind="accent")
    xs, w = d.row(3)
    pairs = [("Balances and activity", "accounts, sync state, faucet", "Dashboard HTTP API", "/api/v1/… on :32805"),
             ("Chain facts", "tip, blocks, mempool", "Zakura JSON-RPC", "on :18232"),
             ("Compact blocks", "for your own light client", "lightwalletd gRPC", ":9067, plaintext")]
    for x, (t, s, t2, s2) in zip(xs, pairs):
        d.card(x, 164, w, 62, t, s)
        d.card(x, 262, w, 62, t2, s2, kind="accent")
        d.arrow([(x + w / 2, 126), (x + w / 2, 156)], cls="lead", head=False)
        d.arrow([(x + w / 2, 226), (x + w / 2, 254)])
    d.text(500, 350, "read the real addresses each run from ths endpoints --json", "s")
    return d


def source_to_runtime():
    d = D("source-to-runtime", 1000, 440, "Source reaches a run only through an image",
          "The web and server sources build the app image, the lightwalletd Dockerfile builds the "
          "lightwalletd image, and Zakura is pinned and pulled. ths start refuses to run until all "
          "three exist. Editing source changes neither a running container nor an existing image.")
    xs, w = d.row(3)
    d.zone(48, 72, 904, 116, "SOURCE IN THIS REPOSITORY")
    for x, t, s in zip(xs, ["web/", "crates/tsz-server/", "docker/lightwalletd.Dockerfile"],
                       ["React dashboard", "HTTP API and wallet", None]):
        d.card(x, 104, w, 62, t, s)
    for x in (xs[0] + w / 2, xs[1] + w / 2, xs[2] + w / 2):
        d.arrow([(x, 166), (x, 236)])
    d.text(xs[0] + w / 2 + 70, 206, "ths build", "a")
    d.text(xs[2] + w / 2 + 70, 206, "ths build", "a")
    d.zone(48, 236, 904, 172, "IMAGES — THE ONLY THING A RUN USES")
    d.card(xs[0], 268, w * 2 + 28, 60, "app:&lt;version&gt;", "web/ and the server crate, together")
    d.card(xs[2], 268, w, 60, "lightwalletd:&lt;version&gt;", "from that Dockerfile")
    d.card(xs[1], 344, w, 52, "zakuracore/zakura:1.4.0", "pinned — ths pull, never built")
    d.text(500, 432, "editing source changes neither a running container nor an existing image", "s")
    return d


def diagnose_state():
    d = D("diagnose-state", 1000, 428, "Work down the layers; stop at the first one that is wrong",
          "Four triage steps: does the launcher run at all, is this instance up and where, is the "
          "node healthy but the wallet stale, and only then a payment you are unsure of.")
    xs, w = d.row(2, gap=32)
    rows = [("Does the launcher run?", "ths doctor · ths pull", "Docker daemon, then the images"),
            ("Is this instance up, and where?", "ths status · ths endpoints --json", "each name is its own instance"),
            ("Node healthy, balances stale?", "wallet_sync in /api/v1/status", "usually lightwalletd lagging behind"),
            ("Only then: a payment you doubt", "/api/v1/activity, then the chain", "only a keyed payment is safe to repeat")]
    y = 84
    for i, (t, s, hint) in enumerate(rows):
        d.card(xs[0], y, w, 62, t, s, kind="accent" if i == 3 else "plain")
        d.card(xs[1], y, w, 62, hint, None, kind="note")
        d.arrow([(xs[0] + w, y + 31), (xs[1] - 8, y + 31)], cls="thin")
        if i < 3:
            d.arrow([(xs[0] + 90, y + 62), (xs[0] + 90, y + 84)], cls="lead", head=False)
        y += 84
    return d


FIGURES = [instance_lifecycle, architecture_topology, state_ownership, account_pools,
           treasury_funding, send_choices, payment_flow, mining_flow, wallet_sync,
           retry_decisions, interface_selection, source_to_runtime, diagnose_state]

if __name__ == "__main__":
    problems = []
    for fn in FIGURES:
        d = fn()
        problems += d.save()
        print(f"wrote images/{d.name}.svg")
    for p in problems:
        print("warning:", p, file=sys.stderr)
    sys.exit(0)
