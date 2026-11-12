#!/usr/bin/env python3
"""Report product/test size drift against the last release, and check repository hygiene.

The baseline is the tree at tag `v<workspace version>` (Cargo.toml still says the last release
during development), read from git's blobs: nothing is committed and nothing is refreshed per
commit. Drift is printed as GitHub `::notice::` annotations and never fails; a module growing is a
question for design review, not a failure. Hygiene (committed measurement artifacts) still fails.
Without the tag (a shallow clone) the drift is not measured:
`git fetch --no-tags --depth=1 origin refs/tags/<tag>:refs/tags/<tag>` brings it.

Product lines: non-blank lines of crates/<crate>/src/**/*.rs outside `#[cfg(test)]` items (a test
module, import or helper is skipped to its matching brace or `;`, with strings, raw strings, char
literals and comments masked), excluding tests.rs files, `tests` directories and files declared
only as `#[cfg(test)] mod` (including `#[path]` ones).
Run from the repository root: python3 -I tools/check-size.py"""

import pathlib
import posixpath
import re
import subprocess
import sys

CFG_TEST = re.compile(r"#\[(cfg\((test|all\(test\b[^\]]*)\)|test)\]")  # cfg(test) items and #[test] fns
MOD_DECL = re.compile(r"\bmod\s+(\w+)\s*;")
RAW_STRING = re.compile(r'b?r(#*)"')
CHAR_LITERAL = re.compile(r"'(\\.[^']*|[^\\'])'")
PATH_ATTR = re.compile(r'#\[path\s*=\s*"([^"]+)"\]')


def code_mask(text: str) -> list[bool]:
    """Per character: True where it is code (not inside a comment, string or char literal)."""
    mask = [True] * len(text)
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
        elif (m := RAW_STRING.match(text, i)) and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
            close = '"' + m.group(1)
            j = text.find(close, m.end())
            j = n if j < 0 else j + len(close)
        elif c == '"' or text.startswith('b"', i) and (i == 0 or not text[i - 1].isalnum()):
            j = i + (2 if c == "b" else 1)
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            j += 1
        elif c == "'" and (m := CHAR_LITERAL.match(text, i)):
            j = m.end()  # a char literal; a lifetime has no closing quote and stays code
        else:
            i += 1
            continue
        for k in range(i, min(j, n)):
            mask[k] = False
        i = j
    return mask


def item_end(text: str, mask: list[bool], start: int) -> int:
    """End of the item, field or variant that begins at `start`: its matching brace, or its `;`/`,`."""
    depth, i = 0, start
    while i < len(text):
        if mask[i]:
            ch = text[i]
            if ch in "{([":
                depth += 1
            elif ch in "})]":
                if depth == 0:
                    return i  # the enclosing item closes: a field or variant ends here
                depth -= 1
                if depth == 0 and ch == "}":
                    return i + 1
            elif ch in ";," and depth == 0:
                return i + 1  # an item, field, variant or argument
        i += 1
    return len(text)


def strip_test_items(path: str, text: str, excluded: set[str]) -> str:
    """The file's text without `#[cfg(test)]` items; test-only module files go into `excluded`."""
    mask = code_mask(text)
    out, pos = [], 0
    name, parent = posixpath.basename(path), posixpath.dirname(path)
    for m in CFG_TEST.finditer(text):
        if m.start() < pos or not mask[m.start()]:
            continue
        end = item_end(text, mask, m.end())
        item = text[m.end():end]
        if decl := MOD_DECL.search(item):
            attr = PATH_ATTR.search(item)
            base = parent if name in ("mod.rs", "lib.rs", "main.rs") else path.removesuffix(".rs")
            if attr:
                excluded.add(posixpath.normpath(posixpath.join(parent, attr.group(1))))
            else:
                excluded.add(f"{base}/{decl.group(1)}.rs")
                excluded.add(f"{base}/{decl.group(1)}/mod.rs")
        out.append(text[pos:m.start()])
        pos = end
    out.append(text[pos:])
    return "".join(out)


def counted(path: str) -> bool:
    """`crates/<crate>/src/**/*.rs` and `crates/<crate>/tests/**/*.rs`."""
    parts = path.split("/")
    return len(parts) > 3 and parts[0] == "crates" and parts[2] in ("src", "tests") and path.endswith(".rs")


def working_tree() -> dict[str, str]:
    return {p.as_posix(): p.read_text(encoding="utf-8", errors="replace")
            for p in sorted(pathlib.Path("crates").rglob("*.rs")) if counted(p.as_posix())}


def tag_tree(tag: str) -> dict[str, str] | None:
    """Every counted file at `tag`, from git's blobs; None when the tag is not in this clone."""
    listed = subprocess.run(["git", "ls-tree", "-r", "--name-only", tag, "--", "crates"], capture_output=True, text=True)
    if listed.returncode != 0:
        return None
    paths = [path for path in listed.stdout.splitlines() if counted(path)]
    blobs = subprocess.run(["git", "cat-file", "--batch"], input="".join(f"{tag}:{p}\n" for p in paths).encode(),
                           capture_output=True, check=True).stdout
    files, at = {}, 0
    for path in paths:
        header_end = blobs.index(b"\n", at)
        size = int(blobs[at:header_end].split()[2])
        files[path] = blobs[header_end + 1:header_end + 1 + size].decode("utf-8", errors="replace")
        at = header_end + 1 + size + 1
    return files


def module_key(crate: str, path: str) -> str:
    """`crate:module` where a module is a top-level file or directory under src, and `harness/*`
    and `cmd/*` split one level deeper (an adapter or a command is the unit a lane owns)."""
    parts = path.split("/")[3:]
    key = "/".join(parts[:2]) if parts[0] in ("harness", "cmd") and len(parts) > 1 else parts[0]
    return f"{crate}:{key.removesuffix('.rs')}"


def measure(files: dict[str, str]) -> dict[str, int]:
    """Product lines per crate and per module (`crate:module`), test lines per crate (`crate:tests`)."""
    sizes: dict[str, int] = {}
    excluded: set[str] = set()
    stripped = {path: strip_test_items(path, text, excluded) for path, text in files.items() if path.split("/")[2] == "src"}
    for path, text in files.items():
        crate = path.split("/")[1]
        every = sum(1 for line in text.splitlines() if line.strip())
        product = 0
        if path in stripped and posixpath.basename(path) != "tests.rs" and "tests" not in path.split("/")[3:-1] and path not in excluded:
            product = sum(1 for line in stripped[path].splitlines() if line.strip())
            sizes[module_key(crate, path)] = sizes.get(module_key(crate, path), 0) + product
        sizes[crate] = sizes.get(crate, 0) + product
        sizes[f"{crate}:tests"] = sizes.get(f"{crate}:tests", 0) + every - product
    return sizes


def tracked(*patterns: str) -> list[str]:
    out = subprocess.run(["git", "ls-files", "--", *patterns], capture_output=True, text=True, check=True)
    return [line for line in out.stdout.splitlines() if line]


def main() -> int:
    version = re.search(r'^version = "([^"]+)"', pathlib.Path("Cargo.toml").read_text(), re.M)
    tag = f"v{version.group(1)}"
    now = measure(working_tree())
    base = tag_tree(tag)
    if base is None:
        print(f"::warning::size drift not measured: tag {tag} is not in this clone "
              f"(git fetch --no-tags --depth=1 origin refs/tags/{tag}:refs/tags/{tag})")
    else:
        before = measure(base)
        for name in sorted(now.keys() | before.keys()):
            lines, previous = now.get(name, 0), before.get(name, 0)
            if lines != previous:
                print(f"::notice title=size {name}::{lines} lines ({lines - previous:+d} since {tag})")
    for crate in sorted(name for name in now if ":" not in name):
        since = f" ({now[crate] - before.get(crate, 0):+d} since {tag})" if base is not None else ""
        print(f"{crate:12} {now[crate]:7} product lines{since}")

    failures: list[str] = []
    for path in tracked("docs/*.json", "docs/**/*.json", "tools/profiling/results/*"):
        failures.append(f"{path}: measurements belong in commit messages, not committed files")
    for failure in failures:
        print(f"FAIL {failure}", file=sys.stderr)
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
