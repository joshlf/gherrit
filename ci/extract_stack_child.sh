#!/usr/bin/env bash

set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
metadata=$(bash "$script_dir/extract_stack_metadata.sh")
jq -er '.child // ""' <<<"$metadata"
