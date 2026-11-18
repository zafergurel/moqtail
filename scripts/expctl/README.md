# expctl

Queue, start, stop and resume `scripts/experiment.sh` runs on the subscriber
machine through one HTTP JSON API. The web page (`/`), the `expctl` CLI and
`curl` all use the same API, so a run started from one is visible and
controllable from the others.

- One run executes at a time; the others wait in the queue.
- Each run lives in `results/<id>/`; its controller state (`state.json`,
  `run.log`, `exit_code`) is in `results/<id>/.expctl/`. The service keeps
  nothing only in memory, so it can restart at any time, and a restart does not
  stop a running experiment.
- **Stop** sends `TERM` to the run's process group: the subscriber exits at once
  and `experiment.sh` clears tc and stops the relay and publisher.
- **Resume** re-queues a stopped, failed or interrupted run with `--resume`;
  repetitions that already have a result file are skipped.
- Every run records the git commit it ran on (`*` = uncommitted changes).

## API

All `/api` calls need `Authorization: Bearer <token>`.

| Call | |
|---|---|
| `GET /api/runs` | All runs with status and progress |
| `POST /api/runs` `{"name": "...", "args": ["--method", "switch", ...]}` | Queue a run (`args` go to `experiment.sh`; `--output` and `--resume` are set by expctl) |
| `GET /api/runs/<id>` | One run |
| `POST /api/runs/<id>/stop` | Stop a running run, or drop a queued one |
| `POST /api/runs/<id>/resume` | Re-queue a stopped, failed or interrupted run |
| `GET /api/runs/<id>/log?tail=200` | Last lines of the run log |
| `GET /api/runs/<id>/report` | `results.md`, or `summarize.py` output while the run is unfinished |
| `GET /api/runs/<id>/files[/<name>]` | List or download result files |

## Install (on the subscriber machine)

```
mkdir -p ~/.config/expctl ~/.config/systemd/user
echo "EXPCTL_TOKEN=$(python3 -c 'import secrets; print(secrets.token_urlsafe(24))')" > ~/.config/expctl/env
chmod 600 ~/.config/expctl/env
ln -sf ~/projects/moqtail/scripts/expctl/expctl.service ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now expctl
sudo loginctl enable-linger "$USER"   # keep it running without a login session
```

## CLI

```
export EXPCTL_URL=http://hp.home.gurel:8080   # default
mkdir -p ~/.config/expctl && echo <token> > ~/.config/expctl/token
scripts/expctl/expctl start --name smoke -- --method switch --reps 1 --delays 0
scripts/expctl/expctl list
scripts/expctl/expctl log <id>
scripts/expctl/expctl stop <id>
scripts/expctl/expctl resume <id>
scripts/expctl/expctl report <id>
```
