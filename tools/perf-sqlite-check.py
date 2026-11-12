#!/usr/bin/env python3
"""Focused SQLite row-scale heap gate using an explicitly supplied probe-heap binary.

Writes new synthetic sources and scratch catalogs only. Holds row payload size near128KiB,
so increasing logical session size measures row/navigation retention rather than giant rows.
Optional baseline checks preserved-order message and metadata hashes in separate processes.
Native RSS/footprint and physical temporary I/O are NOT Rust heap measurements.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
from perf_fixture_sqlite import generate_sqlite

CASES = (("devin", "history"), ("devin", "metadata"), ("devin", "unselected"),
         ("hermes", "history"), ("hermes", "prompt-only"), ("goose", "history"),
         ("opencode", "history"), ("cursor", "history"),
         ("cursor", "inline"), ("cursor", "legacy"))


def checked_peak(result):
    if result.get("build_features") != {"mmap": True, "probe_heap": True}:
        raise ValueError("--probe must be a native mmap binary built with --features probe-heap")
    heap = result.get("requested_rust_heap")
    peak = heap.get("peak_above_baseline_bytes") if isinstance(heap, dict) else None
    if type(peak) is not int or peak < 0:
        raise ValueError("valid live Rust heap counters are required")
    return peak


def campaign(args, root):
    scratch = root / "catalog"
    scratch.mkdir()
    temporary = root / "temporary"
    temporary.mkdir()
    env = dict(os.environ, CLUSTERVISION_HOME=str(scratch), TMPDIR=str(temporary))
    env.pop("CLUSTERVISION_CACHE", None)
    results = []
    failed = []
    for harness, shape in CASES:
        if args.format and harness not in args.format:
            continue
        for size in args.sizes:
            destination = root / f"{harness}-{shape}-{size}"
            destination.mkdir()
            records = max(1, (size + 127999) // 128000)
            manifest = generate_sqlite(harness, destination, size, records, args.escaped, shape)
            (destination / "PERF-FIXTURE.json").write_text(json.dumps(manifest, indent=2) + "\n")
            common = [harness, manifest["id"], manifest["path"], "--root", manifest["root"], "--mode", "lazy"]
            def run(binary, operation, extra=()):
                result = subprocess.run([str(binary.resolve()), operation, *common, *extra], env=env,
                                        text=True, capture_output=True, check=True)
                return json.loads(result.stdout)
            result = run(args.probe, "discard")
            peak = checked_peak(result)
            # Allows compact Devin forest and Cursor header descriptors to grow by row count.
            # Payloads stay ~128KiB/row; 2KiB/row cannot conceal whole-payload retention.
            # Does not allow payload-size memory growth (old32MB Devin/Hermes fail decisively).
            limit = 2 * 1024 * 1024 + records * 2048
            leftover = list(temporary.iterdir())
            entry = {"fixture": manifest, "probe": result, "heap_limit_bytes": limit,
                     "temporary_files_after": [str(path) for path in leftover]}
            if result["messages"] != manifest["expected_messages"] or peak > limit or leftover:
                failed.append(f"{harness}/{shape}/{size}")
            if args.baseline:
                before = run(args.baseline, "hash", ["--raw-order"])
                after = run(args.probe, "hash", ["--raw-order"])
                entry["baseline_hash"] = before["detail"]
                entry["candidate_hash"] = after["detail"]
                if before["detail"] != after["detail"]:
                    failed.append(f"{harness}/{shape}/{size}: fidelity")
            results.append(entry)
            print(f"{harness}/{shape}: {size} logical payload bytes, {records} rows, {peak} peak requested Rust bytes", flush=True)
    (root / "SQLITE-SCALE-REPORT.json").write_text(json.dumps({"results": results, "failures": failed}, indent=2) + "\n")
    if failed:
        raise SystemExit("SQLite scale gate failed: " + ", ".join(failed))


def hermes_giant(args, root):
    """One demanded ordinary TEXT cell per role: native reads and replay dedup stay bounded."""
    catalog = root / "catalog"
    catalog.mkdir()
    temporary = root / "temporary"
    temporary.mkdir()
    env = dict(os.environ, CLUSTERVISION_HOME=str(catalog), TMPDIR=str(temporary))
    env.pop("CLUSTERVISION_CACHE", None)
    results = []
    # User rows exercise exact replay dedup; assistant-only cells cannot expose key ownership.
    for shape in ("history", "prompt-only"):
        for size in args.sizes:
            fixture = root / f"hermes-giant-{shape}-{size}"
            fixture.mkdir()
            manifest = generate_sqlite("hermes", fixture, size, 1, args.escaped, shape)
            common = ["hermes", manifest["id"], manifest["path"], "--root", manifest["root"], "--sqlite-heap"]
            probes = {}
            for operation, mode in (("collect", "lazy"), ("doctor", "facts"), ("doctor", "full")):
                child = subprocess.run([str(args.probe.resolve()), operation, *common, "--mode", mode],
                                       env=env, text=True, capture_output=True, check=True)
                row = json.loads(child.stdout)
                peak = checked_peak(row)
                assert row["messages"] == manifest["expected_messages"] == 1, "giant body was skipped"
                native = row["sqlite_native_heap"]
                assert native["accounting_enabled_and_verified"], "native SQLite accounting is inactive"
                assert 0 < native["peak_above_baseline_bytes"] <= 8 * 1024 * 1024, "native giant-cell read retained payload"
                assert 0 < native["largest_allocation_request_bytes"] <= 1024 * 1024, "SQLite materialized the giant cell"
                if mode != "full":
                    assert peak <= 2 * 1024 * 1024, "lean Rust body ownership grew with payload"
                probes[f"{operation}-{mode}"] = row
                print(f"hermes/giant/{shape}/{mode}: {size} payload bytes, {peak} requested Rust peak, {native['peak_above_baseline_bytes']} native peak", flush=True)
            assert probes["doctor-facts"]["detail"] == probes["doctor-full"]["detail"], "Facts doctor changed the demanded report"
            assert not list(temporary.iterdir()), "incremental read leaked temporary files"
            results.append({"fixture": manifest, "probes": probes})
    (root / "HERMES-GIANT-REPORT.json").write_text(json.dumps({"results": results}, indent=2) + "\n")


def giant_discovery(args, root):
    """Exercise actual adapter discovery, including native SQLite cell allocation."""
    catalog = root / "catalog"
    catalog.mkdir()
    temporary = root / "temporary"
    temporary.mkdir()
    env = dict(os.environ, CLUSTERVISION_HOME=str(catalog), TMPDIR=str(temporary))
    env.pop("CLUSTERVISION_CACHE", None)
    results = []
    cases = (("devin", "history"), ("devin", "metadata"), ("goose", "history"),
             ("hermes", "history"), ("hermes", "prompt-only"))
    for harness, shape in cases:
        if args.format and harness not in args.format:
            continue
        for size in args.sizes:
            fixture = root / f"{harness}-discovery-{shape}-{size}"
            fixture.mkdir()
            manifest = generate_sqlite(harness, fixture, size, 1, args.escaped, shape)
            child = subprocess.run([str(args.probe.resolve()), "discover", harness,
                                    manifest["id"], manifest["path"], "--root", manifest["root"],
                                    "--sqlite-heap"], env=env, text=True, capture_output=True, check=True)
            row = json.loads(child.stdout)
            peak = checked_peak(row)
            assert row["detail"]["discovered_sessions"] == 1, "giant session was skipped"
            # Counting is free for Devin (a row count over the message forest), so discovery
            # reports it. Goose and Hermes would have to decode every message body to count, so
            # discovery reports "not counted" and the native-heap check below proves the giant
            # cell was never decoded; a full read or `cv index` certifies the number later.
            if harness == "devin":
                assert row["detail"]["discovered_turns"] == 1, "giant conversational turn was not counted"
                assert row["detail"]["uncounted_sessions"] == 0, "devin discovery must count"
            else:
                assert row["detail"]["discovered_turns"] == 0, f"{harness} discovery decoded the giant turn to count it"
                assert row["detail"]["uncounted_sessions"] == 1, f"{harness} discovery must report the session as not counted"
            native = row["sqlite_native_heap"]
            assert native["accounting_enabled_and_verified"], "native SQLite accounting is inactive"
            assert 0 < native["peak_above_baseline_bytes"] <= 8 * 1024 * 1024, "discovery retained a native giant cell"
            assert 0 < native["largest_allocation_request_bytes"] <= 1024 * 1024, "SQLite materialized a giant discovery cell"
            assert peak <= 2 * 1024 * 1024, "discovery Rust ownership grew with payload"
            assert not list(temporary.iterdir()), "discovery leaked temporary files"
            results.append({"fixture": manifest, "probe": row})
            print(f"{harness}/discovery/{shape}: {size} payload bytes, {peak} requested Rust peak, {native['peak_above_baseline_bytes']} native peak", flush=True)
    (root / "GIANT-DISCOVERY-REPORT.json").write_text(json.dumps({"results": results}, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--baseline", type=Path)
    parser.add_argument("--out", type=Path, help="retain generated fixtures/report; otherwise all inputs are operation-owned temporary files")
    parser.add_argument("--sizes", type=int, nargs="+", default=[8_000_000, 32_000_000])
    parser.add_argument("--format", action="append", choices=("devin", "hermes", "goose", "opencode", "cursor"))
    parser.add_argument("--escaped", action="store_true", help="use escaped payloads for a focused decoder scale check")
    modes = parser.add_mutually_exclusive_group()
    modes.add_argument("--giant-discovery", action="store_true", help="check actual giant-cell discovery counts and Rust/native SQLite memory")
    modes.add_argument("--hermes-giant", action="store_true", help="check giant assistant and user Hermes cells through lazy collection and Facts/full doctor, including native SQLite allocation")
    args = parser.parse_args()
    if len(args.sizes) < 2 or any(size <= 0 for size in args.sizes):
        parser.error("at least two positive sizes are required")
    run = giant_discovery if args.giant_discovery else hermes_giant if args.hermes_giant else campaign
    if args.out:
        args.out.mkdir(parents=True, exist_ok=False)
        run(args, args.out.resolve())
    else:
        with tempfile.TemporaryDirectory(prefix="cv-sqlite-budget-") as temporary:
            run(args, Path(temporary))


if __name__ == "__main__":
    main()
