#!/usr/bin/env python3
"""Replay recorded events through the asymmetric IIR algorithm.

Reads NDJSON produced by raw_export, applies the same pipeline as the
Rust replay binary (per-tick event feeding -> asymmetric IIR with rhythm
detection), and writes per-tick NDJSON output for comparison.
"""

import argparse
import json
import sys

from analysis import load_events, compute_iir_fps


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("-i", "--input", required=True, help="NDJSON file from raw_export")
    p.add_argument("-o", "--output", help="Output NDJSON (default: stdout)")
    p.add_argument("--attack-half-life-ms", type=float, default=10.0)
    p.add_argument("--release-half-life-ms", type=float, default=100.0)
    p.add_argument("--min-fps", type=float, default=45.0)
    p.add_argument("--max-fps", type=float, default=115.0)
    p.add_argument("--tick-us", type=int, default=1000, help="Resample grid interval in us")
    args = p.parse_args()

    attack_hl_us = args.attack_half_life_ms * 1000.0
    release_hl_us = args.release_half_life_ms * 1000.0

    good = load_events(args.input)
    result = compute_iir_fps(
        good.lazy(), attack_hl_us, release_hl_us,
        args.min_fps, args.max_fps, args.tick_us,
    )

    out = open(args.output, "w") if args.output else sys.stdout
    try:
        for row in result.iter_rows(named=True):
            json.dump(
                {
                    "time_us": row["time_us"],
                    "fps_limit": row["fps_limit"],
                    "norm": row["norm"],
                    "y": row["y"],
                    "expected": row["expected"],
                    "mean_delta": row["mean_delta"],
                    "events": row["events"],
                },
                out,
            )
            out.write("\n")
    except BrokenPipeError:
        return

    if args.output:
        out.close()


if __name__ == "__main__":
    main()
