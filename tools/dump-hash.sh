#!/usr/bin/env bash
# dump-hash.sh — the byte-identity gate: one sha256 line per (command, session, demand), streamed.
#
#   tools/dump-hash.sh --cv BIN --scratch DIR [--probe BIN] [--probe-positioned BIN]
#                      [--manifest FILE] [--home DIR]... [--only NAME]... > hashes.txt
#
# Over every root the manifest names (tools/dump-hash.manifest: the committed corpus versions,
# the synthetic edges, and any `--home DIR` of real sessions), stage a clone of the store under
# DIR/stage/<root>/home, point cv at it with the testkit's World environment, and hash the stdout
# of each command below. Nothing rendered is kept: every stream goes straight into sha256, and
# the stage is rebuilt from the sources on each run, so two runs of one binary must print equal
# line sets, and a run of a second binary differs exactly where its output differs.
#
# Line shape (sorted, LC_ALL=C):   <sha256>  <command>  <session>  <demand>
#   command   the cv argv with the session id elided (`show --last 3`); a nonzero exit appends
#             `!<code>`; a produced file is `file:<kind>`; the probe is `probe:messages|metadata`
#   session   `<root>` for fleet commands, `<root>/<harness>:<id>` for one session
#   demand    `-`, or `<mode>/<build>` for the CODEC.md stream dump (lazy, lazy-extra, facts,
#             bulk, full, complete × mmap, positioned)
#
# DIR/cvhome/<root> is the one scratch CLUSTERVISION_HOME per root, reused across runs (a warm
# catalog and index must read the same as a cold one; `index` itself is run, never hashed).
# Masked as build identity, not output: `rewind`'s provenance `derived_at` (the clock) and `cv_version`
# (the binary's `git describe`, which differs between any two commits); the `--scratch` path is masked to
# `<scratch>` in every hashed stream, so two runs of one binary agree whatever `--scratch` names. `pack` prints the day it ran.
set -euo pipefail

usage() { sed -n '2,25p' "$0" >&2; exit 2; }

CV= PROBE= PROBE_POS= SCRATCH= MANIFEST=
HOMES=() ONLY=()
while [ $# -gt 0 ]; do
  case "$1" in
    --cv) CV=$2; shift 2 ;;
    --probe) PROBE=$2; shift 2 ;;
    --probe-positioned) PROBE_POS=$2; shift 2 ;;
    --scratch) SCRATCH=$2; shift 2 ;;
    --manifest) MANIFEST=$2; shift 2 ;;
    --home) HOMES+=("$2"); shift 2 ;;
    --only) ONLY+=("$2"); shift 2 ;;
    *) usage ;;
  esac
done
[ -n "$CV" ] && [ -n "$SCRATCH" ] || usage
REPO=$(cd "$(dirname "$0")/.." && pwd)
MANIFEST=${MANIFEST:-$REPO/tools/dump-hash.manifest}
CORPUS=$REPO/crates/cv-core/tests/corpus
CV=$(cd "$(dirname "$CV")" && pwd)/$(basename "$CV")
[ -z "$PROBE" ] || PROBE=$(cd "$(dirname "$PROBE")" && pwd)/$(basename "$PROBE")
[ -z "$PROBE_POS" ] || PROBE_POS=$(cd "$(dirname "$PROBE_POS")" && pwd)/$(basename "$PROBE_POS")
mkdir -p "$SCRATCH"
SCRATCH=$(cd "$SCRATCH" && pwd)
STAGE=$SCRATCH/stage
MASK="s#$SCRATCH#<scratch>#g"   # the scratch dir is where, not what: it leaks into paths cv prints
rm -rf "$STAGE"
mkdir -p "$STAGE" "$SCRATCH/cvhome"
OUT=$(mktemp "$SCRATCH/lines.XXXXXX")
trap 'rm -f "$OUT"' EXIT

if command -v sha256sum >/dev/null 2>&1; then SHA="sha256sum"; else SHA="shasum -a 256"; fi
CP="cp -Rp"; cp -c "$MANIFEST" "$OUT.probe" 2>/dev/null && CP="cp -Rpc"; rm -f "$OUT.probe"   # APFS clones
MODES="lazy lazy-extra facts bulk full complete"
QUERIES=("error" "file" "test" "the" "tool result")
PACK_TASK="fix the parser and add a test"

# The testkit's World: cv sees only the staged home; every harness override is cleared.
for v in CV_ENDPOINT CLAUDE_CONFIG_DIR CLUSTERVISION_CLAUDE_ROOTS CLUSTERVISION_MAX_STALE_SECS \
         CURSOR_USER_DIR CURSOR_CONFIG_DIR CODEX_HOME HERMES_HOME KIMI_SHARE_DIR KIMI_CODE_HOME \
         CONTINUE_GLOBAL_DIR GOOSE_PATH_ROOT OPENCLAW_STATE_DIR OPENCODE_DB CLINE_SESSION_DATA_DIR \
         CLINE_DATA_DIR CLINE_DIR JETSKI_APP_DATA_DIR CV_EXPORTS CLAURDVOYANT_MAX_STALE_SECS \
         CV_PACK_LLM CV_SERVE_TOKEN CVD_TOKEN; do unset "$v"; done
export NO_COLOR=1 TERM=dumb

wanted() { # --only filters by root name
  [ ${#ONLY[@]} -eq 0 ] && return 0
  local o; for o in "${ONLY[@]}"; do [ "$1" = "$o" ] && return 0; done; return 1
}

# emit <command> <session> <demand> -- argv…   : hash stdout, record the exit code
emit() {
  local command=$1 session=$2 demand=$3; shift 3
  local sum code
  set +e
  sum=$("$@" </dev/null 2>/dev/null | LC_ALL=C sed "$MASK" | $SHA | cut -c1-64; exit "${PIPESTATUS[0]}"); code=$?
  set -e
  [ "$code" -eq 0 ] || command="$command!$code"
  printf '%s  %s  %s  %s\n' "$sum" "$command" "$session" "$demand" >>"$OUT"
}
# emit_file <kind> <session> <path> [sed-filter]: hash a file the reshape produced (missing =
# `!missing`); the filter masks a line the file stamps with the wall clock
emit_file() {
  local kind=$1 session=$2 path=$3 filter=${4:-} sum
  if [ -f "$path" ]; then sum=$(sed "${filter:-p;d}" "$path" | LC_ALL=C sed "$MASK" | $SHA | cut -c1-64); else sum=$(printf '%064d' 0); kind="$kind!missing"; fi
  printf '%s  file:%s  %s  -\n' "$sum" "$kind" "$session" >>"$OUT"
}

# ---- staging -------------------------------------------------------------------------------
# A corpus version dir is a home (`.claude`, `.codex`, `.local/share/…`, `Cursor/User` …) except
# where the harness keeps its store elsewhere: Goose's `sessions/`, OpenClaw's state dir, the
# Cursor IDE user dir. Snapshot-only versions (lmstudio, zed) hold no sessions and are skipped.
stage_corpus() { # <harness> <version> <home>
  local src=$CORPUS/$1/$2 home=$3
  [ -f "$src/SNAPSHOT.toml" ] && return 1
  mkdir -p "$home"
  case "$1" in
    goose) mkdir -p "$home/.local/share/goose"; $CP "$src/sessions" "$home/.local/share/goose/sessions" ;;
    openclaw) $CP "$src" "$home/.openclaw" ;;
    cursor) if [ -d "$src/Cursor/User" ]; then
              mkdir -p "$home/Library/Application Support/Cursor"
              $CP "$src/Cursor/User" "$home/Library/Application Support/Cursor/User"
            else $CP "$src/.cursor" "$home/.cursor"; fi ;;
    *) local e; for e in "$src"/.[!.]* "$src"/*; do
         case "$(basename "$e")" in MANIFEST.toml|summary.json|hooks.jsonl|workflows.json) ;; *) $CP "$e" "$home/"; esac
       done ;;
  esac
}

# The probe reads a store through `harness::for_harness_with_root`: where each harness's root sits
# in a staged home (the layout its detection finds).
probe_root() { # <home> <harness> <id> <path>
  case "$2" in
    antigravity) echo "$1/.gemini" ;;
    claude) echo "$1/.claude" ;;            codex) echo "$1/.codex" ;;
    gemini) echo "$1/.gemini" ;;            qwen) echo "$1/.qwen" ;;
    grok) echo "$1/.grok" ;;                kimi) echo "$1/.kimi" ;;
    kimi-code) echo "$1/.kimi-code" ;;      hermes) echo "$1/.hermes" ;;
    continue) echo "$1/.continue" ;;        cline|roo) echo "$1" ;;
    openclaw) echo "$1/.openclaw" ;;        opencode) echo "$1/.local/share/opencode" ;;
    goose) echo "$1/.local/share/goose/sessions" ;;
    devin) echo "$1/.local/share/devin/cli" ;;
    lmstudio) echo "$1/.lmstudio/conversations" ;;
    cursor) case "$3" in cli:*) echo "$1/.cursor" ;; *) echo "$1/Library/Application Support/Cursor/User" ;; esac ;;
    chatgpt-export|claude-export|opensession) dirname "$4" ;;
    *) return 1 ;;
  esac
}

# Claude-format edges the corpus cannot hold: a giant record (over the 8 MiB mapped window), an
# escape-heavy body, a malformed line beside a blank one, and a live file cut mid-record.
stage_synthetic() { # <home> <edge>…
  python3 -I - "$@" <<'EOF'
import json, sys, pathlib
home, edges = pathlib.Path(sys.argv[1]), sys.argv[2:]
proj = home / ".claude/projects/-synthetic-project"
proj.mkdir(parents=True, exist_ok=True)
T = "2026-01-02T03:04:{:02d}.000Z"
def rec(sid, n, kind, content, parent, **extra):
    r = dict(type=kind, uuid=f"{sid[:8]}-0000-4000-8000-{n:012d}", parentUuid=parent, sessionId=sid,
             cwd="/synthetic/project", version="2.1.296", timestamp=T.format(n), isSidechain=False,
             userType="external", message=dict(role=kind, content=content))
    if kind == "assistant":
        r["message"].update(model="claude-fable-5-1", id=f"msg_{n:04d}", usage=dict(input_tokens=100 + n, output_tokens=10))
    r.update(extra)
    return r
def exchange(sid, payload):
    u = rec(sid, 1, "user", "run the edge", None)
    a = rec(sid, 2, "assistant", [dict(type="text", text="running"), dict(type="tool_use", id="toolu_edge", name="Bash", input=dict(command="edge"))], u["uuid"])
    t = rec(sid, 3, "user", [dict(type="tool_result", tool_use_id="toolu_edge", content=payload)], a["uuid"], toolUseResult=dict(stdout=payload[:64]))
    b = rec(sid, 4, "assistant", [dict(type="text", text="done: " + payload[:32])], t["uuid"])
    return [u, a, t, b]
for edge in edges:
    sid = {"giant": "11111111-1111-4111-8111-000000000001", "escaped": "11111111-1111-4111-8111-000000000002",
           "malformed": "11111111-1111-4111-8111-000000000003", "live": "11111111-1111-4111-8111-000000000004"}[edge]
    if edge == "giant":
        lines = exchange(sid, "".join(f"line {i:08d} of a giant tool result\n" for i in range(300_000)))
    elif edge == "escaped":
        body = ("tab\t quote\" backslash\\ nul\x00 bell\x07 é ü 中文 \U0001F600     </script> \\n literal "
                "\r\n crlf " + "\\\"" * 2000 + "\n" * 500 + "\U0001F9EC" * 3000)
        lines = exchange(sid, body)
    elif edge == "malformed":
        lines = exchange(sid, "ok")
        lines = lines[:2] + ['{"type":"user","uuid":"broken', "", "not json at all", json.dumps({"type": "unknown", "x": 1})] + lines[2:]
    else:
        lines = exchange(sid, "ok")
    text = "\n".join(l if isinstance(l, str) else json.dumps(l, ensure_ascii=False) for l in lines) + "\n"
    if edge == "live":
        text = text[: len(text) // 2 + len(text) // 4]   # cut inside the last record, no newline
    (proj / f"{sid}.jsonl").write_text(text, encoding="utf-8")
EOF
}

# ---- the commands ----------------------------------------------------------------------------
run_root() { # <root> <home>
  local root=$1 home=$2 out=$STAGE/$root/out
  mkdir -p "$out" "$SCRATCH/cvhome/$root"
  export HOME=$home USERPROFILE=$home APPDATA=$home/.config LOCALAPPDATA=$home/.local/share \
         XDG_CACHE_HOME=$home/.cache XDG_CONFIG_HOME=$home/.config XDG_DATA_HOME=$home/.local/share \
         XDG_STATE_HOME=$home/.local/state CLUSTERVISION_HOME=$SCRATCH/cvhome/$root
  cd "$STAGE/$root"
  echo "== $root" >&2

  emit "ls --json" "$root" - "$CV" ls --json --limit 100000
  emit "ls --json --enrich" "$root" - "$CV" ls --json --enrich --limit 100000
  emit "stats --json" "$root" - "$CV" stats --json
  emit "stats --tokens --json" "$root" - "$CV" stats --tokens --json
  "$CV" index >/dev/null 2>&1 || echo "   index failed for $root" >&2
  local q; for q in "${QUERIES[@]}"; do
    emit "search --json '$q'" "$root" - "$CV" search --json --limit 50 "$q"
    emit "search --json --kind all '$q'" "$root" - "$CV" search --json --kind all --limit 50 "$q"
  done
  emit "pack" "$root" - "$CV" pack "$PACK_TASK"
  emit "pack --format prompt" "$root" - "$CV" pack --format prompt "$PACK_TASK"

  # harness, id, path, dir-of-first-agent-or-empty, first-agent-id-or-empty: per listed session
  local rows; rows=$("$CV" ls --json --limit 100000 2>/dev/null | python3 -I -c '
import json, sys, pathlib
for r in sorted(json.load(sys.stdin), key=lambda r: (r["harness"], r["id"])):
    p = pathlib.Path(r["path"]); agents = sorted((p.with_suffix("") / "subagents").glob("agent-*.jsonl")) if r["harness"] == "claude" else []
    a = agents[0].name[len("agent-"):-len(".jsonl")] if agents else ""
    print(r["harness"], r["id"], r["path"], a, sep="\t")')
  local harness id path agent
  while IFS=$'\t' read -r harness id path agent; do
    [ -n "$id" ] || continue
    local ref="$harness:$id" s="$root/$harness:$id"
    local w; for w in "" "--first 3" "--last 3" "--range 1..4" "--around 2 --context 1"; do
      # shellcheck disable=SC2086
      emit "show${w:+ $w}" "$s" - "$CV" show "$ref" $w
      # shellcheck disable=SC2086
      emit "show --json${w:+ $w}" "$s" - "$CV" show --json "$ref" $w
    done
    emit "show --tree" "$s" - "$CV" show --tree "$ref"
    emit "show --kind affordances --json" "$s" - "$CV" show --kind affordances --json "$ref"
    emit "export --format md" "$s" - "$CV" export --format md "$ref"
    emit "export --format md --last 3" "$s" - "$CV" export --format md --last 3 "$ref"
    emit "export --format json" "$s" - "$CV" export --format json "$ref"
    emit "events" "$s" - "$CV" events "$ref"
    emit "events --json" "$s" - "$CV" events --json "$ref"
    emit "events --subagents --json" "$s" - "$CV" events --subagents --json "$ref"
    emit "tools" "$s" - "$CV" tools "$ref"
    emit "tools --json" "$s" - "$CV" tools --json "$ref"
    emit "tools --across --json" "$s" - "$CV" tools --across --json "$ref"
    emit "tools --timeline --json" "$s" - "$CV" tools --timeline --json "$ref"
    emit "doctor --json" "$s" - "$CV" doctor --json "$ref"

    local b; for b in mmap positioned; do
      local probe=$PROBE; [ "$b" = positioned ] && probe=$PROBE_POS
      [ -n "$probe" ] || continue
      local proot; proot=$(probe_root "$home" "$harness" "$id" "$path") || continue
      local m; for m in $MODES; do
        local j; j=$("$probe" hash "$harness" "$id" "$path" --root "$proot" --mode "$m" 2>/dev/null) || { printf '%064d  probe!1  %s  %s/%s\n' 0 "$s" "$m" "$b" >>"$OUT"; continue; }
        printf '%s\n' "$j" | python3 -I -c '
import json, sys; d = json.load(sys.stdin)["detail"]; s, dm = sys.argv[1:]
print(d["messages_sha256"], "probe:messages", s, dm, sep="  "); print(d["metadata_sha256"], "probe:metadata", s, dm, sep="  ")' "$s" "$m/$b" >>"$OUT"
      done
    done

    [ "$harness" = claude ] || continue
    local stem; stem=$(printf '%s' "$id" | tr -d - | tail -c 12)   # the tail: synthetic ids share a head
    local pid="00000000-0000-4000-8000-$stem" rid="00000000-0000-4000-8001-$stem"
    emit "prune --json" "$s" - "$CV" prune --json --to "$pid" --force --min-size 256 --keep-last 2 "$ref"
    emit_file prune.jsonl "$s" "$(dirname "$path")/$pid.jsonl"
    emit_file prune.flat.jsonl "$s" "$(dirname "$path")/$pid.flat.jsonl"
    emit "rewind --json" "$s" - "$CV" rewind --json --to "$rid" --out "$out" "$ref"
    emit_file rewind.jsonl "$s" "$out/$rid.jsonl"
    emit_file rewind.json "$s" "$out/$rid.rewind.json" '/"derived_at"/d; /"cv_version"/d'   # clock and build identity
    [ -n "$agent" ] || continue
    local into; into=$(printf '%s\n' "$rows" | awk -F'\t' -v d="$(dirname "$path")" -v me="$id" '$1=="claude" && $2!=me && index($3, d "/")==1 && $3 !~ /subagents/ {print $2; exit}')
    [ -n "$into" ] || continue
    emit "adopt --json" "$s" - "$CV" adopt --json --from "$id" --into "$into" "$agent"
    local intodir; intodir=$(dirname "$path")/$into/subagents
    emit_file adopt.jsonl "$s" "$intodir/agent-$agent.jsonl"
    emit_file adopt.meta.json "$s" "$intodir/agent-$agent.meta.json"
  done <<<"$rows"
}

# ---- the manifest ----------------------------------------------------------------------------
SYNTHETIC=()
while read -r kind arg _; do
  case "$kind" in
    ''|'#'*) ;;
    corpus) root=${arg//\//-}; wanted "$root" || continue
            stage_corpus "${arg%%/*}" "${arg#*/}" "$STAGE/$root/home" || { echo "== $root (snapshot only, skipped)" >&2; continue; }
            run_root "$root" "$STAGE/$root/home" ;;
    synthetic) SYNTHETIC+=("$arg") ;;
    home) HOMES+=("$arg") ;;
    *) echo "dump-hash: unknown manifest line: $kind $arg" >&2; exit 2 ;;
  esac
done <"$MANIFEST"
if [ ${#SYNTHETIC[@]} -gt 0 ] && wanted synthetic; then
  stage_synthetic "$STAGE/synthetic/home" "${SYNTHETIC[@]}"
  run_root synthetic "$STAGE/synthetic/home"
fi
for h in "${HOMES[@]+"${HOMES[@]}"}"; do
  root=home-$(basename "$h"); wanted "$root" || continue
  mkdir -p "$STAGE/$root"; $CP "$h" "$STAGE/$root/home"
  run_root "$root" "$STAGE/$root/home"
done
LC_ALL=C sort "$OUT"
echo "$(wc -l <"$OUT" | tr -d ' ') lines" >&2
