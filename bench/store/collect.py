#!/usr/bin/env python3
"""Summarize one lab-run.sh variant: peak/final disk, wall time, cache hit rates.

usage: collect.py <lab-dir> <variant>

Disk is the used space of the isolated APFS volume the lab runs on, sampled
every 2 s, relative to the volume just before the lanes started. Hit rates come
from the run summaries justrust recorded under <lab-dir>/jh-<variant>/runs.
"""
import json, sys
from pathlib import Path

lab, variant = Path(sys.argv[1]), sys.argv[2]
v = lab / variant
meta = dict(l.strip().split("=", 1) for l in (v / "meta.txt").read_text().splitlines() if "=" in l)
base = int(meta["base_used_mb"])
samples = [int(l.split()[1]) for l in (v / "df.log").read_text().splitlines() if l.strip()]
steps = []
for l in (v / "steps.log").read_text().splitlines():
    if l.startswith("STEP"):
        steps.append(dict(kv.split("=") for kv in l.split()[1:]))
runs = list((lab / f"jh-{variant}" / "runs").glob("*/summary.json"))
dep_h = dep_m = loc_h = loc_m = 0
for p in runs:
    u = json.loads(p.read_text()).get("units", {})
    dep_h += u.get("depcache_hits", 0); dep_m += u.get("depcache_misses", 0)
    loc_h += u.get("localcache_hits", 0); loc_m += u.get("localcache_misses", 0)
rate = lambda h, m: f"{100 * h / (h + m):.1f}%" if h + m else "n/a"
fails = [s for s in steps if s["rc"] != "0"]
print(json.dumps({
    "variant": variant,
    "wall_secs": int(meta["wall"]),
    "peak_disk_gb": round((max(samples) - base) / 1024, 2),
    "final_disk_gb": round((int(meta["end_used_mb"]) - base) / 1024, 2),
    "steps": len(steps), "failed_steps": len(fails),
    "depcache_hit_rate": rate(dep_h, dep_m), "depcache": [dep_h, dep_m],
    "localcache_hit_rate": rate(loc_h, loc_m), "localcache": [loc_h, loc_m],
    "step_secs": {s["step"] + "@" + s["lane"]: int(s["secs"]) for s in steps},
}, indent=1))
