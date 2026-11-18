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

"""Self-check for server.py against a fake experiment.sh: queue, stop, resume,
service restart mid-run, auth. Run: python3 scripts/expctl/selftest.py"""

import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
TOKEN = "selftest-token-0123456789"
PORT = "18089"

FAKE = r"""#!/usr/bin/env bash
# Fake experiment.sh: 4 reps, 1 s each, skips reps that have a result file.
out=""; resume=false
while [ $# -gt 0 ]; do case "$1" in --output) out="$2"; shift 2;; --resume) resume=true; shift;; *) shift;; esac; done
mkdir -p "$out"
trap 'echo "cleanup"; exit 143' TERM
echo "Total runs:       4"
for i in 1 2 3 4; do
  f="$out/fake_rep$i.json"
  if [ -s "$f" ]; then echo "SKIP: fake rep $i (already done)"; continue; fi
  echo "--- fake rep $i ---"
  sleep 1 & wait $!
  echo '{}' > "$f"
  echo "Progress: $i / 4"
done
"""


def call(method, path, body=None, token=TOKEN):
    req = urllib.request.Request(
        f"http://127.0.0.1:{PORT}/api{path}", method=method,
        data=json.dumps(body).encode() if body is not None else None,
        headers={"Authorization": f"Bearer {token}"},
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        return e.code, None


def wait_for(run_id, statuses, timeout=20):
    end = time.time() + timeout
    while time.time() < end:
        code, r = call("GET", f"/runs/{run_id}")
        if code == 200 and r["status"] in statuses:
            return r
        time.sleep(0.3)
    raise AssertionError(f"{run_id} never reached {statuses}: {r}")


def serve(root):
    env = {**os.environ, "EXPCTL_ROOT": root, "EXPCTL_TOKEN": TOKEN, "EXPCTL_PORT": PORT}
    p = subprocess.Popen([sys.executable, str(HERE / "server.py")], env=env)
    for _ in range(50):
        try:
            urllib.request.urlopen(f"http://127.0.0.1:{PORT}/", timeout=1)
            return p
        except OSError:
            time.sleep(0.1)
    raise AssertionError("server did not start")


def main():
    with tempfile.TemporaryDirectory() as root:
        (Path(root) / "scripts").mkdir()
        (Path(root) / "scripts/experiment.sh").write_text(FAKE)
        subprocess.run(["git", "init", "-q", root], check=True)
        srv = serve(root)
        try:
            assert call("GET", "/runs", token="wrong")[0] == 401
            assert call("POST", "/runs", {"args": ["--output", "x"]})[0] == 400

            _, a = call("POST", "/runs", {"name": "a", "args": []})
            time.sleep(1.1)  # run ids are per second
            _, b = call("POST", "/runs", {"name": "b", "args": []})
            wait_for(a["id"], {"running"})
            assert call("GET", f"/runs/{b['id']}")[1]["status"] == "queued", "queue: one at a time"

            time.sleep(1.5)
            call("POST", f"/runs/{a['id']}/stop")
            r = wait_for(a["id"], {"stopped"})
            assert 0 < r["progress"]["results"] < 4, r["progress"]

            # b starts once a is stopped; restart the service while it runs.
            wait_for(b["id"], {"running"})
            srv.terminate(); srv.wait()
            srv = serve(root)
            r = wait_for(b["id"], {"done"})
            assert r["progress"] == {"results": 4, "total": 4, "current": "fake rep 4"}, r["progress"]

            call("POST", f"/runs/{a['id']}/resume")
            r = wait_for(a["id"], {"done"})
            assert r["progress"]["results"] == 4 and r["resume"] is True
            log = (Path(root) / "results" / a["id"] / ".expctl/run.log").read_text()
            assert "SKIP: fake rep 1" in log, "resume skips finished reps"
        finally:
            srv.terminate()
    print("expctl selftest: ok")


if __name__ == "__main__":
    main()
