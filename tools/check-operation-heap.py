#!/usr/bin/env python3
"""Small process-isolated whole-operation Rust heap gates, without timing thresholds.

Synthetic inputs only; output validation runs after each measured operation.
"""
import argparse
import hashlib
import importlib.util
import json
import os
import re
import shutil
import sys
from pathlib import Path
import subprocess
import tempfile

MIB = 1 << 20
PAYLOAD = 64 * MIB
CEILING = 4 * MIB
NEW_ID = "22222222-2222-4222-8222-222222222222"


def checked_peak(heap):
    assert isinstance(heap, dict), "heap instrumentation/counters missing"
    peak = heap.get("peak_above_baseline_bytes")
    assert type(peak) is int and peak > 0, "invalid or inactive live Rust heap counter"
    assert peak <= CEILING, f"whole-operation live Rust heap {peak} exceeds {CEILING} bytes"
    for name in ("allocation_requests", "requested_bytes"):
        value = heap.get(name)
        assert type(value) is int and value > 0, f"invalid or inactive {name} counter"
    return peak


def file_digest(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(64 << 10), b""):
            h.update(chunk)
    return h.hexdigest()


def prune_fixture(path, case, timestamp=None, cwd=None, payload_bytes=None):
    def row(role, uuid, content, **extra):
        if timestamp is not None:
            extra["timestamp"] = timestamp
        if cwd is not None:
            extra["cwd"] = cwd
        return dict(type=role, uuid=uuid, sessionId="old", **extra,
                    message=dict(role=role, content=content))
    rows = [row("user", "u", "prompt"), None, row("user", "v", "recent"),
            row("assistant", "z", [dict(type="text", text="recent answer")])]
    field = "thinking" if case == "thinking" else "text"
    block = dict(type=field, **{field: "MARKER"})
    if case == "thinking":
        block["signature"] = "sig"
    rows[1] = row("assistant", "a", [block, dict(type="text", text="conclusion")], parentUuid="u")
    if case == "thinking":
        rows[1]["message"]["content"].append(dict(type="tool_use", id="linked", name="Bash",
            input=dict(command="printf useful", description="linked control")))
        for index, tokens in ((1, 1000), (3, 2000)):
            rows[index]["message"]["usage"] = dict(input_tokens=tokens,
                cache_read_input_tokens=0, cache_creation_input_tokens=0)
        rows.insert(2, row("user", "result", [dict(type="tool_result", tool_use_id="linked",
            content="small result\n" * 512)]))
    prefix = "" if case == "thinking" else "ALPHA beta "
    if case == "unicode-declass":
        prefix = "🙂 " + prefix
    write_rows(path, rows, prefix, payload_bytes=payload_bytes)


def write_rows(path, rows, payload_prefix="", row_prefix=b"", row_suffix=b"\n", payload_bytes=None):
    payload_bytes = PAYLOAD if payload_bytes is None else payload_bytes
    with path.open("wb") as output:
        for row in rows:
            output.write(row_prefix)
            wire = json.dumps(row).encode()
            if b'"MARKER"' in wire:
                before, after = wire.split(b'MARKER')
                output.write(before)
                output.write(payload_prefix.encode())
                for _ in range(payload_bytes // (64 << 10)):
                    output.write(b"x" * (64 << 10))
                output.write(after)
            else:
                output.write(wire)
            output.write(row_suffix)


def rewind_gate(binary, root, env):
    case = root / "rewind"
    case.mkdir()
    source = case / "source.jsonl"
    prune_fixture(source, "thinking")
    original = file_digest(source)
    child = subprocess.run([str(binary), str(source), "rewind"], env=env, capture_output=True, text=True)
    assert child.returncode == 0, f"rewind probe failed: {child.stderr[-2000:]}"
    report = json.loads(child.stdout)
    peak = checked_peak(report["heap"])
    output = Path(report["result"]["new_path"])
    # Frozen native output from67ad8bf9; includes the giant reasoning and linked tool result.
    assert output.stat().st_size == 67117243
    assert file_digest(output) == "4553371cfa62e4eabe181e4f4438c5b2443ac62487ec97524e8b193c0c412654"
    assert file_digest(source) == original, "rewind modified source"
    assert report["result"]["open_tool_calls"] == 0
    print(json.dumps({"case":"rewind-native", "peak_live_rust_bytes":peak,
                      "source_bytes":source.stat().st_size, "output_bytes":output.stat().st_size}))


def prune_gate(binary, root, env, cases=("thinking", "ascii-declass", "unicode-declass")):
    # Frozen native sidecars (measured when the prune turn/linked-input paths landed). Hashes cover the
    # full decoded payload and metadata, without building a second giant owned Value tree.
    expected = {
        "thinking": ("a#think0", 67116376, "68c8fcaf6c42fa65fcd367f4eca3e8334dfb412f08637b2a206a97cfa3ad4822"),
        "ascii-declass": ("a#declass0", 67109028, "e02a472d3c4aa721b5199b991ef3c3aa1b1c73420f1885adaa73d1574a9c0b1f"),
        "unicode-declass": ("a#declass0", 67109033, "f33abf0c9f6c74196c646e4e57da3415f1da247097dbc964ccc6fdbd79d547dd"),
    }
    for case in cases:
        stash_id, sidecar_size, sidecar_hash = expected[case]
        directory = root / case
        directory.mkdir()
        source = directory / "source.jsonl"
        prune_fixture(source, case)
        original = file_digest(source)
        if case == "thinking":
            assert source.stat().st_size == 67117078
            assert original == "aa73756329ab751759a30b8c22fd0113728c9153b27a092634a3523cbb60407b"
        if case == "unicode-declass":
            assert source.stat().st_size == 67109416
            assert original == "1f357173a16bdbe9a790f72b95fd830d5eb301f284198f3c7f5f60665d42798e"
        flags = ["thinking", "revive"] if case == "thinking" else ["declassify"]
        child = subprocess.run([str(binary), str(source), *flags], env=env,
                               text=True, capture_output=True)
        assert child.returncode == 0, f"prune probe failed: {child.stderr[-2000:]}"
        report = json.loads(child.stdout)
        peak = checked_peak(report.get("heap"))
        result = report["result"]
        assert result["snipped_payloads"] == (2 if case == "thinking" else 1)
        assert result["tokens_freed"] > PAYLOAD // 8
        assert result["before_bytes"] > PAYLOAD and result["after_bytes"] < 2048
        if case == "thinking":
            assert report["revive_tokens"] == 107 and report["revive_old_tokens"] == 2000
            assert report["usage_rewritten"] == 2
        output_path = Path(result["new_path"])
        if case == "thinking":
            assert output_path.stat().st_size == 1329
            assert file_digest(output_path) == "96ba16a24d651dc47de704cc5d5fcad7a53951bde906f0cf0bff474f93d28317"
        if case == "unicode-declass":
            assert output_path.stat().st_size == 823
            assert file_digest(output_path) == "02277ad8d3b8e96ae49402adf892f2d905d5de5678013df342fa230beec32305"
        output = [json.loads(line) for line in output_path.read_text().splitlines()]
        assert all(row["sessionId"] == NEW_ID for row in output)
        assert [row["uuid"] for row in output] == (["u", "a", "result", "v", "z"]
                                                 if case == "thinking" else ["u", "a", "v", "z"])
        assert output[0]["message"]["content"] == "prompt"
        assert output[-2]["message"]["content"] == "recent"
        assert output[-1]["message"]["content"] == [dict(type="text", text="recent answer")]
        changed = output[1]["message"]["content"]
        # --drop-thinking omits the block outright; declassify leaves its marker in place.
        if case == "thinking":
            changed = [None, *changed]
        assert len(changed) == 3 if case == "thinking" else len(changed) == 2
        assert changed[1] == dict(type="text", text="conclusion")
        if case == "thinking":
            assert changed[2] == dict(type="tool_use", id="linked", name="Bash",
                input=dict(command="printf useful", description="linked control"))
            result_block = output[2]["message"]["content"]
            assert len(result_block) == 1 and result_block[0]["type"] == "tool_result"
            assert result_block[0]["tool_use_id"] == "linked"
            assert "id=linked tool=Bash command=printf useful, description=linked control |" in result_block[0]["content"]
        if case != "thinking":
            assert f"id={stash_id} " in changed[0]["text"] and "[PRUNED " in changed[0]["text"]
        sidecar = Path(result["sidecar_path"])
        assert sidecar.stat().st_size == sidecar_size and file_digest(sidecar) == sidecar_hash
        assert file_digest(source) == original, "prune modified its source"
        print(json.dumps(dict(case=case, peak_live_rust_bytes=peak,
                              sidecar_bytes=sidecar_size, sidecar_sha256=sidecar_hash)), flush=True)
        shutil.rmtree(directory)




def usage_gate(binary, root, env):
    directory = root / "usage-rewrite"
    directory.mkdir()
    source = directory / "source.jsonl"
    usage = dict(input_tokens=999999999, cache_read_input_tokens=11,
                 cache_creation_input_tokens=22, output_tokens=3,
                 cache_creation=dict(ephemeral_1h_input_tokens=5,
                                     ephemeral_5m_input_tokens=6, other="keep"),
                 iterations=[dict(type="advisor_message", input_tokens=9999),
                             dict(type="message", input_tokens=999999999,
                                  cache_read_input_tokens=11, cache_creation_input_tokens=22,
                                  output_tokens=3)])
    def row(role, uuid, content, **extra):
        return dict(type=role, uuid=uuid, sessionId="old", message=dict(role=role, content=content, **extra))
    rows = [dict(type="system", subtype="compact_boundary", sessionId="old"),
            row("user", "u", "prompt"),
            dict(row("assistant", "a", [dict(type="text", text="small result")], usage=usage), ignored="MARKER"),
            row("user", "v", "recent"),
            row("assistant", "z", [dict(type="text", text="recent answer")])]
    write_rows(source, rows, row_prefix=b"  ", row_suffix=b"  \r\n")
    original = file_digest(source)
    child = subprocess.run([str(binary), str(source), "revive"], env=env, text=True, capture_output=True)
    assert child.returncode == 0, f"usage prune probe failed: {child.stderr[-2000:]}"
    report = json.loads(child.stdout)
    peak = checked_peak(report.get("heap"))
    result = report["result"]
    assert result["snipped_payloads"] == 0 and result["sidecar_path"] is None
    # The only recorded prompt follows the boundary, so it is not evidence: content plus the
    # unknown fixed overhead (20k), with revive's 5% margin.
    assert report["usage_rewritten"] == 1 and report["revive_tokens"] == 21069
    assert report["revive_old_tokens"] == 1000000032
    output = Path(result["new_path"])
    assert output.stat().st_size == 67110014
    # Frozen owned operation: checks complete giant body preservation and exact usage rewrite.
    assert file_digest(output) == "82895801012a033034b36107784d30835b2890522939f28a85467928a0ce93b7"
    assert file_digest(source) == original
    print(json.dumps(dict(case="unchanged-giant-usage-rewrite", peak_live_rust_bytes=peak,
                          usage_rewritten=1, output_bytes=output.stat().st_size)), flush=True)
    shutil.rmtree(directory)



def claude_output_digest(path):
    """Hash validated canonical probe JSONL as an array, normalizing only top-level UUIDs.

    The emitter's uuid/parentUuid prefix is bounded; all remaining bytes are hashed unchanged.
    This is an output check for this fixed probe, not a general JSON parser/canonicalizer.
    """
    uuid = rb"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}"
    prefix_pattern = re.compile(rb'^\{"uuid":"(' + uuid + rb')","parentUuid":(null|"' + uuid + rb'"),')
    names = {}
    def name(value):
        if value not in names:
            names[value] = f"generated-{len(names)}".encode()
        return b'"' + names[value] + b'"'
    digest = hashlib.sha256()
    digest.update(b"[")
    rows = 0
    with path.open("rb") as source:
        while prefix := source.readline(160):
            match = prefix_pattern.match(prefix)
            assert match, "unexpected Claude probe record prefix"
            normalized_uuid = name(match[1])
            parent = b"null" if match[2] == b"null" else name(match[2][1:-1])
            if rows:
                digest.update(b",")
            prefix = b'{"uuid":' + normalized_uuid + b',"parentUuid":' + parent + b',' + prefix[match.end():]
            while True:
                if prefix.endswith(b"\n"):
                    digest.update(prefix[:-1])
                    break
                digest.update(prefix)
                prefix = source.readline(64 << 10)
                assert prefix, "unterminated Claude probe record"
            rows += 1
    digest.update(b"]")
    return digest.hexdigest(), rows


def claude_port_gate(binary, root, env):
    directory = root / "claude-port"
    directory.mkdir()
    source = directory / "source.jsonl"
    generator = Path(__file__).parent / "profiling/claude-port-fixture.py"
    subprocess.run([sys.executable, "-I", str(generator), str(source), "--characters", str(PAYLOAD)],
                   env=env, check=True)
    # Frozen fixture/output contract, measured natively at 64M characters.
    original = file_digest(source)
    assert source.stat().st_size == 89479544
    assert original == "0ce7431d3a56ccfeb3c5a565551557380e9eb04ff02f5de1dc1472210301584d"
    child = subprocess.run([str(binary), "codex", str(source), str(directory / "output"), "native"],
                           env=env, text=True, capture_output=True)
    assert child.returncode == 0, f"verified Claude export failed: {child.stderr[-2000:]}"
    report = json.loads(child.stdout)
    peak = checked_peak(json.loads(child.stderr))
    assert report["messages"] == 4 and report["fidelity"] == dict(deltas=[])
    output = Path(report["path"])
    assert output.stat().st_size == 74566782
    digest, rows = claude_output_digest(output)
    assert digest == "bb1364b6a0e47516873453e41eee4700f8c472f017b79820daf5489b5ae34119"
    assert rows == 4
    assert file_digest(source) == original, "Claude export modified its source"
    print(json.dumps(dict(case="verified-claude-export", peak_live_rust_bytes=peak,
                          messages=4, output_bytes=output.stat().st_size,
                          normalized_sha256=digest)), flush=True)
    shutil.rmtree(directory)



def claude_portable_gate(binary, root, env):
    """Portable replay must not own giant reasoning/tool bodies while changing wire fields."""
    directory = root / "claude-portable"
    directory.mkdir()
    source = directory / "source.jsonl"
    encoded = b'line\\n\\u03bb ' * 4096
    decoded = ("line\nλ " * 4096).encode()
    repeats = PAYLOAD // len(encoded)
    expected = hashlib.sha256()
    for _ in range(repeats):
        expected.update(decoded)
    expected = expected.hexdigest()
    ids = ["a:b", "a#b", "a_b"]
    def row(role, uid, content, parent=None):
        return dict(type=role, uuid=uid, parentUuid=parent, sessionId="source",
                    timestamp="2026-10-14T12:00:00Z", cwd="/synthetic/portable",
                    message=dict(role=role, model="proxy-model", content=content))
    rows = [row("user", "u", "a:b is ordinary prompt text"),
            row("assistant", "a", [dict(type="thinking", thinking="GIANT", signature="foreign"),
                *[dict(type="tool_use", id=ident, name="Bash", input=dict(command="echo a:b"))
                  for ident in ids]], "u"),
            row("user", "r", [dict(type="tool_result", tool_use_id=ident,
                                    content="GIANT" if index == 0 else "small a:b result")
                               for index, ident in enumerate(ids)], "a"),
            row("assistant", "z", [dict(type="text", text="finished")], "r")]
    with source.open("wb") as output:
        for record in rows:
            wire = json.dumps(record, separators=(",", ":")).encode()
            parts = wire.split(b'GIANT')
            output.write(parts[0])
            for part in parts[1:]:
                for _ in range(repeats):
                    output.write(encoded)
                output.write(part)
            output.write(b"\n")
    original = file_digest(source)
    child = subprocess.run([str(binary), "claude", str(source), str(directory / "output"), "portable"],
                           env=env, text=True, capture_output=True)
    assert child.returncode == 0, f"portable Claude export failed: {child.stderr[-2000:]}"
    report = json.loads(child.stdout)
    peak = checked_peak(json.loads(child.stderr))
    assert all(delta["expected"] for delta in report["fidelity"]["deltas"]), report
    calls, results, giant_texts, giant_results = [], [], 0, 0
    # Decode output only after the child measurement; these allocations are outside its Rust heap.
    with Path(report["path"]).open() as output:
        for line in output:
            content = json.loads(line).get("message", {}).get("content", [])
            if isinstance(content, str):
                assert content == "a:b is ordinary prompt text"
                continue
            for block in content:
                assert block["type"] not in ("thinking", "redacted_thinking"), block["type"]
                if block["type"] == "tool_use":
                    calls.append(block["id"])
                    assert block["input"] == dict(command="echo a:b")
                elif block["type"] == "tool_result":
                    results.append(block["tool_use_id"])
                    if len(block["content"]) > 1024:
                        assert hashlib.sha256(block["content"].encode()).hexdigest() == expected
                        giant_results += 1
                elif block["type"] == "text" and len(block["text"]) > 1024:
                    assert hashlib.sha256(block["text"].encode()).hexdigest() == expected
                    giant_texts += 1
    assert len(calls) == len(set(calls)) == 3 and calls == results, (calls, results)
    assert calls[2] == "a_b" and all(re.fullmatch(r"[A-Za-z0-9_-]+", ident) for ident in calls)
    assert giant_texts == giant_results == 1
    assert file_digest(source) == original
    print(json.dumps(dict(case="portable-claude-replay", peak_live_rust_bytes=peak,
                          payload_bytes=repeats * len(encoded), tool_pairs=len(calls))), flush=True)
    shutil.rmtree(directory)


def codex_output_digest(path):
    """Hash the entire fixed-clock output, normalizing only the package version in its metadata."""
    digest = hashlib.sha256()
    with path.open("rb") as source:
        header = source.readline(128 << 10)
        assert header.endswith(b"\n"), "missing or oversized Codex metadata record"
        metadata = json.loads(header)
        assert metadata["type"] == "session_meta"
        assert isinstance(metadata["payload"]["cli_version"], str)
        metadata["payload"]["cli_version"] = "VERSION"
        digest.update(json.dumps(metadata, ensure_ascii=False, separators=(",", ":")).encode() + b"\n")
        rows = 1
        final = header
        for chunk in iter(lambda: source.read(64 << 10), b""):
            digest.update(chunk)
            rows += chunk.count(b"\n")
            final = chunk
        assert final.endswith(b"\n"), "unterminated Codex output"
    return digest.hexdigest(), rows


def codex_thinking_gate(binary, root, env):
    directory = root / "codex-thinking"
    directory.mkdir()
    source = directory / "source.jsonl"
    # Same 64 MiB reasoning/tool fixture, with a fixed clock so complete wire bytes are stable.
    prune_fixture(source, "thinking", timestamp="2026-10-12T12:00:00Z", cwd="/synthetic/codex-thinking")
    original = file_digest(source)
    assert source.stat().st_size == 67117443
    assert original == "a18516c6e53acc6a6acce30fa5aeca7e15db9838b69927a71af485874899477a"
    for mode in ("text", "drop"):
        child = subprocess.run([str(binary), "claude", str(source), str(directory / mode), mode, "codex"],
                               env=env, text=True, capture_output=True)
        assert child.returncode == 0, f"verified Codex {mode} export failed: {child.stderr[-2000:]}"
        report = json.loads(child.stdout)
        peak = checked_peak(json.loads(child.stderr))
        output = Path(report["path"])
        digest, rows = codex_output_digest(output)
        # Frozen whole-port output: partial usage has no invented total; the report
        # declares timestamps supplied by the target (including split accounting turns).
        expected_digest, expected_rows, expected_report = CODEX_THINKING_EXPECTED[mode]
        assert digest == expected_digest and rows == expected_rows
        assert report["messages"] == 5 and report["fidelity"] == expected_report
        assert file_digest(source) == original, "Codex export modified its source"
        print(json.dumps(dict(case=f"verified-codex-{mode}-thinking-export", peak_live_rust_bytes=peak,
                              messages=5, output_bytes=output.stat().st_size,
                              normalized_sha256=digest)), flush=True)
    shutil.rmtree(directory)


CODEX_THINKING_EXPECTED = {'text': ['c6bc76805ca65ae2f0d545d380dd97b7b3cad84630acf7d51ab375ffcdca199e',
          14,
          {'deltas': [{'what': 'thinking(signature)', 'a': '1', 'b': '0', 'expected': True, 'where': 'codex'},
                      {'what': 'timestamps supplied by target', 'a': '4', 'b': '6', 'expected': True, 'where': 'codex'},
                      {'what': 'message ids', 'a': '4', 'b': '0', 'expected': True, 'where': 'codex'},
                      {'what': 'message ids (tool turns)', 'a': '1', 'b': '0', 'expected': True, 'where': 'codex'}]}],
 'drop': ['f44659d7fbb0e8de2c128e5eda50ad5b9e2917866f1c1c55ee82d3da85705190',
          13,
          {'deltas': [{'what': 'thinking(text)', 'a': '1', 'b': '0', 'expected': True, 'where': 'codex'},
                      {'what': 'thinking(signature)', 'a': '1', 'b': '0', 'expected': True, 'where': 'codex'},
                      {'what': 'timestamps supplied by target', 'a': '4', 'b': '5', 'expected': True, 'where': 'codex'},
                      {'what': 'message ids', 'a': '4', 'b': '0', 'expected': True, 'where': 'codex'},
                      {'what': 'message ids (tool turns)', 'a': '1', 'b': '0', 'expected': True, 'where': 'codex'}]}]}


def gemini_output_digest(path):
    """Fixed pretty or JSONL fixture: normalize only validated message id headers (UUID v5),
    not payload/tool IDs."""
    pretty = re.compile(rb'(      "id": ")([0-9a-f]{8}-[0-9a-f]{4}-5[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})(",\n)')
    jsonl = re.compile(rb'^(\{"id":")([0-9a-f]{8}-[0-9a-f]{4}-5[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12})(",)')
    digest = hashlib.sha256()
    ids = 0
    bol = True
    with path.open("rb") as source:
        for line in iter(lambda: source.readline(64 << 10), b""):
            if bol:
                match = pretty.fullmatch(line)
                if match:
                    line = match[1] + b"00000000-0000-4000-8000-000000000000" + match[3]
                    ids += 1
                else:
                    m2 = jsonl.match(line)
                    if m2:
                        line = m2[1] + b"00000000-0000-4000-8000-000000000000" + m2[3] + line[m2.end():]
                        ids += 1
            bol = line.endswith(b"\n")
            digest.update(line)
    return digest.hexdigest(), ids


def gemini_port_gate(binary, root, env):
    directory = root / "gemini-port"
    directory.mkdir()
    source = directory / "source.jsonl"
    prune_fixture(source, "ascii-declass", timestamp="2026-10-12T12:00:00Z", cwd="/synthetic/export", payload_bytes=32 * MIB)
    original = file_digest(source)
    assert source.stat().st_size == 33555239
    assert original == "a2859c7a05a72e86e52f0cd359c9222ec3c4bb949ef36eb36c3531d71be602fb"
    child = subprocess.run([str(binary), "claude", str(source), str(directory / "output"), "native", "gemini"],
                           env=env, text=True, capture_output=True)
    assert child.returncode == 0, f"verified Gemini export failed: {child.stderr[-2000:]}"
    report = json.loads(child.stdout)
    peak = checked_peak(json.loads(child.stderr))
    assert report["messages"] == 4 and report["fidelity"] == dict(deltas=[])
    output = Path(report["path"])
    assert output.stat().st_size == 33555582
    digest, ids = gemini_output_digest(output)
    assert ids == 4 and digest == "567c3d0e0fc2568b6d56e2a1e1fcf9319787e8df9515c2365c525899da17c2a0"
    assert file_digest(source) == original, "Gemini export modified its source"
    print(json.dumps(dict(case="verified-gemini-export", peak_live_rust_bytes=peak,
                          messages=4, output_bytes=output.stat().st_size, normalized_sha256=digest)), flush=True)
    shutil.rmtree(directory)



def cline_port_gate(binary, root, env):
    directory = root / "cline-port"
    directory.mkdir()
    source = directory / "source.jsonl"
    prune_fixture(source, "ascii-declass", timestamp="2026-10-12T12:00:00Z", cwd="/synthetic/export", payload_bytes=32 * MIB)
    original = file_digest(source)
    assert source.stat().st_size == 33555239
    assert original == "a2859c7a05a72e86e52f0cd359c9222ec3c4bb949ef36eb36c3531d71be602fb"
    child = subprocess.run([str(binary), "claude", str(source), str(directory / "output"), "native", "cline"],
                           env=env, text=True, capture_output=True)
    assert child.returncode == 0, f"verified Cline export failed: {child.stderr[-2000:]}"
    report = json.loads(child.stdout)
    peak = checked_peak(json.loads(child.stderr))
    # Frozen legacy format loses timestamps and IDs; ownership changes preserve its report.
    assert report["messages"] == 4 and report["fidelity"] == dict(deltas=[
        dict(what="timestamps", a="4", b="1", expected=False, where="cline"),
        dict(what="message ids", a="4", b="0", expected=False, where="cline")])
    output = Path(report["path"])
    files = {
        "api_conversation_history.json": (33555154, "ce2ed1d91cd5c44afd871021a6a417c7a3bcde99ac31369715447509f515286c"),
        "task_metadata.json": (58, "862aa2776b8e6dc3df0f1b435e4aa4c2f6f6e83418e97179ec8bc22b7d8a195b"),
        "ui_messages.json": (89, "f6a41742990f6eba4165b270afc88296d9b9331705772b22aaa353f76584af87"),
    }
    for name, (size, digest) in files.items():
        path = output / name
        assert path.stat().st_size == size and file_digest(path) == digest, f"Cline output changed: {name}"
    assert file_digest(source) == original, "Cline export modified its source"
    print(json.dumps(dict(case="verified-cline-export", peak_live_rust_bytes=peak,
                          messages=4, output_bytes=sum(size for size, _ in files.values()),
                          sha256=files["api_conversation_history.json"][1])), flush=True)
    shutil.rmtree(directory)


def gemini_rehome_fixture(path):
    """Modern recording; fixed metadata and one ordinary 32 MiB assistant body."""
    stamp = "2026-10-12T12:00:00Z"
    header = dict(sessionId="synthetic-source", projectHash="controlled-project",
                  startTime=stamp, lastUpdated=stamp, summary="controlled complete title",
                  directories=["/synthetic/gemini-rehome"], kind="subagent",
                  unknown=dict(kept_by_native_reader=False))
    messages = [dict(id="u", type="user", timestamp=stamp, content="small prompt"),
                dict(id="a", type="gemini", timestamp=stamp,
                     content="ordinary native text " + "x" * (32 * MIB - 21),
                     model="controlled-model", tokens=dict(input=11, output=3),
                     unknown=dict(ignored=True)),
                dict(id="w", type="warning", timestamp=stamp, content="small notice")]
    with path.open("x") as output:
        for record in [header, *messages]:
            json.dump(record, output, separators=(",", ":"))
            output.write("\n")


def gemini_rehome_gate(binary, root, env):
    directory = root / "gemini-rehome"
    chats = directory / "source" / "chats"
    chats.mkdir(parents=True)
    source = chats / "session-modern.jsonl"
    gemini_rehome_fixture(source)
    original = file_digest(source)
    assert original == "0677c225601bb8f04ad4b6856ef09158edebf60acbed8f041073b1f0bd9ab988"
    child = subprocess.run([str(binary), "gemini", str(source), str(directory / "output"), "native", "gemini"],
                           env=env, text=True, capture_output=True)
    assert child.returncode == 0, f"verified same-harness Gemini port failed: {child.stderr[-2000:]}"
    report = json.loads(child.stdout)
    peak = checked_peak(json.loads(child.stderr))
    assert report["messages"] == 3 and report["fidelity"] == dict(deltas=[])
    output = Path(report["path"])
    assert output.stat().st_size == 33555134
    # Frozen complete owned preparation: normalize only generated message UUIDs.
    digest, ids = gemini_output_digest(output)
    assert ids == 3 and digest == "d7ce3172b16c416d47897dceb23675d968f76b17b298332335fe8908b7732374"
    assert file_digest(source) == original, "same-harness Gemini port modified its source"
    print(json.dumps(dict(case="verified-gemini-complete-rehome", peak_live_rust_bytes=peak,
                          messages=3, output_bytes=output.stat().st_size, normalized_sha256=digest)), flush=True)
    shutil.rmtree(directory)

def claude_rehome_gate(binary, root, env):
    directory = root / "claude-rehome"
    directory.mkdir()
    source = directory / "source.jsonl"
    prune_fixture(source, "thinking", timestamp="2026-10-12T12:00:00Z", cwd="/synthetic/same-claude")
    with source.open("ab") as output:
        output.write(b'{"type":"future-meta","sessionId":"old","opaque":{"kept":true}}\n')
    original = file_digest(source)
    assert source.stat().st_size == 67117492
    assert original == "e1ff8319b4025d0e008c0192bd68183bf20ee564a747028cf6d22316036592a4"
    child = subprocess.run([str(binary), "claude", str(source), str(directory / "output"), "native"],
                           env=env, text=True, capture_output=True)
    assert child.returncode == 0, f"verified same-harness Claude port failed: {child.stderr[-2000:]}"
    report = json.loads(child.stdout)
    peak = checked_peak(json.loads(child.stderr))
    output = Path(report["path"])
    assert report["messages"] == 6 and report["fidelity"] == dict(deltas=[])
    assert output.stat().st_size == 67117687
    # Frozen owned complete parse/rehome: all bytes, including ordinary bodies and unknown carrier.
    digest = file_digest(output)
    assert digest == "f86d92ed9208134168092ec42e0e29ce22777d9a2239840dbc93be6ecfc95883"
    assert file_digest(source) == original, "same-harness port modified its source"
    print(json.dumps(dict(case="verified-claude-complete-rehome", peak_live_rust_bytes=peak,
                          messages=6, output_bytes=output.stat().st_size, sha256=digest)), flush=True)
    shutil.rmtree(directory)


def window_binary(report_path):
    artifacts = [json.loads(line) for line in report_path.read_text().splitlines()]
    candidates = [item["executable"] for item in artifacts
                  if item.get("reason") == "compiler-artifact"
                  and item.get("target", {}).get("name") == "cv"
                  and item.get("profile", {}).get("test") and item.get("executable")]
    assert len(candidates) == 1, "expected exactly one compiled cv test executable"
    return Path(candidates[0]).resolve(strict=True)


def window_gate(binary, root, env):
    spec = importlib.util.spec_from_file_location("window_extra", Path(__file__).parent / "profiling/window-extra.py")
    fixtures = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixtures)
    for harness in ("claude", "cursor"):
        directory = root / harness
        directory.mkdir()
        path = fixtures.generate_fixture(directory, harness, "unselected", 64)
        assert path.stat().st_size > PAYLOAD
        original = file_digest(path)
        child_env = dict(env, CLUSTERVISION_HOME=str(directory / "catalog"),
                         CV_WINDOW_PROBE_SOURCE=str(path), CV_WINDOW_PROBE_HARNESS=harness,
                         CV_WINDOW_PROBE_RANGE="0..1", CV_WINDOW_PROBE_ORACLE="0")
        child = subprocess.run([str(binary), fixtures.TEST, "--exact", "--ignored",
                                "--nocapture", "--test-threads=1"], env=child_env,
                               text=True, capture_output=True)
        assert child.returncode == 0, f"window probe failed: {child.stderr[-2000:]}"
        match = re.search(r"CV_WINDOW_HEAP (\{[^\n]+\})", child.stderr)
        assert match, "whole-command heap counters missing (test hook must actually run)"
        peak = checked_peak(json.loads(match[1]))
        start = child.stdout.index("{\n")
        output, _ = json.JSONDecoder().raw_decode(child.stdout[start:])
        message = dict(id="u", role="user", kind="prompt", origin="human",
                       content=[dict(type="text", text="selected prompt")])
        expected = dict(id="window-extra", harness=harness, messages=[message], source_path=str(path))
        if harness == "claude":
            message["timestamp"] = "2026-01-01T00:00:00Z"
            expected.update(cwd="/tmp/window", title="final title",
                            created_at="2026-01-01T00:00:00Z", updated_at="2026-01-01T00:00:00Z",
                            git=dict(branch="late-main"), extra=dict(claude=dict(custom_title="final title")))
        else:
            message["extra"] = dict(cursor=dict(unified_mode_raw=99))
            expected.update(title="EOF composer title", model="latest-model",
                            created_at="1970-01-01T00:00:01Z", updated_at="1970-01-01T00:00:02Z",
                            extra=dict(cursor=dict(unified_mode="agent")))
        assert output == expected, "selected message or late metadata changed"
        assert file_digest(path) == original, "window command modified its source"
        print(json.dumps(dict(case=f"{harness}-unselected-rich-window",
                              peak_live_rust_bytes=peak, messages=1)), flush=True)
        shutil.rmtree(directory)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--prune-probe", type=Path, required=True)
    parser.add_argument("--window-build-report", type=Path, required=True)
    parser.add_argument("--claude-port-probe", type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cv-operation-heap-") as directory:
        root = Path(directory)
        (root / "home").mkdir()
        env = dict(os.environ, HOME=str(root / "home"),
                   CLUSTERVISION_HOME=str(root / "catalog"), RAYON_NUM_THREADS="1")
        env.pop("CLUSTERVISION_CACHE", None)
        binary = args.prune_probe.resolve(strict=True)
        prune_gate(binary, root, env)
        rewind_gate(binary, root, env)
        usage_gate(binary, root, env)
        window_gate(window_binary(args.window_build_report), root, env)
        claude_port_gate(args.claude_port_probe.resolve(strict=True), root, env)
        claude_portable_gate(args.claude_port_probe.resolve(strict=True), root, env)
        codex_thinking_gate(args.claude_port_probe.resolve(strict=True), root, env)
        gemini_port_gate(args.claude_port_probe.resolve(strict=True), root, env)
        cline_port_gate(args.claude_port_probe.resolve(strict=True), root, env)
        gemini_rehome_gate(args.claude_port_probe.resolve(strict=True), root, env)
        claude_rehome_gate(args.claude_port_probe.resolve(strict=True), root, env)


if __name__ == "__main__":
    main()
