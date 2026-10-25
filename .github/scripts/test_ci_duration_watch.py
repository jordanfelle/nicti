"""Tests for ci_duration_watch.py's streak rule (#522). Run: python3 -m unittest discover -s .github/scripts"""

import unittest
from unittest import mock

import ci_duration_watch as w


class Decide(unittest.TestCase):
    def test_within_budget_is_ok(self):
        self.assertEqual(w.decide(10, 12, 20, False), "ok")

    def test_two_slow_warm_runs_alert(self):
        self.assertEqual(w.decide(15, 12, 13, False), "alert")

    def test_one_slow_run_is_not_a_streak(self):
        self.assertEqual(w.decide(15, 12, 4, False), "no-streak")

    def test_a_lockfile_run_is_excused_however_slow(self):
        self.assertEqual(w.decide(15.2, 12, 12.3, True), "excused-current")

    def test_no_previous_run(self):
        self.assertEqual(w.decide(15, 12, None, False), "no-prev")


class PickPrevious(unittest.TestCase):
    @staticmethod
    def runs(*kinds):
        # newest first; "L" = changed Cargo.lock, "N" = did not
        return [{"id": 100 - i, "head_sha": f"{k}{i}"} for i, k in enumerate(kinds)]

    @staticmethod
    def is_lock(sha):
        return sha.startswith("L")

    def test_skips_lockfile_runs_to_the_nearest_clean_one(self):
        picked = w.pick_previous(self.runs("L", "L", "N", "N"), self.is_lock)
        self.assertEqual(picked["head_sha"], "N2")

    def test_alternating_lock_and_clean_runs_still_confirm_a_streak(self):
        # L N L N ...: the current (clean, slow) run is confirmed by the nearest clean run.
        picked = w.pick_previous(self.runs("L", "N", "L", "N"), self.is_lock)
        self.assertEqual(picked["head_sha"], "N1")

    def test_only_lockfile_runs_gives_nothing_to_confirm_with(self):
        self.assertIsNone(w.pick_previous(self.runs("L", "L", "L"), self.is_lock))

    def test_lookback_is_bounded(self):
        runs = self.runs(*(["L"] * w.MAX_LOOKBACK + ["N"]))
        self.assertIsNone(w.pick_previous(runs, self.is_lock))


class LockChanged(unittest.TestCase):
    def test_detects_cargo_lock_among_files(self):
        with mock.patch.object(w, "gh_api", return_value="src/lib.rs\nCargo.lock\n"):
            self.assertTrue(w.lock_changed("o/r", "abc"))

    def test_other_files_only(self):
        with mock.patch.object(w, "gh_api", return_value="src/lib.rs\ncrates/x/Cargo.toml\n"):
            self.assertFalse(w.lock_changed("o/r", "abc"))

    def test_a_nested_cargo_lock_is_not_the_workspace_lock(self):
        with mock.patch.object(w, "gh_api", return_value="spikes/x/Cargo.lock\n"):
            self.assertFalse(w.lock_changed("o/r", "abc"))

    def test_a_truncated_file_list_is_not_trusted_as_changed(self):
        listing = "\n".join(["Cargo.lock"] + [f"f{i}.rs" for i in range(w.COMMIT_FILES_CAP)])
        with mock.patch.object(w, "gh_api", return_value=listing):
            self.assertFalse(w.lock_changed("o/r", "abc"))

    def test_an_api_failure_is_not_an_excuse_and_does_not_abort(self):
        err = w.subprocess.CalledProcessError(1, ["gh"])
        with mock.patch.object(w, "gh_api", side_effect=err):
            self.assertFalse(w.lock_changed("o/r", "abc"))


if __name__ == "__main__":
    unittest.main()
