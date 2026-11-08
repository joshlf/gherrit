#!/usr/bin/env bash

set -euo pipefail

# Match the marker written by src/pre_push/body.rs. The last matching line is
# authoritative; other PR prose can contain examples of the same marker. The
# JSON decoder below validates the child field before the caller uses it.
metadata=$(
  sed -n 's/^.*<!-- gherrit-meta: \(.*\) -->[[:space:]]*$/\1/p' |
    tail -n 1
)

if [[ -z $metadata ]]; then
  echo "Could not find terminal GHerrit metadata in the PR body." >&2
  exit 1
fi

# GHerrit versions before the metadata serializer fix appended one stray quote
# after the JSON object. Accept that single known legacy spelling so merging an
# older PR can still advance its stack.
if [[ ${metadata: -1} == '"' ]]; then
  metadata=${metadata%\"}
fi

# A null child marks the top of the stack. Missing or malformed metadata is an
# error; the action cannot confuse a failed read with a finished stack.
if ! child=$(
  jq -Rer '
    fromjson |
    if type == "object"
      and has("child")
      and (.child == null or (.child | type == "string"))
    then (.child // "")
    else error("metadata child must be a string or null")
    end
  ' <<<"$metadata"
); then
  echo "GHerrit metadata is not a valid stack object." >&2
  exit 1
fi

printf '%s\n' "$child"
