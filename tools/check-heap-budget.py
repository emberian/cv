#!/usr/bin/env python3
"""Process-isolated live Rust heap gates; no timing thresholds.

The single OpenCode shell control also checks separately instrumented native SQLite allocations.

Run with a release store_probe built with probe-heap. The second binary must also disable mmap.
Only generated scratch inputs are read, and every subprocess uses an isolated cv catalog.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sqlite3
import shutil
import sys
import tempfile


MIB = 1 << 20
SIZES = (16 * MIB, 64 * MIB)
STORE_CEILING = 8 * MIB
STORE_GROWTH = 2 * MIB
CHUNK_CEILING = 2 * MIB
CHUNK_GROWTH = MIB // 2
CHUNK_BYTES = 64 << 10


def probe(binary, args, env, mmap, fd_limit=None):
    def limit_files():
        import resource
        _, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
        resource.setrlimit(resource.RLIMIT_NOFILE,
                           (fd_limit if hard == resource.RLIM_INFINITY else min(fd_limit, hard), hard))
    result = subprocess.run([str(binary), *map(str, args)], env=env, text=True, capture_output=True,
                            preexec_fn=limit_files if fd_limit is not None else None)
    if result.returncode:
        raise AssertionError(f"probe failed ({result.returncode}): {result.stderr[-2000:]}")
    try:
        row = json.loads(result.stdout)
    except json.JSONDecodeError as error:
        raise AssertionError(f"probe did not return JSON: {result.stdout[-1000:]}") from error
    assert row.get("build_features") == {"mmap": mmap, "probe_heap": True}, "wrong probe features; heap instrumentation is required"
    heap = row.get("requested_rust_heap")
    assert isinstance(heap, dict), "probe-heap counters missing; a clean timing binary cannot pass this gate"
    peak = heap.get("peak_above_baseline_bytes")
    assert type(peak) is int and peak >= 0, "invalid live Rust heap counter"
    return row, peak


def bound(peaks, ceiling, growth, case):
    assert max(peaks) <= ceiling, f"{case}: live heap {peaks} exceeds {ceiling} bytes"
    assert peaks[1] <= peaks[0] + growth, f"{case}: heap grows with source size: {peaks} (allowance {growth})"


def stores(binary, root, env):
    generator = Path(__file__).with_name("perf-fixture.py")
    # Store promises here are for mmap builds. The no-mmap guarantee below applies to the
    # positioned-read chunk resolver, not adapters that must own giant source records.
    # Most of a real Codex fleet is .jsonl.zst: the same rollouts, compressed, take the decoded path.
    for harness, shape in (("gemini", "giant"), ("claude", "fixed"), ("claude", "giant"), ("codex", "fixed"), ("codex", "giant"),
                           ("codex-zst", "fixed"), ("codex-zst", "giant")):
        compressed = harness == "codex-zst"
        harness = "codex" if compressed else harness
        for escaped in (False, True):
            peaks = []
            for size in SIZES:
                # Fixed records refute whole-history retention; giant records refute temporary
                # field/line ownership (including detector passes) in mmap-backed lazy parsing.
                records = 4 if shape == "giant" else size // (256 << 10)
                out = root / f"{harness}{'-zst' if compressed else ''}-{shape}-{size}-{escaped}"
                command = [sys.executable, str(generator), harness, str(out), "--payload-bytes", str(size), "--records", str(records)]
                if escaped:
                    command.append("--escaped")
                manifest = json.loads(subprocess.check_output(command, env=env, text=True))
                if compressed:
                    subprocess.check_call(["zstd", "-q", "--rm", manifest["path"], "-o", manifest["path"] + ".zst"])
                    manifest["path"] += ".zst"
                    manifest["physical_input_bytes"] = os.path.getsize(manifest["path"])
                row, peak = probe(binary, ["count", harness, manifest["id"], manifest["path"], "--root", manifest["root"], "--mode", "lazy"], env, True)
                assert row["messages"] == manifest["expected_messages"], f"{harness}: truncated or skipped source"
                peaks.append(peak)
                print(json.dumps({"case": f"{harness}{'-zst' if compressed else ''}-{shape}-{'escaped' if escaped else 'plain'}", "decoded_payload_bytes": size,
                                  "physical_input_bytes": manifest["physical_input_bytes"], "messages": row["messages"], "peak_live_rust_bytes": peak}), flush=True)
            bound(peaks, STORE_CEILING, STORE_GROWTH, f"{harness} shape={shape} escaped={escaped} compressed={compressed}")


def active_formats(binary, root, env, baseline=None, formats=None, mmap=True):
    """Bound discarded history/unrelated bodies; selected giant output is deliberately excluded."""
    generator = Path(__file__).with_name("perf-fixture.py")
    cases = (("cline-sdk", "history"), ("opensession", "history"),
             ("chatgpt-export", "unrelated"), ("claude-export", "unrelated"),
             ("kimi", "sidecar-unmatched"))
    # Compression generation needs zstd on PATH; explicitly opt in to this native-codec case.
    if formats and "zed" in formats:
        cases += (("zed", "history"),)
    for format_name, shape in cases:
        if formats and format_name not in formats:
            continue
        for escaped in (False, True):
            peaks = {"discard": [], "discover": []}
            for size in SIZES:
                command = [sys.executable, str(generator), "cursor" if format_name == "cursor-ide" else format_name,
                           str(root / f"{format_name}-{shape}-{size}-{escaped}"),
                           "--payload-bytes", str(size), "--records", str(size // (256 << 10)), "--shape", shape]
                if escaped:
                    command.append("--escaped")
                if format_name == "zed":
                    command.append("--entropy")
                manifest = json.loads(subprocess.check_output(command, env=env, text=True))
                common = [manifest["harness"], manifest["id"], manifest["path"], "--root", manifest["root"]]
                for operation in peaks:
                    # A fresh discovery catalog prevents a cache hit from hiding a growing scan.
                    catalog = root / f"catalog-{format_name}-{size}-{escaped}-{operation}"
                    catalog.mkdir()
                    case_env = dict(env, CLUSTERVISION_HOME=str(catalog))
                    row, peak = probe(binary, [operation, *common], case_env, mmap)
                    if operation == "discard":
                        assert row["messages"] == manifest["expected_messages"], "truncated or skipped source"
                    else:
                        assert row["detail"]["discovered_sessions"] == manifest["expected_discovered_sessions"], "missing discovered session"
                    peaks[operation].append(peak)
                    print(json.dumps({"case": f"{format_name}-{shape}-{operation}-{'escaped' if escaped else 'plain'}",
                                      "backend": "mmap" if mmap else "positioned",
                                      "decoded_payload_bytes": size, "selected_payload_bytes": manifest["selected_payload_bytes"],
                                      "physical_input_bytes": manifest["physical_input_bytes"], "messages": row["messages"],
                                      "decoded_document_bytes": manifest.get("decoded_document_bytes"),
                                      "compressed_blob_bytes": manifest.get("compressed_blob_bytes"),
                                      "peak_live_rust_bytes": peak}), flush=True)
            for operation, operation_peaks in peaks.items():
                bound(operation_peaks, STORE_CEILING if mmap else 2 * MIB,
                      STORE_GROWTH if mmap else MIB // 4,
                      f"{format_name} {shape} {operation} mmap={mmap} escaped={escaped}")
            if baseline:
                # Fidelity runs remain separate from allocation probes and use small payloads.
                command = [sys.executable, str(generator), "cursor" if format_name == "cursor-ide" else format_name,
                           str(root / f"parity-{format_name}-{escaped}"), "--payload-bytes", str(64 << 10),
                           "--records", "4", "--shape", shape]
                if escaped:
                    command.append("--escaped")
                if format_name == "zed":
                    command.append("--entropy")
                manifest = json.loads(subprocess.check_output(command, env=env, text=True))
                common = [manifest["harness"], manifest["id"], manifest["path"], "--root", manifest["root"]]
                for mode in ("lazy", "full"):
                    left, _ = probe(baseline, ["hash", *common, "--mode", mode], env, True)
                    right, _ = probe(binary, ["hash", *common, "--mode", mode], env, mmap)
                    assert left["messages"] == right["messages"] == manifest["expected_messages"], "fidelity count differs"
                    assert left["detail"] == right["detail"], f"{format_name}: resolved content or metadata changed ({mode})"
                refs = []
                for label, comparison_binary in (("baseline", baseline), ("candidate", binary)):
                    catalog = root / f"parity-catalog-{format_name}-{escaped}-{label}"
                    catalog.mkdir()
                    row, _ = probe(comparison_binary, ["discover", *common, "--hash-output"],
                                   dict(env, CLUSTERVISION_HOME=str(catalog)), True if label == "baseline" else mmap)
                    refs.append(row["detail"])
                assert refs[0] == refs[1], f"{format_name}: discovered metadata changed"


def doctors(binary, root, env, baseline=None):
    """Doctor counts decoded content but does not demand an owned transcript/output body."""
    generator = Path(__file__).with_name("perf-fixture.py")
    for harness in ("claude", "codex", "gemini"):
        for escaped in (False, True):
            peaks = []
            for size in SIZES:
                command = [sys.executable, str(generator), harness,
                           str(root / f"doctor-{harness}-{size}-{escaped}"),
                           "--payload-bytes", str(size), "--records", "4"]
                if escaped:
                    command.append("--escaped")
                manifest = json.loads(subprocess.check_output(command, env=env, text=True))
                common = [harness, manifest["id"], manifest["path"], "--root", manifest["root"]]
                row, peak = probe(binary, ["doctor", *common, "--mode", "facts"], env, True)
                assert row["messages"] == manifest["expected_messages"], "doctor skipped or truncated source"
                peaks.append(peak)
                # Full-mode reference is a separate process and outside the bounded consumer claim.
                reference, _ = probe(baseline or binary, ["doctor", *common, "--mode", "full"], env, True)
                assert reference["messages"] == row["messages"] and reference["detail"] == row["detail"], "doctor report changed"
                print(json.dumps({"case": f"doctor-{harness}-{'escaped' if escaped else 'plain'}",
                                  "decoded_payload_bytes": size, "physical_input_bytes": manifest["physical_input_bytes"],
                                  "messages": row["messages"], "peak_live_rust_bytes": peak,
                                  "full_reference_peak_live_rust_bytes": reference["requested_rust_heap"]["peak_above_baseline_bytes"]}), flush=True)
            bound(peaks, STORE_CEILING, STORE_GROWTH, f"doctor {harness} escaped={escaped}")

    metadata_facts(binary, root, env, baseline)
    opencode_shell_doctor(binary, root, env)


SHELL_DOCTOR_CONVERSATIONAL = {
    "total": 13421804, "tool_results": 0, "thinking": 0, "assistant_text": 0,
    "user_text": 0, "tool_call_args": 0, "images": 0, "system_msgs": 0,
    "system_reminders": 13421804,
}

SHELL_DOCTOR_EXPECTED = {
    "sessions": 1, "messages": 1, "compactions": 0, "auto_compactions": 0,
    "pre_tokens": [], "fixed_overhead_est": 0, "peak_context": 0,
    "conversational": SHELL_DOCTOR_CONVERSATIONAL,
    "by_attachment": {"injected_context": 13421804},
    "tool_results_mcp": 0, "tool_results_builtin": 0, "by_tool": [],
    "active_window": {
        "from_compaction": False, "compacted_sessions": 0, "messages": 1, "turns": 1,
        "current_context": None, "peak_context": 0, "first_prompt_tokens": None,
        "reconstructed_visible_turn_tokens": 0, "system_and_tools_overhead": None,
        "token_burn_per_turn": 13421804,
        "conversational": {"compaction_summary": 0, **SHELL_DOCTOR_CONVERSATIONAL},
        "by_attachment": {"injected_context": 13421804},
        "tool_results_mcp": 0, "tool_results_builtin": 0, "by_tool": [],
    },
    "history": {
        "messages": 1, "compactions": 0, "auto_compactions": 0,
        "pre_tokens": [], "fixed_overhead_est": 0, "peak_context": 0,
        "conversational": SHELL_DOCTOR_CONVERSATIONAL,
        "by_attachment": {"injected_context": 13421804},
        "tool_results_mcp": 0, "tool_results_builtin": 0, "by_tool": [],
    },
}


def check_shell_doctor(row, peak):
    assert row["messages"] == 1 and row["detail"] == SHELL_DOCTOR_EXPECTED, "shell doctor report changed"
    assert 0 < peak <= 4 * MIB, f"shell doctor live Rust heap {peak} exceeds {4 * MIB} bytes"
    for key in ("allocation_requests", "requested_bytes"):
        assert type(row["requested_rust_heap"].get(key)) is int and row["requested_rust_heap"][key] > 0, "shell heap instrumentation inactive"
    native = row["sqlite_native_heap"]
    assert native["accounting_enabled_and_verified"], "native SQLite accounting inactive"
    assert 0 < native["peak_above_baseline_bytes"] <= 8 * MIB, "shell native SQLite heap exceeds 8 MiB"
    assert 0 < native["largest_allocation_request_bytes"] <= MIB, "shell fetched a whole large SQLite cell"


def opencode_shell_doctor(binary, root, env):
    """One ordinary native v2 shell body; no selected-history or producer matrix."""
    generator = Path(__file__).with_name("perf-fixture.py")
    manifest = json.loads(subprocess.check_output([
        sys.executable, str(generator), "opencode", str(root / "opencode-shell"),
        "--shape", "shell-v2", "--payload-bytes", str(32 * MIB), "--records", "1"], env=env, text=True))
    source = Path(manifest["path"])
    def fingerprint():
        digest = hashlib.sha256()
        with source.open("rb") as data:
            for chunk in iter(lambda: data.read(CHUNK_BYTES), b""):
                digest.update(chunk)
        return digest.hexdigest()
    before = fingerprint()
    row, peak = probe(binary, ["doctor", "opencode", manifest["id"], source,
                              "--root", manifest["root"], "--mode", "facts", "--sqlite-heap"], env, True)
    check_shell_doctor(row, peak)
    assert fingerprint() == before, "shell doctor modified its source database"
    print(json.dumps(dict(case="doctor-opencode-v2-shell", decoded_payload_bytes=32 * MIB,
                          messages=1, peak_live_rust_bytes=peak,
                          peak_live_sqlite_bytes=row["sqlite_native_heap"]["peak_above_baseline_bytes"])), flush=True)
    shutil.rmtree(manifest["root"])


def metadata_facts(binary, root, env, baseline=None, harnesses=None):
    """Actual doctor reducers must not decode unrelated rich sidecars to keep tiny facts.

    Full report references run in separate processes; their allocation is intentionally outside
    the facts budget. Both sizes hold record count and all demanded content/facts constant.
    """
    generator = Path(__file__).with_name("perf-fixture.py")
    for harness in harnesses or ("claude", "cursor", "goose"):
        for escaped in (False, True):
            peaks = []
            reports = []
            for size in SIZES:
                command = [sys.executable, str(generator), harness,
                           str(root / f"facts-{harness}-{size}-{escaped}"),
                           "--payload-bytes", str(size), "--records", "4", "--shape", "metadata"]
                if escaped:
                    command.append("--escaped")
                manifest = json.loads(subprocess.check_output(command, env=env, text=True))
                common = [manifest["harness"], manifest["id"], manifest["path"], "--root", manifest["root"]]
                row, peak = probe(binary, ["doctor", *common, "--mode", "facts"], env, True)
                reference, reference_peak = probe(baseline or binary, ["doctor", *common, "--mode", "full"], env, True)
                assert row["messages"] == reference["messages"] == manifest["expected_messages"], "facts doctor skipped source"
                assert row["detail"] == reference["detail"], "facts doctor report changed from full metadata"
                assert row["detail"]["compactions"] == manifest["expected_compactions"], "facts lost compaction metadata"
                assert row["detail"]["conversational"]["tool_results"] > 0, "fixture did not exercise tool attribution"
                reports.append(row["detail"])
                peaks.append(peak)
                print(json.dumps({"case": f"doctor-facts-{manifest.get('format', harness)}-{'escaped' if escaped else 'plain'}",
                                  "decoded_metadata_payload_bytes": manifest["decoded_metadata_payload_bytes"],
                                  "physical_input_bytes": manifest["physical_input_bytes"],
                                  "messages": row["messages"], "peak_live_rust_bytes": peak,
                                  "full_reference_peak_live_rust_bytes": reference_peak}), flush=True)
                shutil.rmtree(manifest["root"])
            assert reports[0] == reports[1], "unused metadata changed the demanded doctor report"
            bound(peaks, MIB, MIB // 4, f"doctor metadata facts {harness} escaped={escaped}")


# Full/rich ownership is a reference, not a bounded-memory promise. Native SQLite
# allocations are outside these live Rust counters; file/block indexes may grow.
SELECTED_PAYLOAD_CASES = (("kimi-code", "selected"), ("opencode-files", "selected"),
                          ("opencode", "selected"), ("opencode", "selected-v2"),
                          ("cursor", "selected"), ("cursor-ide", "selected-ide"), ("goose", "selected"),
                          ("codex", "selected-response"), ("codex", "selected-events"), ("gemini", "selected"), ("gemini", "selected-jsonl"))


def selected_payloads(binary, root, env, formats=None, operations=None, mmap=True):
    generator = Path(__file__).with_name("perf-fixture.py")
    for format_name, shape in SELECTED_PAYLOAD_CASES:
        if formats and format_name not in formats:
            continue
        for history in ((False,) if format_name in ("codex", "gemini") else (False, True)):
            for escaped in (False, True):
                peaks = {operation: [] for operation in (operations or ("discard", "doctor", "dataset", "dataset-miss"))}
                for size in SIZES:
                    records = size // (256 << 10) if history else 1
                    command = [sys.executable, str(generator), "cursor" if format_name == "cursor-ide" else format_name,
                               str(root / f"payload-{format_name}-{shape}-{history}-{size}-{escaped}"),
                               "--payload-bytes", str(size), "--records", str(records), "--shape", shape]
                    if escaped:
                        command.append("--escaped")
                    manifest = json.loads(subprocess.check_output(command, env=env, text=True))
                    common = [manifest["harness"], manifest["id"], manifest["path"], "--root", manifest["root"]]
                    reference = None
                    if "doctor" in peaks:
                        reference, _ = probe(binary, ["doctor", *common, "--mode", "full"], env, mmap, fd_limit=64)
                    dataset_reference = None
                    if "dataset" in peaks:
                        dataset_reference, _ = probe(binary, ["dataset-collect", *common, "--mode", "full",
                                                               "--query", "tool:gate-final", "--hash-output"], env, mmap, fd_limit=64)
                    for operation in peaks:
                        if operation == "discard":
                            arguments = ["discard", *common, "--mode", "lazy"]
                        elif operation == "doctor":
                            arguments = ["doctor", *common, "--mode", "facts"]
                        else:
                            query = "tool:gate-final" if operation == "dataset" else "tool:gate-missing"
                            arguments = ["dataset", *common, "--mode", "lazy", "--query", query, "--hash-output"]
                        row, peak = probe(binary, arguments, env, mmap, fd_limit=64)
                        if operation in ("discard", "doctor"):
                            assert row["messages"] == manifest["expected_messages"], "selected payload messages skipped"
                        if operation == "doctor":
                            assert row["detail"] == reference["detail"], "selected facts doctor differs from full report"
                        elif operation == "dataset":
                            for key in ("record_written", "serialized_record_bytes", "record_sha256"):
                                assert row["detail"][key] == dataset_reference["detail"][key], "selected dataset differs from full output"
                            assert row["detail"]["record_written"], "late tool sentinel did not match"
                        elif operation == "dataset-miss":
                            assert not row["detail"]["record_written"], "absent tool query matched"
                            assert row["detail"]["serialized_record_bytes"] == 0
                            assert row["detail"]["record_sha256"] == hashlib.sha256(b"").hexdigest()
                        peaks[operation].append(peak)
                        print(json.dumps({"case": f"selected-{format_name}-{shape}-{operation}",
                                          "history": history, "escaped": escaped, "mmap": mmap, "open_file_limit": 64, "decoded_payload_bytes": size,
                                          "records": records, "peak_live_rust_bytes": peak}), flush=True)
                    shutil.rmtree(manifest["root"])
                for operation, values in peaks.items():
                    bound(values, STORE_CEILING, STORE_GROWTH,
                          f"selected {format_name}/{shape} {operation} history={history} escaped={escaped}")


def queries(binary, root, env):
    """Exercise retained rich histories, while leaving giant argument obligations explicit."""
    generator = Path(__file__).with_name("perf-fixture.py")
    for harness in ("claude", "codex"):
        for escaped in (False, True):
            peaks = []
            for size in SIZES:
                records = size // (256 << 10)
                command = [sys.executable, str(generator), harness,
                           str(root / f"query-{harness}-{size}-{escaped}"),
                           "--payload-bytes", str(size), "--records", str(records), "--shape", "tool-arguments"]
                if escaped:
                    command.append("--escaped")
                manifest = json.loads(subprocess.check_output(command, env=env, text=True))
                # Demand the final record's unique target, refuting an early successful stop.
                expression = f"tool:edit AND touched:file-{records - 1}.rs AND model:test"
                common = [harness, manifest["id"], manifest["path"], "--root", manifest["root"], "--query", expression]
                row, peak = probe(binary, ["query", *common], env, True)
                reference, collected_peak = probe(binary, ["query-collect", *common], env, True)
                assert row["detail"] == reference["detail"] and row["detail"]["matches"], "query result changed or final record was skipped"
                peaks.append(peak)
                print(json.dumps({"case": f"query-{harness}-{'escaped' if escaped else 'plain'}",
                                  "decoded_argument_bytes": size, "records": records,
                                  "physical_input_bytes": manifest["physical_input_bytes"],
                                  "peak_live_rust_bytes": peak, "collected_peak_live_rust_bytes": collected_peak,
                                  "matches": row["detail"]["matches"]}), flush=True)
            bound(peaks, STORE_CEILING, STORE_GROWTH, f"query {harness} escaped={escaped}")


EVENT_SHAPES = ("unfinished", "open-close", "native")


def event_fixture(source, shape, records):
    """Unfinished calls, a spilled completion set followed by reuse, and native reconciliation."""
    expected = []
    messages = 0
    with source.open("w") as out:
        def emit(row):
            nonlocal messages
            out.write(json.dumps(row, separators=(",", ":")) + "\n")
            messages += 1

        def claude(block, role="assistant"):
            row = {"type": role, "sessionId": "events", "uuid": f"event-{messages}",
                   "message": {"role": role, "content": [block]}}
            if messages:
                row["parentUuid"] = f"event-{messages - 1}"
            emit(row)

        def call(i, call_id):
            expected.append((messages, "file_edit", f"/repo/file-{i}.rs"))
            claude({"type": "tool_use", "id": call_id, "name": "Edit",
                    "input": {"file_path": f"/repo/file-{i}.rs", "old_string": "a", "new_string": "b"}})

        if shape == "native":
            for i in range(records):
                path, extra, call_id = f"/repo/file-{i}.rs", f"/repo/native-{i}.rs", f"call-{i}"
                expected.append((messages, "file_edit", path))
                emit({"type": "response_item", "payload": {"type": "function_call", "name": "apply_patch",
                      "call_id": call_id, "arguments": json.dumps({"patch": f"*** Begin Patch\n*** Update File: {path}\n@@\n-a\n+b\n*** End Patch"})}})
                expected.append((messages, "file_edit", extra))
                emit({"type": "event_msg", "payload": {"type": "item_completed", "item": {
                      "type": "FileChange", "id": call_id, "status": "completed", "changes": {
                          path: {"type": "update", "unified_diff": "@@\n-a\n+b\n"},
                          extra: {"type": "add", "unified_diff": "+new\n"}}}}})
                emit({"type": "response_item", "payload": {"type": "function_call_output",
                      "call_id": call_id, "output": "Success"}})
        else:
            for i in range(records):
                call(i, f"call-{i}")
            if shape == "open-close":
                for i in reversed(range(records)):
                    claude({"type": "tool_result", "tool_use_id": f"call-{i}", "content": "ok"}, "user")
                # Reuse a closed ID after clearing both correlation sets.
                call(records, "call-0")
    return "codex" if shape == "native" else "claude", messages, expected


def events(binary, root, env, *, mmap=True):
    """Bound extraction/publication for each correlation shape in both byte-source builds."""
    for shape in EVENT_SHAPES:
        case = root / shape
        case.mkdir()
        events_case(binary, case, env, shape, mmap=mmap)


def events_case(binary, root, env, shape, *, mmap):
    peaks = {"events": [], "events-collected": []}
    for records in (20_000, 200_000):
        fixture = root / f"events-{records}"
        fixture.mkdir()
        source = fixture / "events.jsonl"
        harness, messages, expected = event_fixture(source, shape, records)
        digests = []
        for operation in peaks:
            catalog = root / f"catalog-{operation}-{records}"
            temporary = root / f"spools-{operation}-{records}"
            catalog.mkdir()
            temporary.mkdir()
            case_env = dict(env, CLUSTERVISION_HOME=str(catalog), TMPDIR=str(temporary))
            row, peak = probe(binary, [operation, harness, "events", source, "--root", fixture], case_env, mmap)
            assert row["messages"] == messages and row["detail"]["events"] == len(expected), "events truncated or skipped"
            # Count persisted rows and compare exact ordering/targets outside the measured Rust
            # process; extraction success alone cannot conceal a failed best-effort SQL write.
            with sqlite3.connect(f"file:{catalog / 'catalog.db'}?mode=ro", uri=True) as conn:
                assert conn.execute("SELECT count(*) FROM events").fetchone()[0] == len(expected), "event publication incomplete"
                assert conn.execute("SELECT count(*) FROM source_dependencies WHERE consumer='events'").fetchone()[0] == 1, "source baseline missing"
                digest = hashlib.sha256()
                query = "SELECT e.msg_idx,e.kind,t.target FROM events e LEFT JOIN event_targets t ON t.id=e.target_id ORDER BY e.msg_idx"
                for i, (msg_idx, kind, target) in enumerate(conn.execute(query)):
                    assert (msg_idx, kind, target) == expected[i], "event ordering or target changed"
                    digest.update(json.dumps([msg_idx, kind, target]).encode())
                digests.append(digest.digest())
            assert not list(temporary.iterdir()), "completed publication leaked its owned spool"
            written = row["detail"]["temporary_bytes_written"]
            assert written > 0 if operation == "events" else written == 0, "spool I/O accounting changed"
            peaks[operation].append(peak)
            print(json.dumps({"case": f"{operation}-{shape}", "mmap": mmap, "records": records, "physical_input_bytes": source.stat().st_size,
                              "persisted_events": len(expected), "peak_live_rust_bytes": peak,
                              "temporary_bytes_written": written}), flush=True)
        assert digests[0] == digests[1], "bounded publication differs from the collector"
    bound(peaks["events"], MIB, MIB // 4, f"events {shape} extraction/publication mmap={mmap}")
    assert peaks["events-collected"][1] > 32 * MIB, "retained control no longer demonstrates the old memory cost"
    assert peaks["events-collected"][1] > peaks["events-collected"][0] + 16 * MIB, "retained control must grow with history"


def selected_documents(native, portable, root, env):
    """Selected giant fields remain spans, including positioned document readers."""
    generator = Path(__file__).with_name("perf-fixture.py")
    for harness in ("continue", "lmstudio"):
        for escaped in (False, True):
            peaks = {(mmap, operation): [] for mmap in (True, False)
                     for operation in ("discard", "discover")}
            for size in SIZES:
                name = f"selected-{harness}-{size}-{escaped}"
                command = [sys.executable, str(generator), harness, str(root / name),
                           "--payload-bytes", str(size), "--records", "4"]
                if escaped:
                    command.append("--escaped")
                manifest = json.loads(subprocess.check_output(command, env=env, text=True))
                common = [harness, manifest["id"], manifest["path"], "--root", manifest["root"]]
                for (mmap, operation), values in peaks.items():
                    catalog = root / f"catalog-{name}-{mmap}-{operation}"
                    catalog.mkdir()
                    row, peak = probe(native if mmap else portable, [operation, *common],
                                      dict(env, CLUSTERVISION_HOME=str(catalog)), mmap)
                    if operation == "discard":
                        assert row["messages"] == manifest["expected_messages"], "selected messages were skipped"
                    else:
                        assert row["detail"]["discovered_sessions"] == 1, "selected document was not discovered"
                    values.append(peak)
                    print(json.dumps({"case": name, "operation": operation, "mmap": mmap,
                                      "decoded_payload_bytes": size,
                                      "physical_input_bytes": manifest["physical_input_bytes"],
                                      "peak_live_rust_bytes": peak}), flush=True)
            for key, values in peaks.items():
                bound(values, 2 * MIB, MIB // 4, f"selected {harness} escaped={escaped} {key}")


def encoded_arguments(native, portable, root, env):
    """Late model selection must retain shared source ownership, not one backing per call."""
    generator = Path(__file__).with_name("perf-fixture.py")
    peaks = {True: [], False: []}
    for records in (3000, 12000):
        manifest = json.loads(subprocess.check_output([
            sys.executable, str(generator), "continue", str(root / f"encoded-{records}"),
            "--shape", "encoded-arguments", "--records", str(records),
            "--payload-bytes", str(records * 8192)], env=env, text=True))
        # Independent ChatML oracle; hash incrementally outside the measured child.
        payload = ("plain payload " * 586)[:8192]
        argument = json.dumps({"payload": payload}, separators=(",", ":"))
        message = json.dumps({"content": "```tool_call\nread " + argument + "\n```",
                              "role": "assistant"}, separators=(",", ":")).encode()
        digest = hashlib.sha256(b'{"messages":[')
        expected_bytes = len(b'{"messages":[')
        for i in range(records):
            piece = (b"," if i else b"") + message
            digest.update(piece)
            expected_bytes += len(piece)
        digest.update(b"]}\n")
        expected_bytes += 3
        for mmap, binary in ((True, native), (False, portable)):
            row, peak = probe(binary, ["dataset", "continue", manifest["id"], manifest["path"],
                                      "--root", manifest["root"], "--query", "model:present",
                                      "--hash-output"], env, mmap)
            detail = row["detail"]
            assert detail["record_written"] is True, "late model query incorrectly rejected output"
            assert detail["serialized_record_bytes"] == expected_bytes, "encoded arguments were skipped or changed"
            assert detail["record_sha256"] == digest.hexdigest(), "encoded argument output differs from independent oracle"
            peaks[mmap].append(peak)
            print(json.dumps({"case": "encoded-late-model-dataset", "mmap": mmap, "records": records,
                              "decoded_payload_bytes": records * 8192,
                              "physical_input_bytes": manifest["physical_input_bytes"],
                              "output_bytes": expected_bytes, "peak_live_rust_bytes": peak}), flush=True)
    for mmap, values in peaks.items():
        bound(values, 2 * MIB, MIB // 4, f"encoded deferred arguments mmap={mmap}")


def token_stats(binary, root, env, mmap, baseline=None):
    """Bound usage history and exact dedup, preserving independent provider token arithmetic."""
    generator = Path(__file__).with_name("perf-fixture.py")
    for harness in ("claude", "codex"):
        peaks = []
        for calls in (16_000, 64_000):
            name = f"stats-{harness}-{calls}-{mmap}"
            manifest = json.loads(subprocess.check_output(
                [sys.executable, str(generator), harness, str(root / name),
                 "--shape", "usage", "--records", str(calls)], env=env, text=True))
            catalog, temporary = root / f"catalog-{name}", root / f"temporary-{name}"
            catalog.mkdir()
            temporary.mkdir()
            case_env = dict(env, CLUSTERVISION_HOME=str(catalog), TMPDIR=str(temporary), RAYON_NUM_THREADS="2")
            common = [harness, manifest["id"], manifest["path"], "--root", manifest["root"]]
            row, peak = probe(binary, ["token-stats", *common], case_env, mmap)
            detail = row["detail"]
            assert (detail["sessions"], detail["subagents"], detail["failed"]) == (1, 0, 0), "usage source was skipped"
            assert detail["duplicates"] == manifest["expected_duplicates"], "response-ID precedence changed"
            assert {key: detail["total"][key] for key in manifest["expected_totals"]} == manifest["expected_totals"], "token arithmetic changed"
            assert len(detail["by_model"]) == len(detail["by_harness"]) == 1, "model/provider aggregation changed"
            assert not list(temporary.iterdir()), "token staging leaked its owned temporary files"
            peaks.append(peak)
            if baseline:
                reference, _ = probe(baseline, ["token-stats", *common], case_env, True)
                assert detail == reference["detail"], "old/new token reports differ"
            print(json.dumps({"case": f"token-stats-{harness}-{'mmap' if mmap else 'buffered'}",
                              "calls": calls, "records": manifest["records"],
                              "physical_input_bytes": manifest["physical_input_bytes"],
                              "peak_live_rust_bytes": peak, "duplicates": detail["duplicates"]}), flush=True)
        bound(peaks, 2 * MIB, MIB // 4, f"token stats {harness} mmap={mmap}")


def raw_body(path, repeats):
    """A JSON string of `repeats` escaped newlines: a span covers its body, quotes excluded."""
    block_repeats = 16 << 10
    block = b"x\\n" * block_repeats
    with path.open("wb") as out:
        out.write(b'"')
        blocks, tail = divmod(repeats, block_repeats)
        for _ in range(blocks):
            out.write(block)
        out.write(b"x\\n" * tail)
        out.write(b'"')


def chunks(binary, root, env, mmap):
    peaks = []
    for size in SIZES:
        repeats = (size + 2) // 3
        path = root / f"raw-{size}.txt"
        raw_body(path, repeats)
        body_bytes = path.stat().st_size - 2
        row, peak = probe(binary, ["chunks", "gemini", "unused", path, "--span", f"1:{body_bytes}", "--escaped", "--chunk-bytes", CHUNK_BYTES], env, mmap)
        expected = repeats * 2
        assert row["detail"]["decoded_bytes"] == expected, "decoder dropped or duplicated bytes"
        assert row["detail"]["largest_chunk_bytes"] <= CHUNK_BYTES + 3, "decoder output chunk is unbounded"
        peaks.append(peak)
        print(json.dumps({"case": f"chunks-{'mmap' if mmap else 'ranged-read'}-valid",
                          "raw_bytes": body_bytes, "decoded_bytes": expected, "peak_live_rust_bytes": peak}), flush=True)
    bound(peaks, CHUNK_CEILING, CHUNK_GROWTH, f"chunks mmap={mmap}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("probe", type=Path)
    parser.add_argument("--no-mmap-probe", required=True, type=Path)
    parser.add_argument("--only", choices=("stores", "chunks", "active", "portable-docs", "selected-docs", "encoded-arguments", "doctor", "metadata-facts", "selected-payloads", "portable-selected-payloads", "query", "events", "stats"), help="focused local check; default pass runs stores and chunks")
    parser.add_argument("--baseline-probe", type=Path, help="with active/doctor/metadata-facts/stats, separately check baseline behavior parity")
    parser.add_argument("--format", action="append", choices=("cline-sdk", "opensession", "chatgpt-export", "claude-export", "kimi", "zed", "kimi-code", "opencode-files", "opencode", "cursor", "cursor-ide", "goose", "codex", "gemini"),
                        help="repeat to focus --only active on selected native formats")
    args = parser.parse_args()
    if args.format and args.only not in ("active", "portable-docs", "selected-payloads", "portable-selected-payloads"):
        parser.error("--format applies only to active/portable-docs/selected-payloads/portable-selected-payloads")
    portable_formats = ("cline-sdk", "opensession", "chatgpt-export", "claude-export")
    if args.only == "active" and any(f not in (*portable_formats, "kimi", "zed") for f in args.format or []):
        parser.error("active supports document formats, kimi, and zed")
    if args.only in ("selected-payloads", "portable-selected-payloads") and any(f not in {name for name, _ in SELECTED_PAYLOAD_CASES} for f in args.format or []):
        parser.error("selected-payloads requires a selected payload case format")
    if args.only == "portable-docs" and any(f not in portable_formats for f in args.format or []):
        parser.error("portable-docs supports only document formats")
    if args.baseline_probe and args.only not in ("active", "doctor", "metadata-facts", "stats"):
        parser.error("--baseline-probe applies only to active/doctor/metadata-facts/stats checks")
    with tempfile.TemporaryDirectory(prefix="cv-heap-budget-") as temporary:
        root = Path(temporary)
        catalog = root / "catalog"
        catalog.mkdir()
        env = dict(os.environ, CLUSTERVISION_HOME=str(catalog))
        env.pop("CLUSTERVISION_CACHE", None)
        if args.only == "doctor":
            doctors(args.probe.resolve(), root, env, args.baseline_probe.resolve() if args.baseline_probe else None)
        elif args.only == "metadata-facts":
            metadata_facts(args.probe.resolve(), root, env, args.baseline_probe.resolve() if args.baseline_probe else None)
        elif args.only == "selected-payloads":
            selected_payloads(args.probe.resolve(), root, env, args.format)
        elif args.only == "portable-selected-payloads":
            selected_payloads(args.no_mmap_probe.resolve(), root, env, args.format, mmap=False)
        elif args.only == "query":
            queries(args.probe.resolve(), root, env)
        elif args.only == "events":
            native_root = root / "mmap"
            native_root.mkdir()
            events(args.probe.resolve(), native_root, env)
            portable_root = root / "positioned"
            portable_root.mkdir()
            events(args.no_mmap_probe.resolve(), portable_root, env, mmap=False)
        elif args.only == "stats":
            baseline = args.baseline_probe.resolve() if args.baseline_probe else None
            token_stats(args.probe.resolve(), root, env, True, baseline)
            token_stats(args.no_mmap_probe.resolve(), root, env, False)
        elif args.only == "portable-docs":
            active_formats(args.no_mmap_probe.resolve(), root, env, args.probe.resolve(),
                           args.format or portable_formats, mmap=False)
        elif args.only == "encoded-arguments":
            encoded_arguments(args.probe.resolve(), args.no_mmap_probe.resolve(), root, env)
        elif args.only == "selected-docs":
            selected_documents(args.probe.resolve(), args.no_mmap_probe.resolve(), root, env)
        elif args.only == "active":
            active_formats(args.probe.resolve(), root, env, args.baseline_probe.resolve() if args.baseline_probe else None, args.format)
        elif args.only != "chunks":
            stores(args.probe.resolve(), root, env)
        if args.only in (None, "chunks"):
            chunks(args.probe.resolve(), root, env, True)
            chunks(args.no_mmap_probe.resolve(), root, env, False)
    print("live Rust heap budgets passed", flush=True)


if __name__ == "__main__":
    main()
