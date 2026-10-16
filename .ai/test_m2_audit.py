"""Owned synthetic regressions for the production audit classification seam."""
import copy
import unittest

from m2_audit import accepted_pattern_syntax, accepted_visibility


class VisibilityEvidence(unittest.TestCase):
    def setUp(self):
        self.paths = {"extra.cache"}
        self.row = dict(
            id="owned-case",
            status="differ",
            categories=["stdout"],
            oracle_exit=0,
            target_exit=0,
            oracle_stderr="",
            target_stderr="",
            oracle_only_records=[],
            target_only_records=["extra.cache:red"],
        )

    def test_names_lines_and_counts_can_be_visibility_extras(self):
        for record in ["extra.cache", "extra.cache:7:red", "extra.cache:3"]:
            with self.subTest(record=record):
                self.row["target_only_records"] = [record]
                self.assertTrue(accepted_visibility(self.row, self.paths))

    def test_missing_match_cannot_inherit_acceptance_from_the_same_id(self):
        self.assertTrue(accepted_visibility(self.row, self.paths))
        changed = copy.deepcopy(self.row)
        changed["oracle_only_records"] = ["owned.txt:red"]
        self.assertFalse(accepted_visibility(changed, self.paths))

    def test_an_extra_unapproved_path_is_not_visibility(self):
        self.row["target_only_records"].append("owned.txt:red")
        self.assertFalse(accepted_visibility(self.row, self.paths))

    def test_all_evidence_is_required_and_must_agree(self):
        changes = [
            ("categories", ["stdout", "stderr"]), ("categories", ["exit"]),
            ("oracle_exit", 1), ("target_exit", 2),
            ("oracle_stderr", "read fault"), ("target_stderr", "read fault"),
            ("target_only_records", []), ("target_only_records", "extra.cache"),
            ("target_only_records", [None]), ("status", "failure"),
        ]
        for key, value in changes:
            with self.subTest(key=key, value=value):
                changed = copy.deepcopy(self.row)
                changed[key] = value
                self.assertFalse(accepted_visibility(changed, self.paths))
        for key in self.row:
            if key == "id":
                continue
            with self.subTest(missing=key):
                changed = copy.deepcopy(self.row)
                del changed[key]
                self.assertFalse(accepted_visibility(changed, self.paths))


class PatternSyntaxEvidence(unittest.TestCase):
    def setUp(self):
        self.row = dict(
            id="owned-comment",
            status="differ",
            categories=["exit", "stderr", "stdout"],
            argv=["(?x)red # owned comment", "-"],
            oracle_exit=2,
            target_exit=0,
            oracle_only_records=[],
            target_only_records=["red"],
            target_stderr="",
            oracle_stderr=(
                "rg: regex parse error:\n    (?:(?x)red # owned comment)\n"
                "    ^\nerror: unclosed group\n"
            ),
        )

    def test_comment_exception_is_separate_from_visibility(self):
        self.assertTrue(accepted_pattern_syntax(self.row))
        self.assertFalse(accepted_visibility(self.row, {"extra.cache"}))
        self.row.update(
            target_exit=1, target_only_records=[], categories=["exit", "stderr"]
        )
        self.assertTrue(accepted_pattern_syntax(self.row))

    def test_unrelated_errors_and_missing_evidence_are_refused(self):
        changes = [
            ("oracle_only_records", ["red"]), ("argv", ["red"]),
            ("oracle_stderr", "rg: unreadable input"), ("target_exit", 2),
            ("target_stderr", "read fault"), ("categories", ["stdout"]),
            ("target_exit", 1), ("target_only_records", None),
        ]
        for key, value in changes:
            with self.subTest(key=key):
                changed = copy.deepcopy(self.row)
                changed[key] = value
                self.assertFalse(accepted_pattern_syntax(changed))
        for key in self.row:
            if key == "id":
                continue
            with self.subTest(missing=key):
                changed = copy.deepcopy(self.row)
                del changed[key]
                self.assertFalse(accepted_pattern_syntax(changed))


if __name__ == "__main__":
    unittest.main()
