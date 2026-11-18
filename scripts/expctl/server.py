#!/usr/bin/env python3
# Copyright 2026 The MOQtail Authors
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.

"""expctl: queue, start, stop and resume experiment.sh runs over one HTTP JSON API.

Every run lives in results/<id>/. Its controller state is in results/<id>/.expctl/
(state.json, run.log, exit_code), so the service can restart at any time and pick
up where it was: nothing is kept only in memory. One run executes at a time; the
rest wait in the queue.

Environment: EXPCTL_TOKEN (required), EXPCTL_PORT (default 8080),
EXPCTL_ROOT (default: the repo this file is in).
"""

import json
import os
import re
import secrets
import signal
import subprocess
import threading
import time
from datetime import datetime
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

HERE = Path(__file__).resolve().parent
ROOT = Path(os.environ.get("EXPCTL_ROOT", HERE.parents[1]))
RESULTS = ROOT / "results"
TOKEN = os.environ.get("EXPCTL_TOKEN", "")
PORT = int(os.environ.get("EXPCTL_PORT", "8080"))

ID_RE = re.compile(r"^[A-Za-z0-9_.-]+$")
FINISHED = {"done", "failed", "stopped", "interrupted"}
# Flags expctl sets itself; a run's args must not override them.
RESERVED_ARGS = {"--output", "--resume", "--help", "-h"}

lock = threading.Lock()
children = {}  # run id -> Popen, for runs this process started (so they get reaped)


def now():
    return datetime.now().isoformat(timespec="seconds")


def ctl_dir(run_id):
    return RESULTS / run_id / ".expctl"


def load(run_id):
    path = ctl_dir(run_id) / "state.json"
    return json.loads(path.read_text()) if path.is_file() else None


def save(state):
    d = ctl_dir(state["id"])
    d.mkdir(parents=True, exist_ok=True)
    tmp = d / "state.json.tmp"
    tmp.write_text(json.dumps(state, indent=2) + "\n")
    tmp.replace(d / "state.json")


def all_runs():
    runs = [load(p.parent.parent.name) for p in RESULTS.glob("*/.expctl/state.json")]
    return sorted((r for r in runs if r), key=lambda r: r["created"])


def progress(run_id):
    """Done/total from experiment.sh's own log lines; results counted from disk."""
    log = ctl_dir(run_id) / "run.log"
    text = log.read_text(errors="replace") if log.is_file() else ""
    total = re.findall(r"Total runs:\s+(\d+)", text)
    results = [
        p.name
        for p in (RESULTS / run_id).glob("*.json")
        if p.name != "experiment-metadata.json"
    ]
    scenario = re.findall(r"--- (\S+ rep \d+) ---", text)
    return {
        "results": len(results),
        "total": int(total[-1]) if total else None,
        "current": scenario[-1] if scenario else None,
    }


def view(state):
    return {**state, "progress": progress(state["id"])}


def git(*args):
    r = subprocess.run(["git", *args], cwd=ROOT, capture_output=True, text=True)
    return r.stdout.strip()


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def start(state):
    d = ctl_dir(state["id"])
    d.mkdir(parents=True, exist_ok=True)
    (d / "exit_code").unlink(missing_ok=True)
    args = [*state["args"], "--output", str(RESULTS / state["id"])]
    if state.get("resume"):
        args.append("--resume")
    # The wrapper records the exit code on disk, so the outcome is known even when
    # the service restarts mid-run and is no longer this process's parent.
    wrapper = 'bash scripts/experiment.sh "$@"; echo $? > "$EXPCTL_DIR/exit_code"'
    with open(d / "run.log", "a") as log:
        log.write(f"\n===== expctl {'resume' if state.get('resume') else 'start'} {now()} =====\n")
        log.flush()
        proc = subprocess.Popen(
            ["bash", "-c", wrapper, "expctl", *args],
            cwd=ROOT,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            env={**os.environ, "EXPCTL_DIR": str(d)},
        )
    children[state["id"]] = proc
    state.update(
        status="running",
        pid=proc.pid,
        started=now(),
        finished=None,
        exit_code=None,
        commit=git("rev-parse", "--short", "HEAD"),
        dirty=bool(git("status", "--porcelain", "--untracked-files=no")),
    )
    save(state)


def finish_if_exited(state):
    proc = children.get(state["id"])
    running = proc.poll() is None if proc else alive(state["pid"])
    if running:
        return False
    children.pop(state["id"], None)
    code_file = ctl_dir(state["id"]) / "exit_code"
    code = code_file.read_text().strip() if code_file.is_file() else None
    state["exit_code"] = int(code) if code and code.lstrip("-").isdigit() else None
    if state["status"] == "stopping":
        state["status"] = "stopped"
    elif state["exit_code"] == 0:
        state["status"] = "done"
    elif state["exit_code"] is None:
        state["status"] = "interrupted"  # killed without recording an exit code
    else:
        state["status"] = "failed"
    state["finished"] = now()
    save(state)
    return True


def tick():
    with lock:
        runs = all_runs()
        active = [r for r in runs if r["status"] in ("running", "stopping")]
        for r in active:
            finish_if_exited(r)
        if any(r["status"] in ("running", "stopping") for r in active):
            return
        queued = [r for r in runs if r["status"] == "queued"]
        if queued:
            start(queued[0])


def scheduler():
    while True:
        try:
            tick()
        except Exception as e:  # keep scheduling; the error shows in the service log
            print(f"scheduler: {e!r}", flush=True)
        time.sleep(2)


def create(name, args):
    if not isinstance(args, list) or not all(isinstance(a, str) for a in args):
        raise ValueError("args must be a list of strings")
    bad = RESERVED_ARGS.intersection(args)
    if bad:
        raise ValueError(f"expctl sets these itself: {', '.join(sorted(bad))}")
    slug = re.sub(r"[^A-Za-z0-9_.-]+", "-", name or "run").strip("-")[:40] or "run"
    run_id = f"{datetime.now():%Y%m%d_%H%M%S}_{slug}"
    with lock:
        if (RESULTS / run_id).exists():
            raise ValueError(f"{run_id} already exists; try again in a second")
        state = {
            "id": run_id,
            "name": name or slug,
            "args": args,
            "status": "queued",
            "resume": False,
            "created": now(),
            "started": None,
            "finished": None,
            "pid": None,
            "exit_code": None,
            "commit": None,
            "dirty": None,
        }
        save(state)
    return state


def stop(run_id):
    with lock:
        state = load(run_id)
        if state["status"] == "queued":
            state.update(status="stopped", finished=now())
        elif state["status"] == "running":
            # The whole process group: the client exits now, then experiment.sh's
            # TERM trap clears tc and stops the relay and publisher.
            try:
                os.killpg(state["pid"], signal.SIGTERM)
            except ProcessLookupError:
                pass
            state["status"] = "stopping"
        else:
            raise ValueError(f"cannot stop a {state['status']} run")
        save(state)
        return state


def resume(run_id):
    with lock:
        state = load(run_id)
        if state["status"] not in ("stopped", "failed", "interrupted"):
            raise ValueError(f"cannot resume a {state['status']} run")
        state.update(status="queued", resume=True, finished=None)
        save(state)
        return state


def tail(path, lines):
    if not path.is_file():
        return ""
    return "\n".join(path.read_text(errors="replace").splitlines()[-lines:]) + "\n"


def report(run_id):
    md = RESULTS / run_id / "results.md"
    if md.is_file():
        return md.read_text()
    r = subprocess.run(
        ["python3", str(RESULTS / "summarize.py"), str(RESULTS / run_id)],
        cwd=ROOT, capture_output=True, text=True, timeout=120,
    )
    return r.stdout + r.stderr


class Handler(BaseHTTPRequestHandler):
    def send(self, code, body, ctype="application/json"):
        if ctype == "application/json":
            body = json.dumps(body, indent=2)
        data = body if isinstance(body, bytes) else body.encode()
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def authorized(self):
        got = self.headers.get("Authorization", "")
        return secrets.compare_digest(got, f"Bearer {TOKEN}")

    def route(self, method):
        url = urlparse(self.path)
        parts = [p for p in url.path.split("/") if p]
        query = parse_qs(url.query)
        if method == "GET" and parts in ([], ["index.html"]):
            return self.send(200, (HERE / "index.html").read_bytes(), "text/html; charset=utf-8")
        if not parts or parts[0] != "api":
            return self.send(404, {"error": "not found"})
        if not self.authorized():
            return self.send(401, {"error": "missing or wrong token"})
        parts = parts[1:]
        if parts == ["runs"] and method == "GET":
            return self.send(200, [view(r) for r in all_runs()])
        if parts == ["runs"] and method == "POST":
            length = int(self.headers.get("Content-Length", 0))
            body = json.loads(self.rfile.read(length) or b"{}")
            return self.send(201, view(create(body.get("name", ""), body.get("args", []))))
        if len(parts) < 2 or parts[0] != "runs" or not ID_RE.match(parts[1]):
            return self.send(404, {"error": "not found"})
        run_id, rest = parts[1], parts[2:]
        if load(run_id) is None:
            return self.send(404, {"error": f"no run {run_id}"})
        if method == "GET" and rest == []:
            return self.send(200, view(load(run_id)))
        if method == "POST" and rest == ["stop"]:
            return self.send(200, view(stop(run_id)))
        if method == "POST" and rest == ["resume"]:
            return self.send(200, view(resume(run_id)))
        if method == "GET" and rest == ["log"]:
            lines = int(query.get("tail", ["200"])[0])
            return self.send(200, tail(ctl_dir(run_id) / "run.log", lines), "text/plain; charset=utf-8")
        if method == "GET" and rest == ["report"]:
            return self.send(200, report(run_id), "text/plain; charset=utf-8")
        if method == "GET" and rest == ["files"]:
            return self.send(200, sorted(p.name for p in (RESULTS / run_id).iterdir() if p.is_file()))
        if method == "GET" and len(rest) == 2 and rest[0] == "files" and ID_RE.match(rest[1]):
            f = RESULTS / run_id / rest[1]
            if f.is_file():
                return self.send(200, f.read_bytes(), "application/octet-stream")
        return self.send(404, {"error": "not found"})

    def handle_method(self, method):
        try:
            self.route(method)
        except ValueError as e:
            self.send(400, {"error": str(e)})
        except Exception as e:
            self.send(500, {"error": repr(e)})

    def do_GET(self):
        self.handle_method("GET")

    def do_POST(self):
        self.handle_method("POST")

    def log_message(self, fmt, *args):
        pass  # keep the service log for the scheduler


def main():
    if len(TOKEN) < 16:
        raise SystemExit("EXPCTL_TOKEN must be set (16+ characters)")
    RESULTS.mkdir(exist_ok=True)
    threading.Thread(target=scheduler, daemon=True).start()
    print(f"expctl on :{PORT}, results in {RESULTS}", flush=True)
    ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()


if __name__ == "__main__":
    main()
