"""Offline checks for the review-seeded-bug rubric wording."""

import unittest
from pathlib import Path

import rubric

RUBRIC = Path(__file__).resolve().parents[1] / "tasks" / "review-seeded-bug" / "rubric.json"

# Wording from the Luna smoke run, which the narrow rubric scored as a miss.
ARCHON_LUNA = (
    "High - `verify`, line 27: accepts truncated signatures. `startsWith` accepts "
    "any prefix of the expected MAC, including an empty string. For a valid token, "
    "replacing everything after the dot with an empty signature still passes this "
    "check, so the unsigned payload is trusted if it has a future numeric `exp`."
)
AETHER_LUNA = (
    "Critical - `verify`: Signature validation accepts any prefix of the correct "
    "HMAC, including an empty signature, because it uses `expected.startsWith(signature)`. "
    "For example, `verify(validToken.split('.')[0] + '.', secret)` accepts the token "
    "body without any signature, as long as its `exp` is still valid."
)


def matched(text):
    groups, window, _ = rubric.load_rubric(RUBRIC)
    return [rubric.group_matches(g["all"], text, window) for g in groups]


class SeededBugRubricTests(unittest.TestCase):
    def test_luna_phrasings_pass(self):
        for text in (ARCHON_LUNA, AETHER_LUNA):
            self.assertEqual(matched(text), [True, True], text)

    def test_equivalent_wording_passes(self):
        for text in (
            "`verify` uses startsWith, so tokens can be forged.",
            "`verify` startsWith lets an attacker spoof any user.",
            "`verify` accepts a truncated MAC: signature bypass.",
            "`verify` compares with startsWith; an attacker can mint tokens with arbitrary claims.",
            "`verify` startsWith check means a token with no signature is accepted.",
        ):
            self.assertEqual(matched(text), [True, True], text)

    def test_unrelated_findings_fail(self):
        for text in (
            "`verify` throws on non-JSON bodies; wrap JSON.parse and return null.",
            "`verify` ignores extra dot segments, so a signed token with a suffix is accepted.",
            "`verify` is not constant time; use timingSafeEqual.",
            "`sign` has a doc typo. Also, startsWith is used in the README build script.",
        ):
            self.assertNotEqual(matched(text), [True, True], text)

    def test_forgery_wording_far_from_the_prefix_bug_fails(self):
        filler = " lorem ipsum" * 80
        text = "`verify` uses startsWith." + filler + " Unrelated: a cookie could be forged."
        self.assertEqual(matched(text), [True, False])


if __name__ == "__main__":
    unittest.main()
