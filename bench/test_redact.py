"""Offline checks for email redaction of bench output."""

import tempfile
import unittest
from pathlib import Path

import redact

BENCH = Path(__file__).resolve().parent
PERSONAL = b'someone@' + b'example-mail.test'


class RedactTests(unittest.TestCase):
    def test_personal_address_is_replaced_anywhere(self):
        data = b'{"answer": "mail ' + PERSONAL + b'", "other": "<' + PERSONAL.upper() + b'>"}'
        self.assertEqual(redact.redact_bytes(data),
                         b'{"answer": "mail [redacted-email]", "other": "<[redacted-email]>"}')

    def test_noreply_and_fixture_addresses_are_kept(self):
        kept = b'noreply@anthropic.com 1+a@users.noreply.github.com x@example.com archon@openai.com'
        self.assertEqual(redact.redact_bytes(kept), kept)

    def test_leading_punctuation_and_trailing_digits_are_redacted(self):
        self.assertEqual(redact.redact_bytes(b'_' + PERSONAL), b'[redacted-email]')
        self.assertEqual(redact.redact_bytes(b'+' + PERSONAL + b'1'), b'[redacted-email]')

    def test_fixture_address_before_sentence_punctuation_is_kept(self):
        text = b'mail p@example.com. then q@example.org, and r@example.net;'
        self.assertEqual(redact.redact_bytes(text), text)

    def test_utf16_text_is_detected_in_binary_data(self):
        data = b'\x00\x01' + PERSONAL.decode().encode('utf-16le') + b'\x00'
        self.assertEqual(redact.personal_addresses(data), [])
        self.assertEqual(redact.personal_addresses_any_encoding(data), [PERSONAL])

    def test_text_without_address_is_unchanged(self):
        self.assertEqual(redact.redact_bytes(b'user@ and @handle'), b'user@ and @handle')

    def test_collect_redacts_answers_and_item_records(self):
        import sys
        sys.path.insert(0, str(BENCH / 'results-full-1.6.5/scripts'))
        import collect
        with tempfile.TemporaryDirectory() as directory:
            for name in ('answer.txt', 'archon-items.json'):
                source, target = Path(directory) / f'in-{name}', Path(directory) / f'out-{name}'
                source.write_bytes(b'reply to ' + PERSONAL)
                collect.copy_sanitized(source, target)
                self.assertEqual(target.read_bytes(), b'reply to [redacted-email]')


    def test_repeat_collection_accepts_redacted_start_record(self):
        import sys
        sys.path.insert(0, str(BENCH / 'results-full-1.6.5/scripts'))
        import collect
        with tempfile.TemporaryDirectory() as directory:
            source, destination = Path(directory) / 'src', Path(directory) / 'dst'
            (source / 'results/stamp').mkdir(parents=True)
            (source / 'start.json').write_bytes(b'{"who": "' + PERSONAL + b'"}')
            (source / 'results/stamp/run.json').write_text('{}')
            (source / 'results/stamp/results.jsonl').write_text('{}\n')
            collect.collect_run(source, destination)
            collect.collect_run(source, destination)
            self.assertNotIn(PERSONAL, (destination / 'start.json').read_bytes())


if __name__ == '__main__':
    unittest.main()
