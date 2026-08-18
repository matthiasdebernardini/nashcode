#!/usr/bin/env python3
"""nashgit UAT harness. Implements the auto half of uat/UAT-TESTS.md.

Run from the repo root after `cargo build`:

    python3 uat/uat.py

Builds a throwaway fixture world in a temp dir, boots target/debug/nashgit on a free
port, runs every automatable acceptance check, prints a pass/fail table, exits 1 on
any failure. Stdlib only. Nothing touches a real remote.
"""

import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("NASHGIT_BIN", os.path.join(ROOT, "target", "debug", "nashgit"))

RESULTS = []  # (test_id, name, ok, detail)


def check(test_id, name, ok, detail=""):
    RESULTS.append((test_id, name, bool(ok), detail))
    mark = "ok " if ok else "FAIL"
    print(f"  [{mark}] {test_id} {name}" + (f" — {detail}" if detail and not ok else ""))


def wait_for(fn, timeout=60, interval=1.0, desc="condition"):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            value = fn()
            if value:
                return value
        except Exception:
            pass
        time.sleep(interval)
    raise TimeoutError(f"timed out waiting for {desc}")


# ---- git ---------------------------------------------------------------------------

GIT_ENV = {
    **os.environ,
    "GIT_AUTHOR_NAME": "ada",
    "GIT_AUTHOR_EMAIL": "ada@tailnet",
    "GIT_COMMITTER_NAME": "ada",
    "GIT_COMMITTER_EMAIL": "ada@tailnet",
}


def g(cwd, *args, env=None):
    result = subprocess.run(
        ["git", *args], cwd=cwd, env=env or GIT_ENV, capture_output=True, text=True
    )
    if result.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)} in {cwd}: {result.stderr}")
    return result.stdout.strip()


def write(path, content, mode=None):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(content)
    if mode:
        os.chmod(path, mode)


def tip(bare, branch):
    return g(bare, "rev-parse", f"refs/heads/{branch}")


# ---- http --------------------------------------------------------------------------

BASE = None  # set once the server is up


def http(method, path, body=None, headers=None, base=None):
    """Returns (status, bytes). Follows redirects (303 after form posts is fine)."""
    url = (base or BASE) + path
    data = None
    hdrs = dict(headers or {})
    if body is not None:
        data = json.dumps(body).encode()
        hdrs.setdefault("content-type", "application/json")
    req = urllib.request.Request(url, data=data, headers=hdrs, method=method)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return resp.status, resp.read()
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def get(path, **kw):
    return http("GET", path, **kw)


def jget(path, **kw):
    status, body = get(path, headers={"accept": "application/json"}, **kw)
    return status, json.loads(body)


MATTHIAS = {"Tailscale-User-Login": "matthias@tailnet", "Tailscale-User-Name": "Matthias"}
ROB = {"Tailscale-User-Login": "rob@tailnet", "Tailscale-User-Name": "Rob"}


# ---- webhook listener --------------------------------------------------------------

WEBHOOKS = []
WEBHOOK_LOCK = threading.Lock()


class WebhookHandler(BaseHTTPRequestHandler):
    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("content-length", 0)))
        try:
            payload = json.loads(raw)
        except Exception:
            payload = {"raw": raw.decode(errors="replace")}
        with WEBHOOK_LOCK:
            WEBHOOKS.append((self.path.lstrip("/"), payload))
        self.send_response(200)
        self.end_headers()

    def log_message(self, *args):
        pass


def webhook_events(kind):
    with WEBHOOK_LOCK:
        return [payload for path, payload in WEBHOOKS if path == kind]


# ---- fixtures ----------------------------------------------------------------------

RETRY_V0 = """import time

def fetch(url, attempts=1):
    \"\"\"Fetch a URL. No retries yet.\"\"\"
    return _do_fetch(url)

def _do_fetch(url):
    raise NotImplementedError
"""

RETRY_V1 = """import time

RETRIES = 2
BACKOFF = [0.1, 0.4]

def fetch(url, attempts=RETRIES + 1):
    \"\"\"Fetch a URL, retrying idempotent failures.\"\"\"
    for i in range(attempts):
        try:
            return _do_fetch(url)
        except IOError:
            if i == attempts - 1:
                raise
            time.sleep(BACKOFF[min(i, len(BACKOFF) - 1)])

def _do_fetch(url):
    raise NotImplementedError
"""

JITTER_V1 = """import random

def jittered(delay):
    \"\"\"Spread thundering herds: +/-25% around the base delay.\"\"\"
    return delay * random.uniform(0.75, 1.25)
"""

JITTER_V2 = """import random

JITTER_SPREAD = 0.25

def jittered(delay):
    \"\"\"Spread thundering herds around the base delay.\"\"\"
    return delay * random.uniform(1 - JITTER_SPREAD, 1 + JITTER_SPREAD)
"""


def build_fixtures(root):
    bare_dir = os.path.join(root, "git")
    os.makedirs(bare_dir)

    demo = os.path.join(root, "work", "demo")
    g(root, "init", "-q", "-b", "main", demo)
    write(f"{demo}/README.md", "# demo\n\nExercises nashgit review.\n")
    write(f"{demo}/src/retry.py", RETRY_V0)
    write(
        f"{demo}/.nashgit/ci",
        "#!/bin/sh\n"
        'echo "checking $NASHGIT_REPO@$NASHGIT_BRANCH ($NASHGIT_COMMIT)"\n'
        'python3 -m py_compile src/*.py && echo "all green"\n',
        mode=0o755,
    )
    write(
        f"{demo}/plans/retries.md",
        "---\nbranch: feat/retry-core\n---\n\n# Retry policy\n\n"
        "Retry idempotent requests with exponential backoff.\n\n"
        "- Two retries, then fail loudly.\n"
        "- Only GET and HEAD are idempotent enough to retry.\n\n"
        "Work is tracked on `tasks/retry.md`, shipping via `feat/retry-core`.\n",
    )
    write(
        f"{demo}/tasks/retry.md",
        "---\nstatus: doing\ntitle: Ship the retry policy\nassignee: ada\n"
        "branch: feat/retry-core\nplan: plans/retries.md\n---\n\n"
        "Implement the two-retry policy from `plans/retries.md`.\n",
    )
    write(
        f"{demo}/tasks/docs.md",
        "---\nstatus: todo\ntitle: Document the retry knobs\nassignee: builder-agent\n"
        "plan: plans/retries.md\n---\n\nREADME section on attempts and backoff.\n",
    )
    write(
        f"{demo}/tasks/setup.md",
        "---\nstatus: done\ntitle: Bootstrap the demo service\n---\n\n"
        "Repo skeleton, CI script, first fetch stub.\n",
    )
    write(f"{demo}/tasks/broken.md", "status todo\nno front matter fence here\n")
    write(
        f"{demo}/tasks/ghost.md",
        "---\nstatus: todo\ntitle: Card with a dangling ref\nplan: plans/nope.md\n---\n\n"
        "The plan ref above does not exist.\n",
    )
    g(demo, "add", "-A")
    g(demo, "commit", "-qm", "Bootstrap demo service with plan and cards")

    g(demo, "checkout", "-qb", "feat/retry-core")
    write(f"{demo}/src/retry.py", RETRY_V1)
    g(demo, "commit", "-aqm", "Retry core: two retries with fixed backoff")
    with open(f"{demo}/src/retry.py", "a") as f:
        f.write('\ndef is_idempotent(method):\n    return method in ("GET", "HEAD")\n')
    g(demo, "commit", "-aqm", "Gate retries on idempotent methods")

    g(demo, "checkout", "-qb", "feat/retry-jitter")
    write(f"{demo}/src/jitter.py", JITTER_V1)
    g(demo, "add", "-A")
    g(demo, "commit", "-qm", "Add jitter helper for backoff delays")

    g(demo, "checkout", "-q", "main")
    g(demo, "checkout", "-qb", "chore/bad-lint")
    write(f"{demo}/src/oops.py", "def broken(:\n    pass\n")
    g(demo, "add", "-A")
    g(demo, "commit", "-qm", "Introduce a syntax error (CI must go red)")

    g(demo, "checkout", "-q", "main")
    g(demo, "checkout", "-qb", "stackb/base")
    write(f"{demo}/base.txt", "one\n")
    g(demo, "add", "-A")
    g(demo, "commit", "-qm", "stackb base: one")
    g(demo, "checkout", "-qb", "stackb/child")
    write(f"{demo}/child.txt", "child work\n")
    g(demo, "add", "-A")
    g(demo, "commit", "-qm", "stackb child work")
    g(demo, "checkout", "-q", "stackb/base")
    g(demo, "checkout", "-qb", "stackb/conflict")
    write(f"{demo}/base.txt", "conflict-edit\n")
    g(demo, "commit", "-aqm", "stackb conflicting edit of base.txt")
    g(demo, "checkout", "-q", "main")
    g(root, "clone", "-q", "--bare", demo, os.path.join(bare_dir, "demo.git"))

    beta = os.path.join(root, "work", "beta")
    g(root, "init", "-q", "-b", "main", beta)
    write(f"{beta}/README.md", "# beta\n")
    write(f"{beta}/plans/api.md", "# API sketch\n\nOne endpoint, JSON in, JSON out.\n")
    write(f"{beta}/tasks/api.md", "---\nstatus: todo\ntitle: Sketch the API\n---\n\nSee plans/api.md.\n")
    g(beta, "add", "-A")
    g(beta, "commit", "-qm", "Seed beta")
    g(beta, "checkout", "-qb", "feat/endpoint")
    write(f"{beta}/api.py", "def handler(): ...\n")
    write(f"{beta}/plans/extra.md", "# Branch-only plan\n\nOnly exists on feat/endpoint.\n")
    g(beta, "add", "-A")
    g(beta, "commit", "-qm", "First endpoint stub with a branch-only plan")
    g(beta, "checkout", "-q", "main")
    g(root, "clone", "-q", "--bare", beta, os.path.join(bare_dir, "beta.git"))

    return bare_dir


# ---- server lifecycle --------------------------------------------------------------

def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def start_server(root, bare_dir, port, webhook_port, dgit_url=None, log_name="server.log"):
    env = {
        **os.environ,
        "DGIT_URL": dgit_url or bare_dir,
        "NASHGIT_REPOS": "demo,beta",
        "NASHGIT_MIRRORS": os.path.join(root, "mirrors"),
        "NASHGIT_BIND": f"127.0.0.1:{port}",
        "NASHGIT_WEBHOOKS": os.path.join(root, "webhooks.json"),
    }
    env.pop("ANTHROPIC_API_KEY", None)  # /brain/ask must 404 in this suite
    log = open(os.path.join(root, log_name), "ab")
    return subprocess.Popen([BIN, "serve"], env=env, stdout=log, stderr=log)


def push_and_wait(bare, branch, expect_sha, page):
    """Push happened in a clone; poll pages until the mirror shows the new tip."""
    def seen():
        status, body = get(page)
        return status == 200 and expect_sha[:7].encode() in body
    wait_for(seen, timeout=45, interval=2, desc=f"mirror to see {branch}@{expect_sha[:7]}")


# ---- test phases -------------------------------------------------------------------

def t1_pages(bare):
    print("T1 pages")
    pages = ["/", "/demo", "/demo/stacks", "/demo/plans", "/demo/board", "/demo/ci",
             "/demo/feat/retry-core", "/demo/traces", "/beta"]
    for page in pages:
        status, _ = get(page)
        check("T1", f"GET {page} is 200", status == 200, f"got {status}")
    _, body = get("/")
    check("T1", "index names both repos", b"demo" in body and b"beta" in body)
    _, body = get("/demo")
    check("T1", "branch list shows stack parent and ahead count",
          b"feat/retry-jitter" in body and b"feat/retry-core" in body and b"Counter" in body)
    check("T1", "branch list carries CI status per row", b"CI:" in body)


def t2_stacks():
    print("T2 stack inference")
    _, brain = jget("/brain")
    demo = next(r for r in brain["repos"] if r["name"] == "demo")
    parents = {b["branch"]: b.get("parent") for b in demo["branches"]}
    ahead = {b["branch"]: b.get("ahead") for b in demo["branches"]}
    check("T2", "retry-core parents to main", parents.get("feat/retry-core") == "main", str(parents))
    check("T2", "retry-jitter parents to retry-core",
          parents.get("feat/retry-jitter") == "feat/retry-core", str(parents))
    check("T2", "bad-lint falls back to main", parents.get("chore/bad-lint") == "main")
    check("T2", "stackb/child parents to stackb/base", parents.get("stackb/child") == "stackb/base")
    check("T2", "ahead counts", ahead.get("feat/retry-core") == 2 and ahead.get("feat/retry-jitter") == 1,
          str(ahead))


def t3_review():
    print("T3 review page")
    _, body = get("/demo/feat/retry-core")
    text = body.decode(errors="replace")
    check("T3", "unique commits listed",
          "Retry core: two retries" in text and "Gate retries on idempotent" in text)
    check("T3", "parent's other work absent", "Bootstrap demo service" not in text.split("Retry core")[0]
          or "Bootstrap demo service" not in text)
    check("T3", "diff covers src/retry.py", "src/retry.py" in text)
    check("T3", "stack banner links parent and child",
          'href="/demo/main"' in text and "feat/retry-jitter" in text)
    check("T3", "back-links show the card and plan",
          "tasks/retry.md" in text and "plans/retries.md" in text)
    status, _ = get("/demo/tasks/retry.md")
    check("T3", "reserved word tasks/ renders the card page, not a branch", status == 200)
    status, _ = get("/demo/feat/retry-jitter")
    check("T3", "slash-named branch resolves", status == 200)
    status, _ = get("/demo/branch-that-does-not-exist")
    check("T3", "unknown branch is a 4xx", status in (400, 404), f"got {status}")
    status, _ = get("/norepo")
    check("T3", "unknown repo is 404", status == 404, f"got {status}")


def initial_ci():
    def done():
        _, brain = jget("/brain")
        demo = next(r for r in brain["repos"] if r["name"] == "demo")
        runs = {a["branch"]: a["status"] for a in demo["activity"] if a["type"] == "ci_run"}
        if runs.get("chore/bad-lint") == "failed" and runs.get("feat/retry-core") == "passed":
            return runs
        return None
    return wait_for(done, timeout=180, interval=3, desc="initial CI runs")


def t4_ci(runs):
    print("T4 CI")
    check("T4", "red branch failed, green branch passed",
          runs.get("chore/bad-lint") == "failed" and runs.get("feat/retry-core") == "passed", str(runs))
    _, brain = jget("/brain")
    beta = next(r for r in brain["repos"] if r["name"] == "beta")
    beta_runs = [a for a in beta["activity"] if a["type"] == "ci_run"]
    check("T4", "repo without .nashgit/ci never executes a job",
          beta_runs and all(a["status"] == "skipped" for a in beta_runs), str(beta_runs))
    _, body = get("/demo/feat/retry-core/ci")
    check("T4", "green log shows script output", b"all green" in body)
    _, body = get("/demo/chore/bad-lint/ci")
    check("T4", "red log shows the compile error", b"SyntaxError" in body or b"error" in body.lower())

    _, brain = jget("/brain")
    demo = next(r for r in brain["repos"] if r["name"] == "demo")
    before = len([a for a in demo["activity"]
                  if a["type"] == "ci_run" and a["branch"] == "chore/bad-lint"])
    status, body = http("POST", "/demo/chore/bad-lint/ci/rerun", body={}, headers=MATTHIAS)
    check("T4", "rerun accepted", status == 200 and b"ok" in body, f"{status} {body[:80]}")

    def reran():
        _, brain = jget("/brain")
        demo = next(r for r in brain["repos"] if r["name"] == "demo")
        now = len([a for a in demo["activity"]
                   if a["type"] == "ci_run" and a["branch"] == "chore/bad-lint"])
        return now > before
    try:
        wait_for(reran, timeout=90, interval=3, desc="rerun to finish")
        check("T4", "rerun recorded a new run for the same tip", True)
    except TimeoutError as e:
        check("T4", "rerun recorded a new run for the same tip", False, str(e))


def t5_raw(bare):
    print("T5 raw files")
    status, body = get("/demo/raw/main/plans/retries.md")
    expected = subprocess.run(
        ["git", "-C", bare + "/demo.git", "show", "main:plans/retries.md"],
        capture_output=True).stdout
    check("T5", "raw bytes identical to git show", status == 200 and body == expected)
    status, body = get("/demo/raw/feat/retry-jitter/src/jitter.py")
    expected = subprocess.run(
        ["git", "-C", bare + "/demo.git", "show", "feat/retry-jitter:src/jitter.py"],
        capture_output=True).stdout
    check("T5", "raw works for slash branches", status == 200 and body == expected)


def t6_comments():
    print("T6 comments API")
    status, body = http("POST", "/demo/comments", body={
        "branch": "main", "file": "plans/retries.md", "line": 6,
        "body": "uat-c1: two retries is right"}, headers=MATTHIAS)
    c1 = json.loads(body)
    check("T6", "POST returns 201 with id", status == 201 and isinstance(c1.get("id"), int),
          f"{status} {body[:120]}")
    check("T6", "author from Tailscale header", c1.get("author") == "matthias@tailnet", str(c1))

    time.sleep(0.05)
    status, body = http("POST", "/demo/comments", body={
        "branch": "main", "file": "plans/retries.md", "line": 8,
        "body": "uat-c2: explicit author wins", "author": "planner-agent"})
    c2 = json.loads(body)
    check("T6", "explicit author kept", c2.get("author") == "planner-agent", str(c2))

    time.sleep(0.05)
    status, body = http("POST", "/demo/comments", body={
        "branch": "main", "file": "plans/retries.md", "body": "uat-c3: file-level, no line"})
    c3 = json.loads(body)
    check("T6", "no header, no author falls back to local", c3.get("author") == "local", str(c3))

    status, body = http("POST", "/demo/comments", body={
        "branch": "main", "body": "uat-c4: branch-level"})
    check("T6", "branch-level comment accepted", status == 201, f"{status} {body[:80]}")

    status, body = http("POST", "/demo/comments", body={"branch": "main", "line": 3, "body": "x"})
    check("T6", "line without file rejected", status == 400, f"got {status}")
    status, body = http("POST", "/demo/comments", body={"branch": "no-such", "body": "x"})
    check("T6", "unknown branch rejected", status == 400, f"got {status}")

    _, rows = jget("/demo/comments?file=plans/retries.md")
    ids = [r["id"] for r in rows]
    stamps = [r["created_at"] for r in rows]
    check("T6", "ordered by created_at then id", stamps == sorted(stamps) and ids == sorted(ids),
          str(list(zip(ids, stamps))))
    _, newer = jget(f"/demo/comments?file=plans/retries.md&since={rows[1]['created_at']}")
    check("T6", "since cursor: strictly newer, no repeats",
          [r["id"] for r in newer] == [r["id"] for r in rows[2:]],
          f"cursor gave {[r['id'] for r in newer]}, wanted {[r['id'] for r in rows[2:]]}")

    _, body = get("/demo/plans/retries.md")
    check("T6", "plan page renders its file-anchored thread", b"uat-c1" in body)

    status, _ = http("POST", f"/demo/comments/{c1['id']}/delete", body={}, headers=ROB)
    _, rows = jget("/demo/comments?file=plans/retries.md")
    still_there = any(r["id"] == c1["id"] for r in rows)
    check("T6", "cannot delete someone else's comment", still_there)
    http("POST", f"/demo/comments/{c1['id']}/delete", body={}, headers=MATTHIAS)
    _, rows = jget("/demo/comments?file=plans/retries.md")
    check("T6", "author deletes own comment", not any(r["id"] == c1["id"] for r in rows))


def t7_outdated(root, bare):
    print("T7 outdated comments")
    status, body = http("POST", "/demo/comments", body={
        "branch": "feat/retry-jitter", "file": "src/jitter.py", "line": 3,
        "body": "uat-old-anchor: name this constant"}, headers=MATTHIAS)
    check("T7", "line comment on the branch accepted", status == 201, f"{status}")

    clone = os.path.join(root, "work", "outdated-clone")
    g(root, "clone", "-q", os.path.join(bare, "demo.git"), clone)
    g(clone, "checkout", "-q", "feat/retry-jitter")
    write(f"{clone}/src/jitter.py", JITTER_V2)
    g(clone, "commit", "-aqm", "Name the jitter spread constant")
    g(clone, "push", "-q", "origin", "feat/retry-jitter")
    new_tip = tip(os.path.join(bare, "demo.git"), "feat/retry-jitter")
    push_and_wait(bare, "feat/retry-jitter", new_tip, "/demo/feat/retry-jitter")

    status, body = http("POST", "/demo/comments", body={
        "branch": "feat/retry-jitter", "file": "src/jitter.py", "line": 3,
        "body": "uat-fresh-anchor: better"}, headers=MATTHIAS)

    _, body = get("/demo/feat/retry-jitter")
    text = body.decode(errors="replace")
    out_at = text.lower().find("outdated")
    old_at = text.find("uat-old-anchor")
    fresh_at = text.find("uat-fresh-anchor")
    check("T7", "outdated section exists after the file moved", out_at >= 0)
    check("T7", "moved-file comment is in the outdated section", old_at > out_at > 0,
          f"outdated@{out_at} old@{old_at}")
    check("T7", "fresh-tip comment stays inline", 0 <= fresh_at < out_at,
          f"outdated@{out_at} fresh@{fresh_at}")


def t8_board(bare):
    print("T8 board")
    _, body = get("/demo/board")
    text = body.decode(errors="replace")
    check("T8", "canonical columns in order",
          -1 < text.find("todo") < text.find("doing") < text.find("done"))
    check("T8", "malformed card lands in needs attention",
          "needs-attention" in text or "needs attention" in text.lower())
    check("T8", "broken card did not break the board", "broken.md" in text)

    demo_bare = os.path.join(bare, "demo.git")
    before_tip = tip(demo_bare, "main")
    before_raw = get("/demo/raw/main/tasks/docs.md")[1]
    status, body = http("POST", "/demo/board/move",
                        body={"file": "tasks/docs.md", "status": "doing"}, headers=MATTHIAS)
    moved = json.loads(body)
    check("T8", "move reports ok with the commit", status == 200 and moved.get("ok"), f"{status} {body[:120]}")
    after_tip = tip(demo_bare, "main")
    check("T8", "exactly one new commit on main", g(demo_bare, "rev-parse", f"{after_tip}~1") == before_tip)
    author = g(demo_bare, "log", "-1", "--format=%an|%ae", "main")
    check("T8", "commit authored as the Tailscale user", "Matthias" in author and "matthias@tailnet" in author,
          author)
    after_raw = get("/demo/raw/main/tasks/docs.md")[1]
    check("T8", "only the status line changed",
          after_raw == before_raw.replace(b"status: todo", b"status: doing"),
          f"before={before_raw!r} after={after_raw!r}")

    status, _ = http("POST", "/demo/board/move", body={"file": "plans/retries.md", "status": "done"})
    check("T8", "file outside tasks/ rejected", status == 400, f"got {status}")
    status, _ = http("POST", "/demo/board/move", body={"file": "tasks/docs.md", "status": "Bad Status!"})
    check("T8", "invalid status rejected", status == 400, f"got {status}")
    check("T8", "rejected moves made no commit", tip(demo_bare, "main") == after_tip)
    return after_tip


def t9_links():
    print("T9 links")
    _, body = get("/demo/plans/retries.md")
    text = body.decode(errors="replace")
    check("T9", "plan lists the cards that reference it",
          "tasks/retry.md" in text and "tasks/docs.md" in text)
    check("T9", "plan shows its branch with CI status",
          "feat/retry-core" in text and "CI" in text)
    check("T9", "backticked branch token links to the branch page",
          'href="/demo/feat/retry-core"' in text)
    status, body = get("/demo/tasks/ghost.md")
    text = body.decode(errors="replace")
    check("T9", "dangling ref renders with a missing marker, page still 200",
          status == 200 and "missing" in text and "plans/nope.md" in text)


def t10_merge(bare):
    print("T10 merge")
    demo_bare = os.path.join(bare, "demo.git")

    status, body = http("POST", "/demo/chore/bad-lint/merge", body={}, headers=MATTHIAS)
    check("T10", "red CI blocks the merge with 409", status == 409 and b"blocked" in body.lower(),
          f"{status} {body[:120]}")
    check("T10", "blocked merge moved nothing",
          g(demo_bare, "branch", "--list", "chore/bad-lint") != "")

    before_main = tip(demo_bare, "main")
    branch_tip = tip(demo_bare, "feat/retry-core")
    status, body = http("POST", "/demo/feat/retry-core/merge", body={}, headers=MATTHIAS)
    check("T10", "green merge accepted", status == 200, f"{status} {body[:160]}")
    after_main = tip(demo_bare, "main")
    check("T10", "main moved", after_main != before_main)
    parents = g(demo_bare, "log", "-1", "--format=%P", "main").split()
    # The card flip lands after the merge, so the merge commit may not be the tip.
    merge_commit = g(demo_bare, "rev-list", "--merges", "-1", "main")
    check("T10", "no-ff merge commit created (parent had moved)", merge_commit != "", str(parents))
    contains = g(demo_bare, "branch", "--contains", branch_tip)
    check("T10", "branch commits are on main", "main" in contains, contains)

    card = get("/demo/raw/main/tasks/retry.md")[1]
    check("T10", "card with branch: flipped to done", b"status: done" in card, card[:120].decode())
    flip = g(demo_bare, "log", "-1", "--format=%s|%an|%ae", "main", "--", "tasks/retry.md")
    check("T10", "flip is a separate commit authored by the merger",
          "Matthias" in flip and "matthias@tailnet" in flip, flip)

    _, body = get("/demo/stacks")
    text = body.decode(errors="replace")
    check("T10", "audit log records the merge with identity",
          "merge" in text.lower() and "matthias@tailnet" in text)

    try:
        wait_for(lambda: webhook_events("merged"), timeout=15, desc="merged webhook")
        check("T10", "merged webhook fired", True)
    except TimeoutError as e:
        check("T10", "merged webhook fired", False, str(e))


def t11_restack(root, bare):
    print("T11 restack")
    demo_bare = os.path.join(bare, "demo.git")

    # Conflict: main gains a base.txt whose content fights the one stackb/base adds,
    # so rebasing the stack onto the new main hits an add/add conflict.
    clone = os.path.join(root, "work", "restack-clone")
    g(root, "clone", "-q", demo_bare, clone)
    write(f"{clone}/base.txt", "mainline\n")
    g(clone, "add", "-A")
    g(clone, "commit", "-qm", "Main adds base.txt (conflicts with stackb)")
    g(clone, "push", "-q", "origin", "main")

    base_before = tip(demo_bare, "stackb/base")
    child_before = tip(demo_bare, "stackb/child")
    conflict_before = tip(demo_bare, "stackb/conflict")
    status, body = http("POST", "/demo/main/restack", body={}, headers=MATTHIAS)
    check("T11", "conflicted restack answers 409 naming the file",
          status == 409 and b"base.txt" in body, f"{status} {body[:160]}")
    check("T11", "atomic abort: nothing force-pushed",
          tip(demo_bare, "stackb/base") == base_before
          and tip(demo_bare, "stackb/child") == child_before
          and tip(demo_bare, "stackb/conflict") == conflict_before)

    status, body = http("POST", "/demo/stackb/conflict/delete", body={}, headers=MATTHIAS)
    check("T11", "branch deleted from the UI route", status == 200, f"{status} {body[:80]}")
    check("T11", "branch gone from the remote",
          g(demo_bare, "branch", "--list", "stackb/conflict") == "")

    # Unblock: make main's base.txt byte-identical to stackb's, so the replay is clean.
    write(f"{clone}/base.txt", "one\n")
    g(clone, "commit", "-aqm", "Align base.txt with stackb (unblocks the restack)")
    g(clone, "push", "-q", "origin", "main")

    status, body = http("POST", "/demo/main/restack", body={}, headers=MATTHIAS)
    check("T11", "clean restack accepted", status == 200, f"{status} {body[:160]}")
    main_tip = tip(demo_bare, "main")
    child_after = tip(demo_bare, "stackb/child")
    ancestor = subprocess.run(
        ["git", "-C", demo_bare, "merge-base", "--is-ancestor", main_tip, child_after]).returncode
    check("T11", "descendants rebased onto the new main tip",
          child_after != child_before and ancestor == 0,
          f"child {child_before[:7]} -> {child_after[:7]}")

    _, body = get("/demo/stacks")
    check("T11", "audit log records the restack", b"restack" in body.lower())
    try:
        wait_for(lambda: webhook_events("restacked"), timeout=15, desc="restacked webhook")
        check("T11", "restacked webhook fired", True)
    except TimeoutError as e:
        check("T11", "restacked webhook fired", False, str(e))


def t12_traces(root, bare):
    print("T12 traces")
    batch = {"session": "uat-idem", "agent": "uat",
             "events": [{"seq": 1, "kind": "note", "payload": {"text": "once"}}]}
    _, first = http("POST", "/demo/traces/events", body=batch)
    _, second = http("POST", "/demo/traces/events", body=batch)
    first, second = json.loads(first), json.loads(second)
    check("T12", "event batch idempotent on (session, seq)",
          first.get("stored") == 1 and second.get("stored") == 0 and second.get("duplicates") == 1,
          f"first={first} second={second}")

    clone = os.path.join(root, "work", "agent-clone")
    g(root, "clone", "-q", os.path.join(bare, "demo.git"), clone)
    g(clone, "checkout", "-q", "-b", "feat/agent-work", "origin/main")
    env = {**GIT_ENV, "NASHGIT_URL": BASE, "NASHGIT_REPO": "demo"}

    def hook(payload, env_override=None):
        return subprocess.run([BIN, "hook"], input=json.dumps(payload), text=True,
                              env=env_override or env, cwd=clone).returncode

    session = "uat-agent-session"
    rc1 = hook({"session_id": session, "hook_event_name": "UserPromptSubmit", "cwd": clone,
                "prompt": "count how many fetches needed a retry"})
    write(f"{clone}/src/metrics.py", "RETRIED = 0\n\ndef count_retry():\n    global RETRIED\n    RETRIED += 1\n")
    g(clone, "add", "-A")
    g(clone, "commit", "-qm", "Add retry counter metric")
    rc2 = hook({"session_id": session, "hook_event_name": "PostToolUse", "cwd": clone,
                "tool_name": "Edit", "tool_response": {"success": True}})
    write(f"{clone}/src/metrics.py", "RETRIED = 0\n\ndef count_retry():\n    \"\"\"Graphed by ops.\"\"\"\n    global RETRIED\n    RETRIED += 1\n")
    g(clone, "commit", "-aqm", "Document the retry counter")
    rc3 = hook({"session_id": session, "hook_event_name": "Stop", "cwd": clone})
    check("T12", "hook exits 0 on every event", rc1 == rc2 == rc3 == 0, f"{rc1} {rc2} {rc3}")
    g(clone, "push", "-q", "origin", "feat/agent-work")
    sha1 = g(clone, "rev-parse", "HEAD~1")
    sha2 = g(clone, "rev-parse", "HEAD")

    _, sessions = jget("/demo/traces")
    mine = next((s for s in sessions if s["session"] == session), None)
    check("T12", "session lists both commits made between its events",
          mine is not None and mine.get("commits") == 2, str(mine))
    status, linked = jget(f"/demo/commits/{sha2}/trace")
    check("T12", "commit maps back to its session",
          status == 200 and session in json.dumps(linked), f"{status} {linked}")
    status, body = get(f"/demo/traces/{session}")
    check("T12", "session page renders the prompt",
          status == 200 and b"count how many fetches" in body)

    transcript = os.path.join(root, "transcript.jsonl")
    lines = ('{"type":"user","sessionId":"uat-backfill","text":"do the thing"}\n'
             '{"type":"assistant","sessionId":"uat-backfill","text":"done"}\n')
    write(transcript, lines)
    rc = subprocess.run([BIN, "trace", "push", transcript, "--repo", "demo"],
                        env=env, capture_output=True, text=True)
    check("T12", "trace push backfills a transcript", rc.returncode == 0, rc.stderr)
    status, body = get("/demo/traces/uat-backfill/transcript")
    check("T12", "transcript round-trips verbatim", status == 200 and body.decode() == lines)

    out = subprocess.run([BIN, "trace", "list", "--repo", "demo"], env=env,
                         capture_output=True, text=True).stdout
    check("T12", "trace list prints the sessions", session in out and "uat-backfill" in out, out)
    out = subprocess.run([BIN, "trace", "show", session, "--repo", "demo"], env=env,
                         capture_output=True, text=True).stdout
    check("T12", "trace show prints events and commits",
          "UserPromptSubmit" in out and sha2 in out, out)

    rc = subprocess.run([BIN, "hook"], input="not json at all", text=True, env=env).returncode
    check("T12", "hook exits 0 on garbage stdin", rc == 0, str(rc))
    dead = {**env, "NASHGIT_URL": "http://127.0.0.1:9"}
    rc = hook({"session_id": "x", "hook_event_name": "Stop", "cwd": clone}, env_override=dead)
    check("T12", "hook exits 0 with the server unreachable", rc == 0, str(rc))


def t13_brain():
    print("T13 brain")
    status, brain = jget("/brain")
    names = {r["name"] for r in brain["repos"]}
    check("T13", "aggregate covers both repos", names == {"demo", "beta"}, str(names))
    demo = next(r for r in brain["repos"] if r["name"] == "demo")
    check("T13", "aggregate carries branches, plans, cards, activity",
          all(k in demo for k in ("branches", "plans", "cards", "activity")), str(list(demo)))
    _, only = jget("/brain?repo=beta")
    check("T13", "?repo= filters", [r["name"] for r in only["repos"]] == ["beta"])
    _, future = jget("/brain?since=2030-01-01T00:00:00Z")
    empty = all(not r["activity"] for r in future["repos"])
    check("T13", "future ?since= empties activity without erroring", empty)
    status, body = http("POST", "/brain/ask", body={"question": "anything"})
    check("T13", "/brain/ask is 404 without ANTHROPIC_API_KEY", status == 404, f"got {status}")

    status, body = get("/beta/plans?branch=feat/endpoint")
    check("T13", "plans ?branch= shows branch-only plans", b"extra.md" in body)
    status, body = get("/beta/plans")
    check("T13", "default plans list omits branch-only plans", b"extra.md" not in body)


def t14_webhooks():
    print("T14 webhooks")
    pushes = webhook_events("push")
    check("T14", "push webhook fired with repo/branch/commit",
          any({"repo", "branch", "commit"} <= set(p) for p in pushes), str(pushes[:1]))
    finished = webhook_events("ci")
    check("T14", "ci_finished webhook fired with a status",
          any("status" in p for p in finished), str(finished[:1]))
    check("T14", "merged webhook seen", bool(webhook_events("merged")))
    check("T14", "restacked webhook seen", bool(webhook_events("restacked")))


def t15_degradation(root, bare, port, webhook_port, proc):
    print("T15 degradation")
    proc.terminate()
    proc.wait(timeout=10)
    dead = start_server(root, bare, port, webhook_port,
                        dgit_url=os.path.join(root, "no-such-dgit"), log_name="server-dead.log")
    try:
        wait_for(lambda: get("/")[0] == 200, timeout=20, desc="degraded server up")
        for page in ["/", "/demo", "/demo/stacks", "/demo/board", "/demo/plans", "/demo/ci"]:
            status, _ = get(page)
            check("T15", f"degraded GET {page} still 200", status == 200, f"got {status}")
        _, body = get("/demo")
        check("T15", "stale banner shown", b"Showing the last mirrored state" in body)
        status, _ = get("/norepo")
        check("T15", "unknown repo still 404, never 500", status == 404, f"got {status}")

        rc = subprocess.run([BIN, "doctor"], env={**os.environ, "NASHGIT_URL": BASE},
                            capture_output=True, text=True)
        check("T16", "doctor exits 0 against a reachable server",
              rc.returncode == 0 and "reachable" in rc.stdout, rc.stdout)
        rc = subprocess.run([BIN, "doctor"], env={**os.environ, "NASHGIT_URL": "http://127.0.0.1:9"},
                            capture_output=True, text=True)
        check("T16", "doctor exits nonzero when the server is down", rc.returncode != 0)
    finally:
        dead.terminate()
        dead.wait(timeout=10)


# ---- main --------------------------------------------------------------------------

def main():
    global BASE
    if not os.path.exists(BIN):
        sys.exit(f"binary not found at {BIN}; run `cargo build` first (or set NASHGIT_BIN)")

    root = tempfile.mkdtemp(prefix="nashgit-uat-")
    print(f"fixture world: {root}")
    bare = build_fixtures(root)

    webhook_port = free_port()
    listener = HTTPServer(("127.0.0.1", webhook_port), WebhookHandler)
    threading.Thread(target=listener.serve_forever, daemon=True).start()
    write(os.path.join(root, "webhooks.json"), json.dumps({
        "push": f"http://127.0.0.1:{webhook_port}/push",
        "ci_finished": f"http://127.0.0.1:{webhook_port}/ci",
        "merged": f"http://127.0.0.1:{webhook_port}/merged",
        "restacked": f"http://127.0.0.1:{webhook_port}/restacked",
    }))

    port = free_port()
    BASE = f"http://127.0.0.1:{port}"
    proc = start_server(root, bare, port, webhook_port)
    try:
        wait_for(lambda: get("/")[0] == 200, timeout=30, desc="server up")
        runs = initial_ci()

        t1_pages(bare)
        t2_stacks()
        t3_review()
        t5_raw(bare)
        t6_comments()
        t7_outdated(root, bare)
        t8_board(bare)
        t9_links()
        t4_ci(runs)
        t10_merge(bare)
        t11_restack(root, bare)
        t12_traces(root, bare)
        t13_brain()
        t14_webhooks()
        t15_degradation(root, bare, port, webhook_port, proc)
    finally:
        if proc.poll() is None:
            proc.terminate()
            proc.wait(timeout=10)
        listener.shutdown()

    failed = [r for r in RESULTS if not r[2]]
    print(f"\n{len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed")
    for test_id, name, _, detail in failed:
        print(f"  FAIL {test_id} {name} — {detail}")
    print(f"server logs: {root}/server.log")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
