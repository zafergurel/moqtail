#!/usr/bin/env python3
"""Summarize experiment results from a results directory.

Usage:
    python3 results/summarize.py <results_dir>

When the directory contains an experiment-metadata.json file (written by
experiment.sh or local_test.sh), the scenario list, methods, and jitter
buffer size are all read from there.  Otherwise falls back to the
hardcoded 16-scenario canonical layout for backward compatibility.
"""
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


# ── Rendering helpers ──────────────────────────────────────────────────────────

def short_label(label: str) -> str:
    for prefix in ("rp_a2b_", "rp_b2a_", "bw_"):
        if label.startswith(prefix):
            return label[len(prefix):]
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
        ("  Delay (ms)  [- = timed out]", "switch_delay_ms", 0),
        (f"  Stall (ms)  [J={jitter_ms}ms]",  "stall_ms",          0),
        ("  AETR",                             "aetr",              4),
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


def print_generic_summary(data: dict, methods):
    """Flat fallback: all discovered scenarios in a single table."""
    all_scenarios = sorted({s for m in data.values() for s in m})
    if not all_scenarios:
        print("No result files found.")
        return
    col_w, label_w = 12, 22
    for title, key, decimals in [
        ("  Delay (ms)", "switch_delay_ms", 0),
        ("  Stall (ms)", "stall_ms",         0),
        ("  AETR",       "aetr",             4),
    ]:
        print(f"\n{title}")
        print("  " + f"{'Method':<{label_w}}" + "".join(f"{s:>{col_w}}" for s in all_scenarios))
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
            # Unknown scenario names — generic flat table.
            print(f"\nGeneric summary: {results_dir}")
            print_generic_summary(_load_all_runs(results_dir, _LEGACY_METHODS), _LEGACY_METHODS)
        return

    # ── Metadata-driven path ───────────────────────────────────────────────────
    methods   = meta["methods"]
    scenarios = meta["scenarios"]
    track_a   = meta["track_a"]
    track_b   = meta["track_b"]
    jitter_ms = meta.get("jitter_buffer_ms", 40)

    data = _load_runs_for(results_dir, methods, [s["label"] for s in scenarios])

    rp_a2b   = [s for s in scenarios
                if s["bw_bps"] == 0 and s["sequence"].split(",")[0] == track_a]
    rp_b2a   = [s for s in scenarios
                if s["bw_bps"] == 0 and s["sequence"].split(",")[0] == track_b]
    bw_scens = [s for s in scenarios if s["bw_bps"] > 0]

    if rp_a2b or rp_b2a:
        print_rp_section(data, methods, rp_a2b, rp_b2a, jitter_ms)
    if bw_scens:
        print_bw_section(data, methods, bw_scens, jitter_ms)


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <results_dir>", file=sys.stderr)
        sys.exit(1)
    main(Path(sys.argv[1]))
