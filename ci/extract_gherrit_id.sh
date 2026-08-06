#!/usr/bin/env bash
set -euo pipefail
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
git interpret-trailers --parse | python3 "$script_dir/gherrit_protocol.py" trailer-id
