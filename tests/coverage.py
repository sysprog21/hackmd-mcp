#!/usr/bin/env python3
"""Regression checks for the CI coverage merge, exclusions and ratchet."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("coverage_gate", ROOT / ".ci/coverage.py")
GATE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GATE)


class CoverageTests(unittest.TestCase):
    def test_attributes_and_test_items_do_not_hide_production_branches(self):
        source = '''#[tool(
    description = "a [ bracket and a } brace",
)]
fn production() {
    if failed { recover(); }
}
#[cfg(test)]
fn helper(
    input: bool,
) {
    if input { assert!(input); }
}
fn after_helper() { evict(); }
#[cfg(test)]
mod tests {
    #[test]
    fn test_it() { assert!(true); }
}
'''
        excluded = GATE.excluded_lines(source)
        self.assertEqual(excluded, set(range(1, 4)) | set(range(7, 13)) | set(range(14, 19)))
        self.assertNotIn(5, excluded)
        self.assertNotIn(13, excluded)

    def test_inline_attributes_fail_instead_of_hiding_following_source(self):
        for source in ["#[cfg(test)] use fake;\nfn live() { recover(); }\n",
                       "#[inline] fn live() { recover(); }\n"]:
            with self.assertRaisesRegex(ValueError, "shares a line"):
                GATE.excluded_lines(source)

    def test_multiline_test_fields_and_function_types_are_excluded(self):
        source = "#[cfg(test)]\nfield: HashMap<A,\n    B>,\nfn live() { recover(); }\n"
        self.assertEqual(GATE.excluded_lines(source), {1, 2, 3})
        source = "#[cfg(test)]\nfn helper() -> Result<A,\n B> { fake(); }\nfn live() { recover(); }\n"
        self.assertEqual(GATE.excluded_lines(source), {1, 2, 3})

    def test_a_test_match_arm_does_not_hide_the_arms_after_it(self):
        for arm in ["0 => 9,", "y if y >= 1 => 9,", "y if y < 1 => 9,", "y if y <= 1 => 9,"]:
            source = f"match x {{\n    #[cfg(test)]\n    {arm}\n    1 => live(),\n    _ => {{ recover() }}\n}}\n"
            self.assertEqual(GATE.excluded_lines(source), {2, 3}, arm)
        with self.assertRaisesRegex(ValueError, "cannot find the end"):
            GATE.excluded_lines("#[cfg(test)]\nfield: A>,\nfn live() { recover(); }\n")

    def test_merge_preserves_uncovered_platform_and_error_lines(self):
        with tempfile.TemporaryDirectory() as temp:
            paths = []
            for index, (filename, rows) in enumerate([
                ("src/a.rs", "DA:1,1\nDA:2,0\nDA:3,0"),
                (r"src\a.rs", "DA:2,4\nDA:4,0"),
                ("src/a.rs", "DA:1,2"),
            ]):
                path = Path(temp) / str(index)
                path.write_text(f"SF:{filename}\n{rows}\nend_of_record\n")
                paths.append(path)
            coverage = GATE.merge_reports(paths, {"src/a.rs": "ok();\nwindows();\nerror();\nrecover();\n"})
            self.assertEqual(coverage, {("src/a.rs", 1): True, ("src/a.rs", 2): True,
                                       ("src/a.rs", 3): False, ("src/a.rs", 4): False})
            paths[0].write_text("")
            with self.assertRaisesRegex(ValueError, "empty"):
                GATE.merge_reports(paths, {"src/a.rs": "ok();"})

    def test_dependency_suffix_never_counts_as_workspace_source(self):
        with tempfile.TemporaryDirectory() as temp:
            report = Path(temp) / "lcov.info"
            report.write_text("SF:src/lib.rs\nDA:1,0\nend_of_record\n"
                              "SF:/registry/dependency/src/lib.rs\nDA:1,1\nDA:1000,1\nend_of_record\n")
            self.assertEqual(GATE.merge_reports([report], {"src/lib.rs": "fail();\n"}),
                             {("src/lib.rs", 1): False})

    def test_diff_insertions_deletions_and_multiple_files(self):
        diff = '''+++ b/src/a.rs
@@ -1 +1,2 @@
@@ -7,2 +8,0 @@
+++ /dev/null
@@ -1,2 +0,0 @@
+++ b/src/b.rs
@@ -0,0 +1 @@
'''
        self.assertEqual(GATE.changed_lines(diff), {("src/a.rs", 1), ("src/a.rs", 2), ("src/b.rs", 1)})

    def test_diff_names_with_spaces_are_read_and_quoted_names_refused(self):
        self.assertEqual(GATE.changed_lines("+++ b/src/x y.rs\t\n@@ -0,0 +1 @@\n"), {("src/x y.rs", 1)})
        with self.assertRaisesRegex(ValueError, "cannot read the changed file"):
            GATE.changed_lines('+++ "b/src/q\\"x.rs"\n@@ -0,0 +1 @@\n')

    def test_floor_cannot_drop_or_round_an_uncovered_change_up(self):
        GATE.check_floor(80, 80, 4, 5)
        GATE.check_floor(90, 80, 0, 0)
        with self.assertRaisesRegex(ValueError, "cannot decrease"):
            GATE.check_floor(79, 80, 1, 1)
        with self.assertRaisesRegex(ValueError, "below"):
            GATE.check_floor(80, 80, 3, 4)
        with self.assertRaises(ValueError):
            GATE.read_floor("101")

    def repository(self, root):
        """A git repository under `root` holding `src/a.rs` and a floor of 80, and a
        runner for the gate against its first commit, both blind to the caller's
        git config so signing or hooks there cannot break or prompt."""
        env = {key: value for key, value in os.environ.items() if key != "GITHUB_STEP_SUMMARY"}
        env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1")
        (root / "src").mkdir()
        (root / ".ci").mkdir()
        (root / "src/a.rs").write_text("fn f() { recover(); }\n")
        (root / GATE.FLOOR).write_text("80\n")
        def git(*args):
            return subprocess.check_output(["git", *args], cwd=root, text=True, env=env).strip()
        def commit(message):
            git("add", "-A")
            git("-c", "user.name=Test", "-c", "user.email=test@example.test", "commit", "-qm", message)
        git("init", "-q")
        commit("base")
        base = git("rev-parse", "HEAD")
        def run():
            return subprocess.run(["python3", str(ROOT / ".ci/coverage.py"), "reports", "--base", base],
                                  cwd=root, capture_output=True, text=True, env=env)
        return git, commit, run

    def reports(self, root, path, hits):
        for platform in GATE.PLATFORMS:
            directory = root / "reports" / f"coverage-{platform}"
            directory.mkdir(parents=True)
            (directory / "lcov.info").write_text(f"SF:{path}\nDA:1,{hits}\nend_of_record\n")

    def test_cli_counts_only_the_lines_a_move_edits(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            git, commit, run = self.repository(root)
            git("mv", "src/a.rs", "src/b.rs")
            commit("move")
            self.reports(root, "src/b.rs", 0)
            result = run()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn("Changed source lines: 0/0", result.stdout)

    def test_cli_requires_all_platforms_and_blocks_floor_removal(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            git, commit, run = self.repository(root)
            (root / GATE.FLOOR).unlink()
            commit("remove floor")
            self.reports(root, "src/a.rs", 1)
            result = run()
            self.assertEqual(result.returncode, 1)
            self.assertIn("cannot be removed", result.stderr)
            extra = root / "reports/coverage-extra"
            extra.mkdir()
            (extra / "lcov.info").write_text("SF:src/a.rs\nDA:1,1\nend_of_record\n")
            result = run()
            self.assertEqual(result.returncode, 1)
            self.assertIn("requires Linux, macOS and Windows", result.stderr)
            (extra / "lcov.info").unlink()
            (root / "reports/coverage-Windows/lcov.info").unlink()
            result = run()
            self.assertEqual(result.returncode, 1)
            self.assertIn("requires Linux, macOS and Windows", result.stderr)


if __name__ == "__main__":
    unittest.main()
