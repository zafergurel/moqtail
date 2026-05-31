#!/usr/bin/env python3
"""Summarize experiment results from a results directory.

Usage:
    python3 results/summarize.py <results_dir> [--from-logs]

When the directory contains an experiment-metadata.json file (written by
experiment.sh or local_test.sh), the scenario list, methods, and jitter
buffer size are all read from there.  Otherwise falls back to the
hardcoded 16-scenario canonical layout for backward compatibility.

With --from-logs: re-derives all metrics from the *_events.csv files and
writes results-from-logs.md alongside results.md for cross-validation.
"""
import contextlib
import io
import json
import statistics
import sys
from pathlib import Path


def avg(lst, decimals=0):
    clean = [x for x in lst if x is not None]
    if not clean:
        return "-"
    m = statistics.mean(clean)
    return round(m) if decimals == 0 else round(m, decimals)


# ── Backward-compat constants (used only when no metadata file is found) ───────

_LEGACY_METHODS = ["switch-message", "sub-update-forward", "joining-fetch"]

_LEGACY_SCENARIOS = [
    "rp_a2b_a500", "rp_a2b_a1000", "rp_a2b_a2000",
    "rp_a2b_b500", "rp_a2b_b1000", "rp_a2b_b2000",
    "rp_b2a_a500", "rp_b2a_a1000", "rp_b2a_a2000",
    "rp_b2a_b500", "rp_b2a_b1000", "rp_b2a_b2000",
    "bw_a2b_4500k", "bw_a2b_7000k", "bw_b2a_3000k", "bw_b2a_7000k",
]


# ── File loading ───────────────────────────────────────────────────────────────

def load_metadata(results_dir: Path):
    """Return parsed experiment-metadata.json, or None if absent/unreadable."""
    meta_path = results_dir / "experiment-metadata.json"
    if meta_path.exists():
        try:
            return json.loads(meta_path.read_text())
        except Exception:
            return None
    return None


def _load_runs_for(results_dir: Path, methods, scenario_labels):
    """Return dict[method][scenario] = list of run dicts."""
    known = set(scenario_labels)
    data: dict = {m: {s: [] for s in scenario_labels} for m in methods}
    for f in sorted(results_dir.glob("*.json")):
        if f.stem == "experiment-metadata":
            continue
        for method in methods:
            prefix = method + "_"
            if not f.stem.startswith(prefix):
                continue
            rest = f.stem[len(prefix):]
            idx = rest.rfind("_rep")
            if idx < 0:
                break
            scenario = rest[:idx]
            if scenario not in known:
                break
            try:
                data[method][scenario].append(json.loads(f.read_text()))
            except Exception:
                pass
            break
    return data


def _load_all_runs(results_dir: Path, methods):
    """Return dict[method][scenario] = list of run dicts, any scenario."""
    data: dict = {}
    for f in sorted(results_dir.glob("*.json")):
        if f.stem == "experiment-metadata":
            continue
        for method in methods:
            prefix = method + "_"
            if not f.stem.startswith(prefix):
                continue
            rest = f.stem[len(prefix):]
            idx = rest.rfind("_rep")
            if idx < 0:
                break
            scenario = rest[:idx]
            data.setdefault(method, {}).setdefault(scenario, [])
            try:
                data[method][scenario].append(json.loads(f.read_text()))
            except Exception:
                pass
            break
    return data


def _load_all_runs_auto(results_dir: Path):
    """Auto-discover methods and scenarios from filenames.

    Filename format: {method}_{scenario}_rep{N}.json
    Method names use only hyphens (no underscores), so the first '_'
    in the stem (after stripping '_repN') splits method from scenario.
    """
    data: dict = {}
    for f in sorted(results_dir.glob("*.json")):
        if f.stem == "experiment-metadata":
            continue
        idx = f.stem.rfind("_rep")
        if idx < 0:
            continue
        stem_no_rep = f.stem[:idx]
        first_under = stem_no_rep.find("_")
        if first_under < 0:
            continue
        method = stem_no_rep[:first_under]
        scenario = stem_no_rep[first_under + 1:]
        data.setdefault(method, {}).setdefault(scenario, [])
        try:
            data[method][scenario].append(json.loads(f.read_text()))
        except Exception:
            pass
    return data


# ── Log-based metric derivation ───────────────────────────────────────────────

def parse_csv_log(csv_path: Path, meta: dict) -> "dict | None":
    """Re-derive switch metrics from a single *_events.csv file.

    CSV column layout (since payload-size logging was added):
        object, label, group, obj_id, payload_bytes, wall_ms[, pt_ms]

    Returns a dict shaped like a JSON run file (top-level aetr + switches[0])
    so it can be fed directly to _metric_rows without modification.
    Returns None if the file is missing key events (e.g. no switch fired).
    """
    jb       = meta.get("jitter_buffer_ms", 40)
    frame_ms = meta.get("interval_ms", 40)
    opg      = meta.get("objects_per_group", 25)
    gop_ms   = opg * frame_ms

    first_group_a        = None
    switch_decision_wall = None
    fresh_b_group        = None
    fresh_b_wall         = None
    last_a_group         = None
    last_a_object        = None

    pre_switch_a_bytes = 0
    trailing_a_bytes   = 0
    redundant_bytes    = 0
    useful_b_bytes     = 0
    b_gop_bytes        = 0

    try:
        rows = [ln.split(",") for ln in csv_path.read_text().splitlines() if ln.strip()]
    except Exception:
        return None

    # Detect format: new CSVs have a payload_bytes column at index 4.
    # Old CSVs go straight to wall_ms at index 4.
    # A 6-field object row has pt_ms (float, contains '.') at index 5 in the
    # old format, but wall_ms (integer) at index 5 in the new format.
    has_payload_col = False
    for p in rows:
        if p[0] == "object" and len(p) >= 6:
            has_payload_col = "." not in p[5]
            break

    # Column indices depend on format.
    # Old: object, label, group, obj, wall_ms[, pt_ms]
    # New: object, label, group, obj, payload_bytes, wall_ms[, pt_ms]
    i_wall   = 5 if has_payload_col else 4
    i_bytes  = 4 if has_payload_col else None

    # First pass: locate key events and last_a_at_switch (logged by Rust at
    # acceptance time — the exact values passed to compute_switch_metrics).
    last_a_at_switch_group: "int | None" = None
    last_a_at_switch_object: "int | None" = None
    for p in rows:
        if p[0] == "switch_decision":
            switch_decision_wall = int(p[1])
        elif p[0] == "fresh_b_iframe":
            fresh_b_group = int(p[1])
            fresh_b_wall  = int(p[2])
        elif p[0] == "last_a_at_switch":
            last_a_at_switch_group  = int(p[1])
            last_a_at_switch_object = int(p[2])
        elif p[0] == "object" and p[1] == "A":
            if int(p[3]) == 0 and first_group_a is None:
                first_group_a = int(p[2])

    if any(v is None for v in [first_group_a, switch_decision_wall,
                                fresh_b_group, fresh_b_wall]):
        return None

    if last_a_at_switch_group is not None:
        # New CSVs: exact values from compute_switch_metrics call site.
        last_a_group  = last_a_at_switch_group
        last_a_object = last_a_at_switch_object
    else:
        # Old CSVs: approximate from the last A object at or before fresh_b_wall.
        # Trailing A objects (stop_a propagation lag) are excluded by the wall cap.
        for p in rows:
            if p[0] != "object" or p[1] != "A":
                continue
            if int(p[i_wall]) <= fresh_b_wall:
                last_a_group  = int(p[2])
                last_a_object = int(p[3])

    if last_a_group is None:
        return None

    # ── Timing ────────────────────────────────────────────────────────────────
    switch_delay_ms = fresh_b_wall - switch_decision_wall

    if has_payload_col:
        # New format (payload logging added): code uses last *received* A object
        # + frame_interval_ms in compute_switch_metrics.
        a_stopped_at = (
            (last_a_group - first_group_a) * gop_ms
            + last_a_object * frame_ms
            + jb
            + frame_ms
        )
    else:
        # Old format: code used the theoretical player position at the moment
        # the fresh B I-frame arrived (last_a_object_at formula), without
        # adding frame_interval_ms.
        max_rel = (fresh_b_wall - jb) / gop_ms
        if max_rel >= 0:
            played_group = first_group_a + int(max_rel)
            played_obj   = int((fresh_b_wall - jb) % gop_ms / frame_ms)
            a_stopped_at = (played_group - first_group_a) * gop_ms + played_obj * frame_ms + jb
        else:
            a_stopped_at = float(jb)

    b_started_at    = fresh_b_wall + jb
    stall_ms        = int(max(0, b_started_at - a_stopped_at))
    skipped_gops    = max(0, fresh_b_group - (last_a_group + 1))
    skipped_dur_ms  = skipped_gops * gop_ms

    # ── Bytes ─────────────────────────────────────────────────────────────────
    # pre_switch_a_bytes: A bytes received during Phase 3 only
    #   (>= switch_decision_wall, before fresh_b_wall).
    # Phase 1 A bytes are normal playback and not counted as overhead.
    # Byte metrics are only available in the new CSV format (has_payload_col).
    for p in rows:
        if p[0] != "object" or len(p) <= i_wall or i_bytes is None:
            continue
        label, group, obj  = p[1], int(p[2]), int(p[3])
        nbytes, wall_ms    = int(p[i_bytes]), int(p[i_wall])

        if label == "A":
            if switch_decision_wall <= wall_ms < fresh_b_wall:
                pre_switch_a_bytes += nbytes
            elif wall_ms >= fresh_b_wall:
                trailing_a_bytes += nbytes
        elif label in ("B", "F"):
            if group < fresh_b_group:
                redundant_bytes += nbytes
            elif group == fresh_b_group:
                # The accepted GOP: both the I-frame and its tail objects.
                useful_b_bytes += nbytes
                b_gop_bytes    += nbytes
            else:
                useful_b_bytes += nbytes

    excess_bytes = redundant_bytes + trailing_a_bytes
    aetr         = excess_bytes / b_gop_bytes if b_gop_bytes > 0 else 0.0

    sw = {
        "switch_delay_ms":     switch_delay_ms,
        "stall_ms":            stall_ms,
        "skipped_gops":        skipped_gops,
        "skipped_duration_ms": skipped_dur_ms,
        "a_stopped_at_ms":     float(a_stopped_at),
        "b_started_at_ms":     float(b_started_at),
        "pre_switch_a_bytes":  pre_switch_a_bytes,
        "trailing_a_bytes":    trailing_a_bytes,
        "redundant_bytes":     redundant_bytes,
        "useful_b_bytes":      useful_b_bytes,
        "b_gop_payload_bytes": b_gop_bytes,
        "excess_bytes":        excess_bytes,
    }
    return {"aetr": round(aetr, 6), "switches": [sw]}


def analyze_from_logs(results_dir: Path, methods, scenario_labels, meta) -> dict:
    """Build data[method][scenario] = [run, ...] from *_events.csv files."""
    known = set(scenario_labels)
    data: dict = {m: {s: [] for s in scenario_labels} for m in methods}

    for f in sorted(results_dir.glob("*_events.csv")):
        # Filename: {method}_{scenario}_rep{N}_events.csv
        stem = f.stem  # e.g. "joining-fetch_sync_rep1_events"
        if not stem.endswith("_events"):
            continue
        stem = stem[: -len("_events")]  # e.g. "joining-fetch_sync_rep1"

        idx = stem.rfind("_rep")
        if idx < 0:
            continue
        stem_no_rep = stem[:idx]  # e.g. "joining-fetch_sync"

        first_under = stem_no_rep.find("_")
        if first_under < 0:
            continue
        method   = stem_no_rep[:first_under]
        scenario = stem_no_rep[first_under + 1:]

        if method not in methods or scenario not in known:
            continue

        run = parse_csv_log(f, meta)
        if run is not None:
            data[method][scenario].append(run)

    return data


# ── Rendering helpers ──────────────────────────────────────────────────────────

def short_label(label: str) -> str:
    for prefix in ("rp_a2b_", "rp_b2a_", "bw_", "delay_a2b_", "delay_b2a_"):
        if label.startswith(prefix):
            return label[len(prefix):]
    if label.startswith("a_ahead_"):
        return "a_" + label[len("a_ahead_"):]
    if label.startswith("b_ahead_"):
        return "b_" + label[len("b_ahead_"):]
    return label


def _print_table(title, method_rows, col_labels, col_w, label_w):
    print(f"\n{title}")
    print("  " + f"{'Method':<{label_w}}" + "".join(f"{h:>{col_w}}" for h in col_labels))
    print("  " + "-" * (label_w + col_w * len(col_labels)))
    for method, vals in method_rows:
        print("  " + f"{method:<{label_w}}" + "".join(f"{str(v):>{col_w}}" for v in vals))


def _metric_rows(data, methods, labels, jitter_ms):
    """Yield (title, method_rows) for Delay, Stall, and AETR."""
    for title, key, decimals in [
        ("  Delay (ms)  [- = timed out]", "switch_delay_ms",    0),
        (f"  Stall (ms)  [J={jitter_ms}ms]",  "stall_ms",       0),
        ("  Skipped (ms)",                "skipped_duration_ms", 0),
        ("  AETR",                         "aetr",               4),
    ]:
        rows = []
        for method in methods:
            vals = []
            for label in labels:
                runs = data.get(method, {}).get(label, [])
                if key == "aetr":
                    mvals = [r.get("aetr") for r in runs]
                else:
                    mvals = [(r.get("switches") or [{}])[0].get(key) for r in runs]
                vals.append(avg(mvals, decimals=decimals))
            rows.append((method, vals))
        yield title, rows


def print_rp_section(data, methods, rp_a2b_scens, rp_b2a_scens, jitter_ms):
    col_w, label_w = 10, 22
    for direction, scens in [("A→B", rp_a2b_scens), ("B→A", rp_b2a_scens)]:
        if not scens:
            continue
        labels = [s["label"] for s in scens]
        col_labels = [short_label(l) for l in labels]
        print(f"\n{'='*70}")
        print(f"  Relative-position: {direction}")
        print(f"{'='*70}")
        for title, rows in _metric_rows(data, methods, labels, jitter_ms):
            _print_table(title, rows, col_labels, col_w, label_w)


def print_bw_section(data, methods, bw_scens, jitter_ms):
    col_w, label_w = 12, 22
    labels = [s["label"] for s in bw_scens]
    col_labels = [short_label(l) for l in labels]
    print(f"\n{'='*70}")
    print("  Bandwidth conditions")
    print(f"{'='*70}")
    for title, rows in _metric_rows(data, methods, labels, jitter_ms):
        _print_table(title, rows, col_labels, col_w, label_w)


def print_downstream_delay_section(data, methods, dl_a2b_scens, dl_b2a_scens, jitter_ms):
    col_w, label_w = 12, 22
    for direction, scens in [("A→B", dl_a2b_scens), ("B→A", dl_b2a_scens)]:
        if not scens:
            continue
        labels = [s["label"] for s in scens]
        col_labels = [short_label(l) for l in labels]
        print(f"\n{'='*70}")
        print(f"  Downstream delay: {direction}")
        print(f"{'='*70}")
        for title, rows in _metric_rows(data, methods, labels, jitter_ms):
            _print_table(title, rows, col_labels, col_w, label_w)


def print_generic_summary(data: dict, methods):
    """Flat fallback: all discovered scenarios in a single table."""
    all_scenarios = sorted({s for m in data.values() for s in m})
    if not all_scenarios:
        print("No result files found.")
        return
    col_labels = [short_label(s) for s in all_scenarios]
    col_w, label_w = 12, 22
    for title, key, decimals in [
        ("  Delay (ms)",   "switch_delay_ms",    0),
        ("  Stall (ms)",   "stall_ms",            0),
        ("  Skipped (ms)", "skipped_duration_ms", 0),
        ("  AETR",         "aetr",                4),
    ]:
        print(f"\n{title}")
        print("  " + f"{'Method':<{label_w}}" + "".join(f"{s:>{col_w}}" for s in col_labels))
        print("  " + "-" * (label_w + col_w * len(all_scenarios)))
        for method in methods:
            if method not in data:
                continue
            vals = []
            for scen in all_scenarios:
                runs = data[method].get(scen, [])
                if key == "aetr":
                    mvals = [r.get("aetr") for r in runs]
                else:
                    mvals = [(r.get("switches") or [{}])[0].get(key) for r in runs]
                vals.append(avg(mvals, decimals=decimals))
            print("  " + f"{method:<{label_w}}" + "".join(f"{str(v):>{col_w}}" for v in vals))


# ── Log-derived summary ───────────────────────────────────────────────────────

def main_from_logs(results_dir: Path):
    """Re-derive metrics from *_events.csv and write results-from-logs.md."""
    meta = load_metadata(results_dir)
    if meta is None:
        print("No experiment-metadata.json found — cannot derive from logs.", file=sys.stderr)
        return

    methods       = meta["methods"]
    scenarios     = meta["scenarios"]
    jitter_ms     = meta.get("jitter_buffer_ms", 40)
    track_a       = meta["track_a"]
    track_b       = meta["track_b"]

    data = analyze_from_logs(results_dir, methods, [s["label"] for s in scenarios], meta)

    rp_a2b = [s for s in scenarios
              if s["bw_bps"] == 0
              and s.get("downstream_delay_ms", 0) == 0
              and s["sequence"].split(",")[0] == track_a
              and not s["label"].startswith("delay_")]
    rp_b2a = [s for s in scenarios
              if s["bw_bps"] == 0
              and s.get("downstream_delay_ms", 0) == 0
              and s["sequence"].split(",")[0] == track_b
              and not s["label"].startswith("delay_")]
    bw_scens = [s for s in scenarios if s["bw_bps"] > 0]
    dl_a2b   = [s for s in scenarios if s["label"].startswith("delay_a2b_")]
    dl_b2a   = [s for s in scenarios if s["label"].startswith("delay_b2a_")]

    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        if rp_a2b or rp_b2a:
            print_rp_section(data, methods, rp_a2b, rp_b2a, jitter_ms)
        if bw_scens:
            print_bw_section(data, methods, bw_scens, jitter_ms)
        if dl_a2b or dl_b2a:
            print_downstream_delay_section(data, methods, dl_a2b, dl_b2a, jitter_ms)
        if not (rp_a2b or rp_b2a or bw_scens or dl_a2b or dl_b2a):
            print_generic_summary(data, methods)

    out_path = results_dir / "results-from-logs.md"
    out_path.write_text(buf.getvalue())
    print(f"Log-derived results: {out_path}")


# ── Main ───────────────────────────────────────────────────────────────────────

def main(results_dir: Path):
    meta = load_metadata(results_dir)

    if meta is None:
        # ── Backward compat: no metadata file ──────────────────────────────────
        # Try the canonical 16-scenario layout first.
        data_legacy = _load_runs_for(results_dir, _LEGACY_METHODS, _LEGACY_SCENARIOS)
        if any(data_legacy[m][s] for m in _LEGACY_METHODS for s in _LEGACY_SCENARIOS):
            rp_a2b = [{"label": s} for s in _LEGACY_SCENARIOS if s.startswith("rp_a2b")]
            rp_b2a = [{"label": s} for s in _LEGACY_SCENARIOS if s.startswith("rp_b2a")]
            bw_scens = [{"label": s} for s in _LEGACY_SCENARIOS if s.startswith("bw_")]
            print_rp_section(data_legacy, _LEGACY_METHODS, rp_a2b, rp_b2a, jitter_ms=40)
            print_bw_section(data_legacy, _LEGACY_METHODS, bw_scens, jitter_ms=40)
        else:
            # Unknown scenario names — generic flat table, auto-discover methods.
            print(f"\nGeneric summary: {results_dir}")
            auto_data = _load_all_runs_auto(results_dir)
            auto_methods = sorted(auto_data.keys())
            print_generic_summary(auto_data, auto_methods)
        return

    # ── Metadata-driven path ───────────────────────────────────────────────────
    methods   = meta["methods"]
    scenarios = meta["scenarios"]
    track_a   = meta["track_a"]
    track_b   = meta["track_b"]
    jitter_ms = meta.get("jitter_buffer_ms", 40)

    data = _load_runs_for(results_dir, methods, [s["label"] for s in scenarios])

    rp_a2b   = [s for s in scenarios
                if s["bw_bps"] == 0
                and s.get("downstream_delay_ms", 0) == 0
                and s["sequence"].split(",")[0] == track_a
                and not s["label"].startswith("delay_")]
    rp_b2a   = [s for s in scenarios
                if s["bw_bps"] == 0
                and s.get("downstream_delay_ms", 0) == 0
                and s["sequence"].split(",")[0] == track_b
                and not s["label"].startswith("delay_")]
    bw_scens = [s for s in scenarios if s["bw_bps"] > 0]
    dl_a2b   = [s for s in scenarios if s["label"].startswith("delay_a2b_")]
    dl_b2a   = [s for s in scenarios if s["label"].startswith("delay_b2a_")]

    if rp_a2b or rp_b2a:
        print_rp_section(data, methods, rp_a2b, rp_b2a, jitter_ms)
    if bw_scens:
        print_bw_section(data, methods, bw_scens, jitter_ms)
    if dl_a2b or dl_b2a:
        print_downstream_delay_section(data, methods, dl_a2b, dl_b2a, jitter_ms)


if __name__ == "__main__":
    args = sys.argv[1:]
    if not args or args[0] in ("-h", "--help"):
        print(f"Usage: {sys.argv[0]} <results_dir> [--from-logs]", file=sys.stderr)
        sys.exit(0 if args else 1)
    from_logs = "--from-logs" in args
    dirs = [a for a in args if not a.startswith("--")]
    if not dirs:
        print(f"Usage: {sys.argv[0]} <results_dir> [--from-logs]", file=sys.stderr)
        sys.exit(1)
    p = Path(dirs[0])
    if from_logs:
        main_from_logs(p)
    else:
        main(p)
