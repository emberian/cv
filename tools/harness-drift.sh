#!/usr/bin/env bash
# harness-drift.sh — does upstream still write what formats/<harness>.toml says it writes?
#
# The 2026-09-19 survey did this by hand and found three harnesses whose live store had moved out
# from under cv (OpenClaw → sqlite, OpenCode → opencode.db, Kimi → ~/.kimi-code) with no error from
# any adapter. This makes it a command. For each harness with a checkout under $PUG:
#
#   1. record HEAD and show it next to the commit the manifest claims it was verified against
#      (`[upstream] commit`);
#   2. grep every type the manifest names out of the checkout — a type that is GONE upstream means
#      the manifest (and probably the adapter) is carrying dead weight;
#   3. harvest the vocabulary around the types that ARE there and print what the manifest does not
#      name — that is the new stuff.
#
# READ-ONLY BY DEFAULT. It never writes inside the cv repo, and it does not touch a checkout at all
# unless you pass --pull: other work shares those trees, and a fast-forward under a lane that is
# mid-bisect or mid-edit is not a thing a reporting tool should do on its own.
#
#   tools/harness-drift.sh                 # every harness, checkouts exactly as they are
#   tools/harness-drift.sh goose hermes    # just these
#   tools/harness-drift.sh --pull          # `git pull --ff-only` each checkout first
#
# Env: PUG (default ~/pug), CV_REPO (default: the repo this script lives in).

set -uo pipefail

PUG="${PUG:-$HOME/pug}"
CV_REPO="${CV_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
FORMATS="$CV_REPO/formats"
PULL=0

# harness(manifest stem) : checkout dir under $PUG : paths inside the checkout that hold the
# persistence vocabulary (a prefix match; empty = the whole tree).
# Verified against the 2026-09-19 checkouts. When upstream moves a directory the run says
# "gone upstream: <path>" and falls back to the whole checkout, so a stale entry degrades to
# slow-but-correct rather than to a wrong answer — but fix it when you see it.
CHECKOUTS=(
  "codex:codex:codex-rs/history/src codex-rs/rollout/src codex-rs/protocol/src codex-rs/thread-store/src"
  "gemini:gemini-cli:packages/core/src/services packages/core/src/utils"
  "qwen:gemini-cli:packages/core/src/services packages/core/src/utils"
  "goose:goose:crates/goose/src/session crates/goose/src/agents crates/goose-provider-types/src/conversation crates/goose-provider-types/src/conversation.rs"
  "hermes:hermes-agent:hermes_state.py hermes_state_common.py hermes_state_ids.py agent hermes_cli"
  "kimi:kimi-cli:src/kimi_cli"
  "openclaw:openclaw:src/config/sessions src/agents/sessions src/transcripts"
  "opencode:opencode:packages/opencode/src/session packages/opencode/src/storage"
)

WANT=()
for a in "$@"; do
  case "$a" in
    --pull) PULL=1 ;;
    --no-pull) PULL=0 ;;   # the default; still accepted so old invocations keep working
    -h|--help) sed -n '2,23p' "${BASH_SOURCE[0]}"; exit 0 ;;
    -*) echo "unknown flag: $a" >&2; exit 2 ;;
    *) WANT+=("$a") ;;
  esac
done

command -v git >/dev/null || { echo "no git on PATH" >&2; exit 1; }
[ -d "$FORMATS" ] || { echo "no manifests at $FORMATS (set CV_REPO)" >&2; exit 1; }
[ -d "$PUG" ] || { echo "no checkout root at $PUG (set PUG) — nothing to compare against" >&2; exit 1; }

# A name nobody knows used to produce an empty report and exit 0, which reads exactly like "no
# drift". Say what the known names are instead.
KNOWN=$(printf '%s\n' "${CHECKOUTS[@]}" | cut -d: -f1)
for w in ${WANT[@]+"${WANT[@]}"}; do
  printf '%s\n' "$KNOWN" | grep -qx "$w" || {
    echo "unknown harness: $w" >&2
    echo "known: $(printf '%s ' $KNOWN)" >&2
    exit 2
  }
done

# Vocabulary noise: words that are type-shaped but mean nothing on their own. Kept tiny and
# explicit — a stoplist that grows is a checker that stops working.
STOP='^(true|false|null|string|number|boolean|object|array|type|id|name|text|data|content|value|error|default|undefined|function|const|let|var|return|import|export|async|await|self|None|True|False)$'

hr() { printf '─%.0s' $(seq 1 78); echo; }

# `"a.b" = { … }` / `a_b = { … }` from the [types] table of a manifest → the wire spelling
# (everything after the first dot; see the key grammar in cv-core/src/formats.rs).
manifest_types() {
  awk '/^\[types\]/{t=1;next} /^\[/{t=0} t' "$1" |
    sed -n 's/^[[:space:]]*"\{0,1\}\([A-Za-z0-9_.*\-]*\)"\{0,1\}[[:space:]]*=.*/\1/p' |
    sed 's/^[A-Za-z0-9_]*\.//' | grep -v '\*$' | sort -u
}

manifest_upstream() {
  sed -n 's/^commit[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$1" | head -1
}

for row in "${CHECKOUTS[@]}"; do
  IFS=: read -r harness dir subpaths <<<"$row"
  if [ ${#WANT[@]} -gt 0 ] && ! printf '%s\n' "${WANT[@]}" | grep -qx "$harness"; then continue; fi
  manifest="$FORMATS/$harness.toml"
  hr; echo "## $harness"
  if [ ! -f "$manifest" ]; then echo "   no manifest at $manifest — skipped"; continue; fi
  co="$PUG/$dir"
  if [ ! -d "$co/.git" ]; then echo "   no checkout at $co — skipped (clone it under \$PUG to include it)"; continue; fi

  before=$(git -C "$co" rev-parse --short HEAD 2>/dev/null)
  if [ "$PULL" = 1 ]; then
    if ! out=$(git -C "$co" pull --ff-only 2>&1); then
      echo "   pull --ff-only failed (left untouched): $(printf '%s' "$out" | tail -1)"
    fi
  fi
  after=$(git -C "$co" rev-parse --short HEAD 2>/dev/null)
  when=$(git -C "$co" log -1 --format=%cs 2>/dev/null)
  claimed=$(manifest_upstream "$manifest")
  echo "   checkout $co"
  if [ "$PULL" = 1 ] && [ "$before" != "$after" ]; then
    echo "   HEAD     $before → $after ($when)   manifest [upstream] commit = ${claimed:-unset}"
  else
    echo "   HEAD     $after ($when)   manifest [upstream] commit = ${claimed:-unset}"
  fi
  # `commit = "..."` may carry a note ("86f1364 (store frozen since 2026-06)"), and the two short
  # hashes need not be the same length, so compare the leading hex token as a prefix either way.
  pinned=${claimed%% *}
  if [ -n "$pinned" ] && [ "${after#"$pinned"}" = "$after" ] && [ "${pinned#"$after"}" = "$pinned" ]; then
    echo "   ⚠ manifest is pinned to $claimed; re-verify the adapter and bump [upstream]"
  fi

  # Where to look. A named subpath that no longer exists is itself a drift signal.
  roots=()
  if [ -n "${subpaths// /}" ]; then
    for p in $subpaths; do
      if [ -e "$co/$p" ]; then roots+=("$co/$p"); else echo "   ⚠ gone upstream: $p"; fi
    done
  fi
  [ ${#roots[@]} -eq 0 ] && roots=("$co")

  # (2) every manifest type, grepped out of the source. A miss in the named subpaths is retried
  # across the whole checkout before it is called GONE — upstream moves files constantly, and a
  # false "GONE" is worse than a slow one.
  present=$(mktemp); gone=$(mktemp); files=$(mktemp)
  # shellcheck disable=SC2064  # expand now: the trap must name THIS iteration's files
  trap "rm -f '$present' '$gone' '$files'" EXIT
  while IFS= read -r ty; do
    [ -z "$ty" ] && continue
    hits=$(grep -rlF --binary-files=without-match --exclude-dir=.git --exclude-dir=node_modules \
             --exclude-dir=target --exclude-dir=dist -- "$ty" "${roots[@]}" 2>/dev/null)
    if [ -n "$hits" ]; then
      echo "$ty" >>"$present"; printf '%s\n' "$hits" >>"$files"
    elif grep -rqlF --binary-files=without-match --exclude-dir=.git --exclude-dir=node_modules \
             --exclude-dir=target --exclude-dir=dist -- "$ty" "$co" 2>/dev/null; then
      echo "$ty" >>"$present"
      echo "     MOVED $ty (outside the manifest subpaths)"
    else
      echo "$ty" >>"$gone"
    fi
  done < <(manifest_types "$manifest")
  n_present=$(wc -l <"$present" | tr -d ' ')
  n_gone=$(wc -l <"$gone" | tr -d ' ')
  echo "   manifest types: $n_present still in the source, $n_gone not found"
  [ "$n_gone" -gt 0 ] && sed 's/^/     GONE  /' "$gone"

  # (3) the vocabulary around them. The persistence file is the one that mentions the MOST of the
  # manifest's types — that is the tagged union itself — so rank by that, drop tests, and keep the
  # top 40. (A flat "file holds >= 2 known types" bar admits 3317 files in a repo the size of
  # OpenClaw's, because `user` and `text` are everywhere.)
  hot=$(grep -vE '(\.|/|_)(test|spec)s?[./]|/__tests__/|/tests?/|/fixtures?/' "$files" |
        sort | uniq -c | sort -rn | awk '$1>=3{ $1=""; sub(/^ /,""); print }' | head -40)
  if [ -z "$hot" ]; then
    echo "   no non-test file holds three or more manifest types — nothing to harvest"
  else
    echo "   persistence files (top by manifest-type density): $(printf '%s\n' "$hot" | wc -l | tr -d ' ')"
    known=$(mktemp); { manifest_types "$manifest"; } | sort -u >"$known"
    # Only literals in TAG position count: a serde rename, a discriminant assignment
    # (`type: "x"`, `"kind": "x"`), a union/match arm (`| "x"`, `"x" =>`), a zod/Literal member.
    # This is the same narrowness `cv_core::formats::type_literals` uses on cv's own side — a
    # harvest of every string in the file is a harvest of nothing.
    TAG='(rename|tag|type|kind|subtype|actionType|event|status)["'"'"']?[[:space:]]*[:=][[:space:]]*'
    novel=$(printf '%s\n' "$hot" | tr '\n' '\0' | xargs -0 grep -hoE \
        -e "$TAG\"[a-zA-Z][a-zA-Z0-9_.-]*\"" \
        -e "$TAG'[a-zA-Z][a-zA-Z0-9_.-]*'" \
        -e '\|[[:space:]]*"[a-zA-Z][a-zA-Z0-9_.-]*"' \
        -e '"[a-zA-Z][a-zA-Z0-9_.-]*"[[:space:]]*=>' \
        -e 'literal\("[a-zA-Z][a-zA-Z0-9_.-]*"\)' \
        -e 'Literal\[[^]]*\]' 2>/dev/null |
      grep -oE '["'"'"'][a-zA-Z][a-zA-Z0-9_.-]*["'"'"']' | tr -d '"'"'"'"' |
      grep -vE "$STOP" | sort | uniq -c | sort -rn |
      awk -v k="$known" 'BEGIN{while((getline l<k)>0) seen[l]=1} !seen[$2]{print}')
    if [ -z "$novel" ]; then
      echo "   no vocabulary outside the manifest"
    else
      echo "   vocabulary the manifest does not name (count, token) — top 40:"
      printf '%s\n' "$novel" | head -40 | sed 's/^/     /'
    fi
    rm -f "$known"
  fi
  rm -f "$present" "$gone" "$files"
done
hr
echo "Source-side check (adapter vs manifest, no checkouts needed): cargo run -p clustervision -- formats check"
echo "Data-side check (real sessions vs manifest):                  cargo run -p clustervision -- formats census"
