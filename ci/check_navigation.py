#!/usr/bin/env python3
"""Check local documentation destinations and explicit source file references."""
import re
from pathlib import Path
from urllib.parse import unquote, urlsplit

ROOT = Path(__file__).resolve().parent.parent
errors = []
for document in [ROOT / 'AGENTS.md', *sorted((ROOT / 'agent_docs').glob('*.md'))]:
    text = document.read_text()
    for target in re.findall(r'\]\(([^\s)]+)\)', text):
        url = urlsplit(target.strip('<>'))
        if not url.scheme and url.path:
            path = document.parent / unquote(url.path)
            if not path.exists():
                errors.append(f'{document.relative_to(ROOT)}: missing link {target}')
    for target in re.findall(r'`((?:src|tests|testutil/src|ci)/[^`\s{}*,:]+)`', text):
        if not (ROOT / target).exists():
            errors.append(f'{document.relative_to(ROOT)}: missing source {target}')
    for target, function in re.findall(
            r'`((?:src|tests|testutil/src)/[^`\s:]+\.rs)::([A-Za-z_][A-Za-z_0-9]*)`', text):
        path = ROOT / target
        if not path.is_file() or not re.search(r'\bfn\s+' + re.escape(function) + r'\b', path.read_text()):
            errors.append(f'{document.relative_to(ROOT)}: missing function {target}::{function}')
if errors:
    raise SystemExit('\n'.join(errors))
print('Documentation destinations and explicit source references exist')
