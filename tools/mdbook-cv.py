#!/usr/bin/env python3
"""mdbook preprocessor: the manual's generated pages, from the running binary.

  {{#cv-commands}}     every command and argument, from `cv schema --commands --json`
  {{#cv-command-map}}  the `cv --help` groups as a table, each command linked into the reference

The clap tree is the one source of what cv can do; these two directives are how the manual says
it without a second, hand-kept list. The binary is $CV_BIN, else ../target/debug/cv relative to
the book root (mdbook runs preprocessors from there). A missing binary fails the build: a manual
without its reference is worse than no build.

    book.toml:  [preprocessor.cv]  command = "python3 ../tools/mdbook-cv.py"
"""
import json
import os
import re
import subprocess
import sys


def cv_bin():
    path = os.environ.get("CV_BIN") or os.path.join("..", "target", "debug", "cv")
    if not os.path.isfile(path):
        sys.exit(f"mdbook-cv: no cv binary at {path} (cargo build -p clustervision, or set CV_BIN)")
    return path


def inline(text, table=False):
    """One line of Markdown: newlines folded, `<`/`>` escaped outside code spans, pipes escaped in
    a table cell."""
    parts = re.split(r"(`[^`]*`)", " ".join(text.split()))
    out = []
    for p in parts:
        if not p.startswith("`"):
            p = p.replace("<", "&lt;").replace(">", "&gt;")
            # A bare `--flag` in help text is code; unquoted, smart punctuation makes it a dash.
            p = re.sub(r"(?<![\w`-])(--[a-z][a-z0-9-]*)", r"`\1`", p)
        if table:
            p = p.replace("|", "\\|")
        out.append(p)
    return "".join(out)


def arg_line(a):
    long = a.get("long", a["name"].replace("_", "-"))
    if a["kind"] == "positional":
        name = f"`<{a['name']}>`"
    elif a["kind"] == "flag":
        name = f"`--{long}`"
    else:
        name = f"`--{long} <{a['value_type']}>`"
    facts = []
    if a.get("multiple"):
        facts.append("repeatable")
    if a.get("required"):
        facts.append("required")
    if a.get("possible_values"):
        facts.append("one of " + ", ".join(f"`{v}`" for v in a["possible_values"]))
    if a.get("default") is not None and "default" not in a["help"]:
        facts.append(f"default `{a['default']}`")
    tail = f" ({'; '.join(facts)})" if facts else ""
    help_text = inline(a["help"]) if a["help"] else ""
    sep = " — " if help_text else ""
    return f"- {name}{sep}{help_text}{tail}"


def commands(schema):
    out = []
    group = None
    for c in schema:
        sub = " " in c["name"]
        if not sub and c["group"] != group:
            group = c["group"]
            out.append(f"## {group}\n")
        out.append(f"{'####' if sub else '###'} `cv {c['name']}`\n")
        out.append(inline(c["about"]) + "\n")
        if c["args"]:
            out.extend(arg_line(a) for a in c["args"])
            out.append("")
    return "\n".join(out)


def command_map(schema):
    groups = {}
    for c in schema:
        if " " not in c["name"]:
            groups.setdefault(c["group"], []).append(c["name"])
    out = ["| Group | Commands |", "| --- | --- |"]
    for g, names in groups.items():
        links = " ".join(f"[`{n}`](reference.md#cv-{n})" for n in names)
        out.append(f"| **{g}** | {links} |")
    return "\n".join(out) + "\n"


def walk(items, fn):
    for item in items:
        ch = item.get("Chapter") if isinstance(item, dict) else None
        if ch:
            ch["content"] = fn(ch["content"])
            walk(ch.get("sub_items", []), fn)


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "supports":
        sys.exit(0)
    _context, book = json.load(sys.stdin)
    cv = cv_bin()
    cache = {}

    def schema():
        if "schema" not in cache:
            out = subprocess.run([cv, "schema", "--commands", "--json"], capture_output=True, text=True, check=True)
            cache["schema"] = json.loads(out.stdout)
        return cache["schema"]

    def expand(content):
        if "{{#cv-commands}}" in content:
            content = content.replace("{{#cv-commands}}", commands(schema()))
        if "{{#cv-command-map}}" in content:
            content = content.replace("{{#cv-command-map}}", command_map(schema()))
        return content

    walk(book.get("items", book.get("sections", [])), expand)
    json.dump(book, sys.stdout)


if __name__ == "__main__":
    main()
