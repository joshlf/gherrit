#!/usr/bin/env python3
"""Exercise navigation checks against temporary document trees."""
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

CHECKER = Path(__file__).with_name('check_navigation.py').read_bytes()


class NavigationTests(unittest.TestCase):
    def check_document(self, document, files=()):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'ci').mkdir()
            (root / 'agent_docs').mkdir()
            checker = root / 'ci/check_navigation.py'
            checker.write_bytes(CHECKER)
            (root / 'AGENTS.md').write_text(document)
            for name in files:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text('fixture')
            return subprocess.run(
                [sys.executable, str(checker)], capture_output=True, text=True)

    def test_missing_destinations_fail_with_or_without_titles(self):
        destinations = [
            'missing.md',
            'missing.md "details"',
            "missing.md 'details'",
            'missing.md (details)',
            'missing.md "details (more)"',
            r'missing.md "escaped \"quote\""',
            '<missing guide.md> "details"',
        ]
        for destination in destinations:
            with self.subTest(destination=destination):
                result = self.check_document(f'[guide]({destination})')
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('missing link', result.stderr)

    def test_existing_destinations_accept_titles(self):
        document = '''[plain](guide.md)
[double](guide.md "details")
[single](guide.md 'details')
[parentheses](guide.md (details))
[angle](<guide with spaces.md> "details")
[encoded](guide%20with%20spaces.md "details")
'''
        result = self.check_document(
            document, ['guide.md', 'guide with spaces.md'])
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_external_and_fragment_only_links_are_not_local_paths(self):
        result = self.check_document(
            '[external](https://example.com/guide "details")\n'
            '[anchor](#details "details")')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_source_references_remain_checked(self):
        result = self.check_document('`src/missing.rs`')
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('missing source src/missing.rs', result.stderr)


if __name__ == '__main__':
    unittest.main()
