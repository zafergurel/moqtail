#!/usr/bin/env python3
"""Generate the paper's result figures directly from experiment output.

Reads one or more results directories and emits vector PDFs sized for the
IEEEtran two-column layout. Values are never hardcoded here, so the figures
cannot drift from the tables.

Usage
-----
    make_charts.py RESULTS_DIR [RESULTS_DIR ...] [--out DIR]

When several directories are given, later ones override earlier ones for any
(method, scenario) cell they both contain. That is how a re-run with more
repetitions is layered on top of the original matrix, e.g.

    make_charts.py results/20260531_115720 results/20260806_195807 \
        --out overleaf-repo/figures

Each bar is a mean over the repetitions in the winning directory; the whisker
spans min-max across those repetitions. Cells whose runs never delivered a
target-track I-frame are drawn as a hatched stub labelled T/O.
"""

import argparse
import json
import sys
from pathlib import Path

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import matplotlib.patches as mpatches
import numpy as np

# ── Paper conventions ──────────────────────────────────────────────────────────

# File-name key -> label used in the figure legend.
METHODS = [
    ("sub-update-forward", "Forward State Toggling"),
    ("joining-fetch",      "Joining FETCH"),
    ("switch",             "SWITCH Msg"),
]
# Colour plus hatch, so the three series stay distinguishable in grayscale print.
STYLE = [
    ("#4878CF", ""),
    ("#E8602C", "//"),
    ("#2CA02C", "xx"),
]
TO_COLOR = "#D9D9D9"

RP_A2B = ["rp_a2b_sync", "rp_a2b_a1000", "rp_a2b_a2000", "rp_a2b_b1000", "rp_a2b_b2000"]
RP_B2A = ["rp_b2a_sync", "rp_b2a_a1000", "rp_b2a_a2000", "rp_b2a_b1000", "rp_b2a_b2000"]
RP_TICKS = [r"sync", r"A ahead" "\n" r"$\delta$=1s", r"A ahead" "\n" r"$\delta$=2s",
            r"B ahead" "\n" r"$\delta$=1s", r"B ahead" "\n" r"$\delta$=2s"]

# IEEEtran conference: 3.4in single column, 7.16in across both.
COL_W, FULL_W = 3.4, 7.16

plt.rcParams.update({
    "font.size": 7,
    "axes.labelsize": 7.5,
    "axes.titlesize": 8,
    "xtick.labelsize": 6.5,
    "ytick.labelsize": 6.5,
    "legend.fontsize": 7,
    "pdf.fonttype": 42,          # embed TrueType, not Type-3; some venues require it
    "axes.linewidth": 0.6,
    "figure.dpi": 200,
})


# ── Loading ────────────────────────────────────────────────────────────────────

def load_dirs(dirs):
    """dict[method][scenario] = list of run dicts. Later dirs win per cell."""
    data = {}
    for d in dirs:
        found = {}
        for f in sorted(Path(d).glob("*.json")):
            if f.stem == "experiment-metadata":
                continue
            for key, _ in METHODS:
                prefix = key + "_"
                if not f.stem.startswith(prefix):
                    continue
                rest = f.stem[len(prefix):]
                idx = rest.rfind("_rep")
                if idx < 0:
                    break
                scenario = rest[:idx]
                try:
                    found.setdefault(key, {}).setdefault(scenario, []).append(
                        json.loads(f.read_text()))
                except Exception as e:
                    print(f"  skipping {f.name}: {e}", file=sys.stderr)
                break
        n = sum(len(v) for m in found.values() for v in m.values())
        cells = sum(len(m) for m in found.values())
        print(f"  {Path(d).name}: {n} runs across {cells} cells")
        for method, scen in found.items():
            for label, runs in scen.items():
                data.setdefault(method, {})[label] = runs   # later dir replaces cell
    return data


def series(data, method, labels, metric):
    """Return (means, lo, hi, n) with None for timed-out cells."""
    means, lo, hi, ns = [], [], [], []
    for label in labels:
        runs = data.get(method, {}).get(label, [])
        vals = []
        for r in runs:
            if metric == "aetr":
                v = r.get("aetr")
            else:
                sw = (r.get("switches") or [{}])[0]
                v = sw.get(metric)
            if v is not None:
                vals.append(v)
        # A cell counts as a timeout when no run produced the metric at all.
        if not runs or not vals:
            means.append(None); lo.append(0); hi.append(0); ns.append(0)
        else:
            m = float(np.mean(vals))
            means.append(m)
            lo.append(m - min(vals))
            hi.append(max(vals) - m)
            ns.append(len(vals))
    return means, lo, hi, ns


# ── Drawing ────────────────────────────────────────────────────────────────────

def panel(ax, data, labels, ticks, metric, ylabel, title, integer=True):
    x = np.arange(len(labels))
    width = 0.26
    allv = []
    for i, (key, _) in enumerate(METHODS):
        m, _, hi, _ = series(data, key, labels, metric)
        allv += [v + h for v, h in zip(m, hi) if v is not None]
    ceiling = max(allv) * 1.45 if allv else 1.0

    for i, ((key, _), (color, hatch)) in enumerate(zip(METHODS, STYLE)):
        means, lo, hi, ns = series(data, key, labels, metric)
        offset = (i - 1) * width
        for j, v in enumerate(means):
            xj = x[j] + offset
            if v is None:
                ax.bar(xj, ceiling * 0.045, width=width, color=TO_COLOR,
                       edgecolor="#8C8C8C", linewidth=0.4, hatch="..")
                ax.text(xj, ceiling * 0.065, "T/O", ha="center", va="bottom",
                        fontsize=5.5, color="#595959", rotation=90)
                continue
            ax.bar(xj, v, width=width, color=color, hatch=hatch,
                   edgecolor="white", linewidth=0.5)
            # Whisker only where repetitions actually disagree.
            if (lo[j] + hi[j]) > (0.005 * ceiling):
                ax.errorbar(xj, v, yerr=[[lo[j]], [hi[j]]], fmt="none",
                            ecolor="#404040", elinewidth=0.6, capsize=1.4,
                            capthick=0.6)
            # Rotated: 15 bars per panel leave no room for horizontal labels.
            label = f"{v:.0f}" if integer else f"{v:.2f}"
            ax.text(xj, v + hi[j] + ceiling * 0.02, label, ha="center",
                    va="bottom", fontsize=5.5, color="#333333", rotation=90)

    ax.set_xticks(x)
    ax.set_xticklabels(ticks)
    ax.set_ylabel(ylabel)
    ax.set_title(title, fontweight="bold", pad=3)
    ax.set_ylim(0, ceiling)
    ax.spines["top"].set_visible(False)
    ax.spines["right"].set_visible(False)
    ax.yaxis.grid(True, linestyle="--", linewidth=0.35, alpha=0.55)
    ax.set_axisbelow(True)
    ax.tick_params(width=0.5, length=2)


def figure(data, metric, ylabel, fname, outdir, integer=True):
    fig, axes = plt.subplots(1, 2, figsize=(FULL_W, 2.05))
    panel(axes[0], data, RP_A2B, RP_TICKS, metric, ylabel,
          r"$Track_A \rightarrow Track_B$", integer)
    panel(axes[1], data, RP_B2A, RP_TICKS, metric, ylabel,
          r"$Track_B \rightarrow Track_A$", integer)
    handles = [mpatches.Patch(facecolor=c, hatch=h, edgecolor="white", label=lbl)
               for (_, lbl), (c, h) in zip(METHODS, STYLE)]
    fig.legend(handles=handles, loc="lower center", ncol=3, frameon=False,
               bbox_to_anchor=(0.5, -0.02))
    fig.tight_layout(rect=[0, 0.10, 1, 1])
    path = Path(outdir) / fname
    fig.savefig(path, bbox_inches="tight")
    plt.close(fig)
    print(f"  wrote {path}")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("dirs", nargs="+", help="results directories, later ones win")
    ap.add_argument("--out", default="overleaf-repo/figures", help="output directory")
    args = ap.parse_args()

    for d in args.dirs:
        if not Path(d).is_dir():
            sys.exit(f"not a directory: {d}")
    outdir = Path(args.out)
    outdir.mkdir(parents=True, exist_ok=True)

    print("Loading:")
    data = load_dirs(args.dirs)
    missing = [f"{k}/{s}" for k, _ in METHODS for s in RP_A2B + RP_B2A
               if s not in data.get(k, {})]
    if missing:
        print(f"  WARNING: {len(missing)} cells absent: {', '.join(missing[:6])}"
              + (" ..." if len(missing) > 6 else ""), file=sys.stderr)

    print("Rendering:")
    figure(data, "switch_delay_ms", "Switching delay (ms)",
           "fig_delay_rp.pdf", outdir, integer=True)
    figure(data, "aetr", "AETR",
           "fig_aetr_rp.pdf", outdir, integer=False)


if __name__ == "__main__":
    main()
