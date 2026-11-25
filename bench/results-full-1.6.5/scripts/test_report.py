"""Checks for the release report's statistical and evidence boundaries."""
import json
import math
import os
import tempfile
import unittest
from pathlib import Path

import analyze
import collect
import guard


def comparison():
    return {
        'old': {'cost_trials': 33, 'graded': 33},
        'new': {'cost_trials': 33, 'graded': 33},
        'diff': [0, -.1, .1],
        'mcnemar': {'p': 1, 'old_only': 0, 'new_only': 0},
        'wall_ratio': [1, .8, 1.2],
        'wall_p': .5,
        'cost_ratio': [1.3, 1.1, 1.5],
        'cost_p': .01,
    }


class ReportTests(unittest.TestCase):
    def test_mcnemar_exact_significance_boundary(self):
        self.assertEqual(analyze.mcnemar_exact(5, 0), .0625)
        self.assertEqual(analyze.mcnemar_exact(6, 0), .03125)
        self.assertEqual(analyze.mcnemar_exact(0, 0), 1)

    def test_pairs_use_matching_task_and_trial_and_exclude_infrastructure(self):
        def row(task, trial, success, infra=False):
            return {'task': task, 'trial': trial, 'arm': 'archon', 'success': success, 'infra': infra}
        old = [row('a', 1, True), row('a', 2, False), row('b', 1, True), row('c', 1, True)]
        new = [row('a', 1, False), row('a', 2, True), row('b', 1, False, True), row('d', 1, True)]
        pairs = analyze.paired(old, new, 'archon')
        self.assertEqual(pairs['pairs'], 2)
        self.assertEqual((pairs['old_only'], pairs['new_only']), (1, 1))
        self.assertEqual(pairs['p'], 1)

    def test_equal_task_rates_still_show_opposite_flips(self):
        def row(trial, passed, wall):
            return {'task': 't', 'trial': trial, 'arm': 'archon', 'success': passed,
                    'infra': False, 'wall_secs': wall, 'items': [], 'error': None, 'note': None}
        old = [row(1, True, 100), row(2, False, 100)]
        new = [row(1, False, 100), row(2, True, 250)]
        task = {'task': 't', 'old': {'pass': 1, 'graded': 2, 'wall_mean': 100},
                'new': {'pass': 1, 'graded': 2, 'wall_mean': 175}, 'wall_ratio': 1.75, 'wall_p': .5}
        flips, slow = analyze.findings(old, new, [task], 'archon')
        self.assertEqual((flips[0]['pairs']['old_only'], flips[0]['pairs']['new_only']), (1, 1))
        self.assertEqual(slow[0]['slow_pairs'][0]['trial'], 2)

    def test_cost_regression_and_unpriced_data(self):
        data = comparison()
        self.assertEqual(analyze.verdict({'luna': {'archon': data}})[0], 'REGRESSION')
        data['cost_p'] = .2
        self.assertEqual(analyze.verdict({'luna': {'archon': data}})[0], 'NO REGRESSION')
        data['new']['cost_trials'] = 32
        with self.assertRaisesRegex(AssertionError, 'unpriced'):
            analyze.verdict({'luna': {'archon': data}})

    def test_pass_loss_and_slowdown_trigger_guard(self):
        data = comparison()
        data['cost_p'] = .5
        data['diff'] = [-.2, -.3, -.1]
        self.assertEqual(analyze.verdict({'luna': {'archon': data}})[0], 'REGRESSION')
        data['diff'] = [0, -.1, .1]
        data['wall_ratio'] = [1.3, 1.1, 1.5]
        data['wall_p'] = .01
        self.assertEqual(analyze.verdict({'luna': {'archon': data}})[0], 'REGRESSION')

    def test_missing_measurements_are_json_null(self):
        safe = analyze.json_safe({'values': [math.nan, math.inf, 1.0]})
        self.assertEqual(safe, {'values': [None, None, 1.0]})
        self.assertEqual(json.loads(json.dumps(safe, allow_nan=False)), safe)

    def test_copied_evidence_redacts_email(self):
        with tempfile.TemporaryDirectory(dir=Path(__file__).parent) as directory:
            source = Path(directory) / 'source'
            target = Path(directory) / 'target'
            email = b'person@example.invalid'
            source.write_bytes(b'answer ' + email)
            collect.copy_sanitized(source, target, email)
            self.assertEqual(source.read_bytes(), b'answer ' + email)
            self.assertEqual(target.read_bytes(), b'answer [redacted-email]')

    def test_guard_rejects_unrelated_process(self):
        self.assertFalse(guard.owned(os.getpid(), 'archon-bench-old'))
        self.assertFalse(guard.owned(os.getpid(), 'archon-bench-new'))


if __name__ == '__main__':
    unittest.main()
