#!/usr/bin/env python3
"""Compare a released cv binary's contracts with a candidate's. No network or real harness stores.

Additions are compatible; a removed or changed existing shape, default or route is not. A
reviewed removal is named with --accept (its contract path, e.g.
tools.workflow.inputSchema.properties.follow, or commands.watch.args.interval.value_type for a
reviewed change of one value) beside the pinned baseline that still has it.
"""
import argparse
import http.client
import json
import os
from pathlib import Path
import re
import selectors
import subprocess
import tempfile


class Incompatible(Exception):
    pass


def require(ok, message):
    if not ok:
        raise Incompatible(message)


def subset(old, new, path="contract", accepted=frozenset()):
    """Additions are compatible; changing existing values, defaults or requiredness is not."""
    if isinstance(old, dict):
        require(isinstance(new, dict), f"{path}: object changed")
        for key, value in old.items():
            if key in {"description", "help", "about"} or (key == "required" and "properties" in old):
                continue
            if f"{path}.{key}" in accepted and key not in new:
                continue
            require(key in new, f"{path}.{key}: removed")
            subset(value, new[key], f"{path}.{key}", accepted)
        # JSON Schema required lists describe input obligations, not output vocabulary.
        if "properties" in old:
            require(set(new.get("required", [])) <= set(old.get("required", [])), f"{path}: new required input")
    elif isinstance(old, list):
        require(isinstance(new, list), f"{path}: array changed")
        for value in old:
            def matches(candidate):
                try:
                    subset(value, candidate, path, accepted)
                    return True
                except Incompatible:
                    return False
            require(any(matches(candidate) for candidate in new), f"{path}: removed/changed {value!r}")
    else:
        # An accepted path also covers a reviewed change of the value itself (a widened type).
        require(old == new or path in accepted, f"{path}: {old!r} -> {new!r}")


def named(rows):
    require(len({r["name"] for r in rows}) == len(rows), "duplicate contract name")
    return {r["name"]: r for r in rows}


def shape(value, key=""):
    if key in {"extra", "input", "details"}:
        return "opaque JSON"
    if isinstance(value, dict):
        return {k: shape(v, k) for k, v in value.items()}
    if isinstance(value, list):
        unique = {json.dumps(shape(v), sort_keys=True) for v in value}
        return [json.loads(v) for v in sorted(unique)]
    if key in {"type", "role", "kind", "origin"} and isinstance(value, str):
        return "=" + value
    return "null" if value is None else type(value).__name__


def data_contract(schema):
    # Protect machine vocabularies/field inventories, not incidental prose in the reference.
    keys = {"session_row": ("keys", "enrich_keys", "search_extra_keys"),
            "session": ("fields", "lineage"), "message": ("fields", "role", "kind", "origin", "usage"),
            "block": ("tag", "types")}
    return {section: {key: schema[section][key] for key in fields} for section, fields in keys.items()}


def world(root):
    root = Path(root)
    home = root / "home"
    projects = home / ".claude/projects/fixture"
    projects.mkdir(parents=True)
    records = [
        {"type": "user", "uuid": "u", "cwd": "/compat", "timestamp": "2026-01-01T00:00:00Z",
         "message": {"role": "user", "content": "hello"}},
        {"type": "assistant", "uuid": "a", "timestamp": "2026-01-01T00:00:01Z",
         "message": {"role": "assistant", "model": "fixture", "content": [{"type": "text", "text": "hello"}]}},
    ]
    (projects / "compat-session.jsonl").write_text("".join(json.dumps(r) + "\n" for r in records))
    # Codex defers its count to a complete read, so its first `ls` row carries the uncounted shape.
    rollouts = home / ".codex/sessions/2026/01/01"
    rollouts.mkdir(parents=True)
    sid = "0198a000-0000-7000-8000-00000000c0de"
    rollout = [
        {"timestamp": "2026-01-01T00:00:00.000Z", "type": "session_meta",
         "payload": {"id": sid, "timestamp": "2026-01-01T00:00:00.000Z", "cwd": "/compat",
                     "originator": "codex_cli_rs", "cli_version": "0.1.0"}},
        {"timestamp": "2026-01-01T00:00:01.000Z", "type": "response_item",
         "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]}},
        {"timestamp": "2026-01-01T00:00:02.000Z", "type": "response_item",
         "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "hello"}]}},
    ]
    (rollouts / f"rollout-2026-01-01T00-00-00-{sid}.jsonl").write_text("".join(json.dumps(r) + "\n" for r in rollout))
    return {"PATH": os.environ["PATH"], "HOME": str(home), "CLUSTERVISION_HOME": str(root / "catalog"),
            "CLAUDE_CONFIG_DIR": str(home / ".claude"), "CODEX_HOME": str(home / ".codex"),
            "XDG_CACHE_HOME": str(root / "cache"), "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_DATA_HOME": str(home / ".local/share"), "NO_COLOR": "1"}


def invoke(binary, env, args, code=0, stdin=None):
    p = subprocess.run([str(binary), *args], env=env, input=stdin, capture_output=True, text=True, timeout=30)
    require(p.returncode == code, f"{args}: exit {p.returncode}, expected {code}: {p.stderr[-500:]}")
    return p.stdout


def http_routes(binary, env, inventory):
    process = subprocess.Popen([str(binary), "serve", "--port", "0"], env=env,
                               stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    try:
        selector = selectors.DefaultSelector()
        selector.register(process.stderr, selectors.EVENT_READ)
        require(bool(selector.select(10)), "serve did not announce its bound address")
        banner = process.stderr.readline()
        match = re.search(r'http://([^: ]+):(\d+)', banner)
        require(match is not None, f"serve banner: {banner!r}")
        host, port = match.group(1, 2)
        responses = {}
        for route in inventory:
            method, template = route.split(" ", 1)
            path = template
            if path.startswith("/api/session/{}/{}"):
                path = path.replace("/api/session/{}/{}", "/api/session/claude/compat-session", 1)
            path = path.replace("{}", "missing").replace("*", "missing")
            connection = http.client.HTTPConnection(host, int(port), timeout=10)
            try:
                connection.request(method, path, body=json.dumps({"who": "compat", "text": "fixture"}) if method == "POST" else None,
                                   headers={"Content-Type": "application/json"})
                response = connection.getresponse()
                body = response.read().decode()
                if response.status == 410:
                    require(bool(re.search(r"/api/|cv ", body)), f"{route}: 410 must point to its replacement")
                responses[route] = {"status": response.status, "pointer": body if response.status == 410 else ""}
                require(response.status < 500, f"{route}: server failure {response.status}: {body[:200]}")
            finally:
                connection.close()
        return responses
    finally:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
        process.stderr.close()


def collect(binary, root, routes=None):
    """The binary's contracts. `routes` probes a baseline that predates `schema --json` routes."""
    env = world(root)
    commands = json.loads(invoke(binary, env, ["schema", "--commands", "--json"]))
    schema = json.loads(invoke(binary, env, ["schema", "--json"]))
    rows = json.loads(invoke(binary, env, ["ls", "--json"]))
    require(isinstance(rows, list) and sorted(r["harness"] for r in rows) == ["claude", "codex"],
            "fixture must discover exactly its Claude and Codex sessions")
    session = json.loads(invoke(binary, env, ["show", "compat-session", "--json"]))
    if any(c["name"] == "mcp" for c in commands):
        requests = "\n".join(json.dumps({"jsonrpc": "2.0", "id": i, "method": method, "params": params})
                             for i, method, params in [(1, "initialize", {"protocolVersion": "2025-06-18"}), (2, "tools/list", {})]) + "\n"
        responses = [json.loads(line) for line in invoke(binary, env, ["mcp"], stdin=requests).splitlines()]
        tools = next(r["result"]["tools"] for r in responses if r.get("id") == 2)
    else:
        tools = []
    # The schema's field inventories must agree with what the binary actually serializes.
    for row in rows:
        require(set(schema["session_row"]["keys"]) <= row.keys(), "session-row schema drifted from ls JSON")
    require(session.keys() <= set(schema["session"]["fields"]), "session schema omits real serde fields")
    for message in session["messages"]:
        require(message.keys() <= set(schema["message"]["fields"]), "message schema omits real serde fields")
        for block in message["content"]:
            require(block["type"] in schema["block"]["types"], f"schema omits block tag {block['type']}")
            require(block.keys() - {"type"} <= set(schema["block"]["types"][block["type"]]), f"schema omits block field: {block['type']}")
    invoke(binary, env, ["show", "absent-session"], code=2)
    invoke(binary, env, ["--not-a-cv-flag"], code=2)
    # Failure to create an archive under an ordinary file is an operational error, not usage.
    denied = Path(root) / "not-a-directory"
    denied.write_text("fixture")
    invoke(binary, dict(env, CLUSTERVISION_HOME=str(denied / "catalog")), ["task", "open", "compat operational failure"], code=1)
    inventory = schema.get("routes", routes)
    require(bool(inventory), "no HTTP route inventory: the candidate's `schema --json` must list routes")
    return {"commands": commands, "schema": data_contract(schema), "rows": {r["harness"]: shape(r) for r in rows}, "session": shape(session),
            "tools": tools, "routes": http_routes(binary, env, inventory), "inventory": inventory,
            "version": invoke(binary, env, ["--version"]).strip()}


def compare(old, new, accepted=frozenset()):
    old, new = json.loads(json.dumps(old)), json.loads(json.dumps(new))
    for key in ("commands", "tools"):
        if key in accepted and not new[key]:
            continue
        before, after = named(old[key]), named(new[key])
        for name, value in before.items():
            if f"{key}.{name}" in accepted and name not in after:
                continue
            require(name in after, f"{key}.{name}: removed")
            if key == "commands":
                value["args"] = named(value["args"])
                after[name]["args"] = named(after[name]["args"])
                for arg, spec in after[name]["args"].items():
                    require(arg in value["args"] or not spec["required"], f"{name}.{arg}: new required argument")
            subset(value, after[name], f"{key}.{name}", accepted)
    for key in ("schema", "rows", "session"):
        subset(old[key], new[key], key, accepted)
    for route, response in old["routes"].items():
        if f"routes.{route}" in accepted and route not in new["routes"]:
            continue
        require(route in new["routes"], f"HTTP route removed: {route}")
        # A baseline 404 is a route it never had or a fixture it lacks; anything else must hold.
        require(response["status"] == 404 or response["status"] == new["routes"][route]["status"]
                or f"routes.{route}.status" in accepted,
                f"HTTP status changed: {route}: {response['status']} -> {new['routes'][route]['status']}")


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--baseline", required=True, type=Path, help="the released cv binary")
    parser.add_argument("--candidate", required=True, type=Path, help="the cv binary under test")
    parser.add_argument("--accept", action="append", default=[], metavar="PATH",
                        help="a reviewed removal's contract path, allowed once against this baseline")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="cv-compat-") as root:
        after = collect(args.candidate.resolve(strict=True), Path(root) / "candidate")
        before = collect(args.baseline.resolve(strict=True), Path(root) / "baseline", routes=after["inventory"])
        compare(before, after, frozenset(args.accept))
    print(f"{after['version']} is compatible with {before['version']}: {len(before['commands'])} CLI commands, "
          f"{len(before['tools'])} MCP tools, {len(before['routes'])} HTTP patterns; schema, real JSON and exits")


if __name__ == "__main__":
    try:
        main()
    except (Incompatible, subprocess.TimeoutExpired) as error:
        raise SystemExit(str(error))
