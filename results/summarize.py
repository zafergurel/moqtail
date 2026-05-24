#!/usr/bin/env python3
"""Summarize experiment results from a paper_run_* directory.

Usage:
    python3 results/summarize.py results/paper_run_02
"""
import json
import re
import statistics
import sys
from pathlib import Path


def avg(lst, decimals=0):
    clean = [x for x in lst if x is not None]
    if not clean:
        return "-"
    m = statistics.mean(clean)
    return round(m) if decimals == 0 else round(m, decimals)


METHODS = ["switch-message", "sub-update-forward", "joining-fetch"]

# All 16 scenarios in display order
SCENARIOS = [
    # Relative-position: A→B
    "rp_a2b_a500", "rp_a2b_a1000", "rp_a2b_a2000",
    "rp_a2b_b500", "rp_a2b_b1000", "rp_a2b_b2000",
    # Relative-position: B→A
    "rp_b2a_a500", "rp_b2a_a1000", "rp_b2a_a2000",
    "rp_b2a_b500", "rp_b2a_b1000", "rp_b2a_b2000",
    # Bandwidth conditions
    "bw_a2b_4500k", "bw_a2b_7000k", "bw_b2a_3000k", "bw_b2a_7000k",
]


def load_runs(results_dir: Path):
    """Return dict[method][scenario] = list of JSON dicts (one per rep)."""
    data = {m: {s: [] for s in SCENARIOS} for m in METHODS}
    for f in sorted(results_dir.glob("*.json")):
        name = f.stem  # e.g. joining-fetch_rp_a2b_a500_rep1
        for method in METHODS:
            prefix = method + "_"
            if not name.startswith(prefix):
                continue
            rest = name[len(prefix):]  # e.g. rp_a2b_a500_rep1
            idx = rest.rfind("_rep")
            if idx < 0:
                break
            scenario = rest[:idx]
            if scenario not in data[method]:
                break
            try:
                data[method][scenario].append(json.loads(f.read_text()))
            except Exception:
                pass
            break
    return data


def load_all_runs(results_dir: Path):
    """Return dict[method][scenario] = list of JSON dicts for any scenario."""
    data: dict = {}
    for f in sorted(results_dir.glob("*.json")):
        name = f.stem
        for method in METHODS:
            prefix = method + "_"
            if not name.startswith(prefix):
                continue
            rest = name[len(prefix):]
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


def print_generic_summary(data: dict):
    all_scenarios: list = sorted({s for m in data.values() for s in m})
    if not all_scenarios:
        print("No result files found.")
        return

    col_w = 12
    label_w = 22

    for metric_name, extractor in [
        ("Delay (ms)", lambda r, i: (r.get("switches") or [{}])[i].get("switch_delay_ms")),
        ("Stall (ms)", lambda r, i: (r.get("switches") or [{}])[i].get("stall_ms")),
        ("AETR",       lambda r, i: r.get("aetr")),
    ]:
        print(f"\n  {metric_name}")
        print("  " + f"{'Method/Scenario':<{label_w}}" + "".join(f"{s:>{col_w}}" for s in all_scenarios))
        print("  " + "-" * (label_w + col_w * len(all_scenarios)))
        for method in METHODS:
            if method not in data:
                continue
            row_vals = []
            for scen in all_scenarios:
                runs = data[method].get(scen, [])
                # collect value for switch index 0 across all reps
                vals = []
                for r in runs:
                    sw = r.get("switches") or []
                    if metric_name == "AETR":
                        v = r.get("aetr")
                    else:
                        v = sw[0].get("switch_delay_ms" if "Delay" in metric_name else "stall_ms") if sw else None
                    vals.append(v)
                row_vals.append(avg(vals, decimals=4 if metric_name == "AETR" else 0))
            print("  " + f"{method:<{label_w}}" + "".join(f"{str(v):>{col_w}}" for v in row_vals))


def print_section(title, rows):
    """Print a table section. rows: list of (label, values...)."""
    if not rows:
        return
    col_w = max(len(str(v)) for row in rows for v in row[1:]) + 2
    label_w = max(len(row[0]) for row in rows) + 2
    print(f"\n{title}")
    print("-" * (label_w + col_w * (len(rows[0]) - 1)))
    for row in rows:
        label = row[0]
        vals = "".join(f"{str(v):>{col_w}}" for v in row[1:])
        print(f"{label:<{label_w}}{vals}")


def main(results_dir: Path):
    data = load_runs(results_dir)

    if not any(data[m][s] for m in METHODS for s in SCENARIOS):
        print(f"\nGeneric summary: {results_dir}")
        print_generic_summary(load_all_runs(results_dir))
        return


    # ── Relative-position table (latency + freeze) ──────────────────────────────
    rp_scenarios_a2b = [s for s in SCENARIOS if s.startswith("rp_a2b")]
    rp_scenarios_b2a = [s for s in SCENARIOS if s.startswith("rp_b2a")]

    for direction, rp_scens in [("A→B", rp_scenarios_a2b), ("B→A", rp_scenarios_b2a)]:
        print(f"\n{'='*70}")
        print(f"  Relative-position: {direction}")
        print(f"{'='*70}")

        # Header
        col_labels = [s.replace("rp_a2b_", "").replace("rp_b2a_", "") for s in rp_scens]
        header_lat = ["Method"] + [f"{l}_lat" for l in col_labels]
        header_frz = ["Method"] + [f"{l}_frz" for l in col_labels]
        col_w = 10
        label_w = 22

        print("\n  Delay (ms)  [- = timed out]")
        print("  " + f"{'Method':<{label_w}}" + "".join(f"{h:>{col_w}}" for h in col_labels))
        print("  " + "-" * (label_w + col_w * len(col_labels)))
        for method in METHODS:
            vals = []
            for scen in rp_scens:
                runs = data[method][scen]
                lats = [r["switches"][0].get("switch_delay_ms") for r in runs if r.get("switches")]
                vals.append(avg(lats))
            print("  " + f"{method:<{label_w}}" + "".join(f"{str(v):>{col_w}}" for v in vals))

        print("\n  Freeze (ms)")
        print("  " + f"{'Method':<{label_w}}" + "".join(f"{h:>{col_w}}" for h in col_labels))
        print("  " + "-" * (label_w + col_w * len(col_labels)))
        for method in METHODS:
            vals = []
            for scen in rp_scens:
                runs = data[method][scen]
                stls = [r["switches"][0].get("stall_ms") for r in runs if r.get("switches")]
                vals.append(avg(stls))
            print("  " + f"{method:<{label_w}}" + "".join(f"{str(v):>{col_w}}" for v in vals))

        print("\n  AETR")
        print("  " + f"{'Method':<{label_w}}" + "".join(f"{h:>{col_w}}" for h in col_labels))
        print("  " + "-" * (label_w + col_w * len(col_labels)))
        for method in METHODS:
            vals = []
            for scen in rp_scens:
                runs = data[method][scen]
                aetrs = [r.get("aetr") for r in runs if r.get("aetr") is not None]
                vals.append(avg(aetrs, decimals=4))
            print("  " + f"{method:<{label_w}}" + "".join(f"{str(v):>{col_w}}" for v in vals))

    # ── Bandwidth-condition table ───────────────────────────────────────────────
    bw_scens = [s for s in SCENARIOS if s.startswith("bw_")]
    bw_labels = [s.replace("bw_", "") for s in bw_scens]
    col_w = 12
    label_w = 22

    print(f"\n{'='*70}")
    print("  Bandwidth conditions")
    print(f"{'='*70}")

    for metric_name, key, decimals in [("Delay (ms)", "switch_delay_ms", 0),
                                        ("Stall (ms)",   "stall_ms",          0),
                                        ("AETR",         "aetr",              4)]:
        if key == "aetr":
            print(f"\n  {metric_name}")
            print("  " + f"{'Method':<{label_w}}" + "".join(f"{h:>{col_w}}" for h in bw_labels))
            print("  " + "-" * (label_w + col_w * len(bw_scens)))
            for method in METHODS:
                vals = []
                for scen in bw_scens:
                    runs = data[method][scen]
                    aetrs = [r.get("aetr") for r in runs]
                    vals.append(avg(aetrs, decimals=4))
                print("  " + f"{method:<{label_w}}" + "".join(f"{str(v):>{col_w}}" for v in vals))
        else:
            print(f"\n  {metric_name}  [- = timed out]")
            print("  " + f"{'Method':<{label_w}}" + "".join(f"{h:>{col_w}}" for h in bw_labels))
            print("  " + "-" * (label_w + col_w * len(bw_scens)))
            for method in METHODS:
                vals = []
                for scen in bw_scens:
                    runs = data[method][scen]
                    mvals = [r["switches"][0].get(key) for r in runs if r.get("switches")]
                    vals.append(avg(mvals, decimals=decimals))
                print("  " + f"{method:<{label_w}}" + "".join(f"{str(v):>{col_w}}" for v in vals))


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print(f"Usage: {sys.argv[0]} <results_dir>", file=sys.stderr)
        sys.exit(1)
    main(Path(sys.argv[1]))
