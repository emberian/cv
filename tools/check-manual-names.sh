#!/usr/bin/env bash
# Every `cv <cmd>` the manual mentions must exist in `cv schema --commands --json`.
#
#   tools/check-manual-names.sh [path/to/cv]      (default: target/debug/cv)
#
# Same shape as ci.yml's `.kind ===` grep: a regex over the prose, a list from the binary, exit 1
# with every offender named. Names the manual documents AS removed (historical rename tables) and
# clap's built-in `help` are allowlisted below.
set -euo pipefail
cd "$(dirname "$0")/.."
CV="${1:-target/debug/cv}"

REMOVED_COMMANDS="convert query recall board help"

fail=0

known_cmds=$("$CV" schema --commands --json | python3 -I -c '
import json, sys
print(" ".join(c["name"] for c in json.load(sys.stdin) if " " not in c["name"]))')
mentioned_cmds=$(grep -ohE '`cv [a-z][a-z-]*' manual/src/*.md | sed 's/`cv //' | sort -u)
for name in $mentioned_cmds; do
  case " $known_cmds $REMOVED_COMMANDS " in
    *" $name "*) ;;
    *) echo "::error::manual names \`cv $name\`, which is not in \`cv schema --commands\`:"
       grep -nE "\`cv $name( |\`)" manual/src/*.md | head -5
       fail=1 ;;
  esac
done

if [ "$fail" -eq 0 ]; then
  echo "manual names: $(echo "$mentioned_cmds" | wc -w | tr -d ' ') commands all exist"
fi
exit "$fail"
