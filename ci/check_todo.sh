#!/usr/bin/env bash
#
# Copyright 2025 The Fuchsia Authors
#
# Licensed under a BSD-style license <LICENSE-BSD>, Apache License, Version 2.0
# <LICENSE-APACHE or https://www.apache.org/licenses/LICENSE-2.0>, or the MIT
# license <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your option.
# This file may not be copied, modified, or distributed except according to
# those terms.

set -euo pipefail

cd "$(dirname "$0")/.."

# Construct the keyword so this checker does not report its own source.
keyword=XODO
keyword=${keyword/X/T}
disable_marker="${keyword}-check-disable"
enable_marker="${keyword}-check-enable"

paths=("$@")
if [[ ${#paths[@]} -eq 0 ]]; then
  paths=(.)
fi

# CI checks committed files. Exclude vendored sources, which follow their own
# task-marker conventions.
matches=$(
  {
    git log -1 --format=%B 2>/dev/null |
      grep -n -w "$keyword" |
      sed 's/^/COMMIT_MESSAGE:/' || true
    git grep -n -I -w "$keyword" -- "${paths[@]}" \
      ':(exclude,glob)**/vendor/**' || true
  } | LC_ALL=C sort -t: -k1,1 -k2,2n
)

if [[ -z $matches ]]; then
  exit 0
fi

current_file=
disabled=0
exit_code=0

while IFS= read -r match; do
  if [[ $match =~ ^(.*):([0-9]+):(.*)$ ]]; then
    file=${BASH_REMATCH[1]}
    content=${BASH_REMATCH[3]}
  else
    echo "Could not parse task-marker match: $match" >&2
    exit 1
  fi

  if [[ $file != "$current_file" ]]; then
    current_file=$file
    disabled=0
  fi

  if [[ $content == *"$disable_marker"* ]]; then
    disabled=1
  elif [[ $content == *"$enable_marker"* ]]; then
    disabled=0
  elif [[ $disabled -eq 0 ]]; then
    if [[ $exit_code -eq 0 ]]; then
      echo "Found $keyword markers in the codebase." >&2
      echo "Use FIXME for non-blocking work, or wrap intentional markers" >&2
      echo "with $disable_marker and $enable_marker." >&2
      echo >&2
    fi
    echo "$match" >&2
    exit_code=1
  fi
done <<< "$matches"

exit "$exit_code"
