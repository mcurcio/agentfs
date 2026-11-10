"""Tests of the model's safety net itself: the corpus runner, the digest's compare, the
mount refusal and the wrapper's stall alarm. Plain Python and bash, no Docker, so CI runs
them on every push:

    python3 -m unittest discover -s crates/agentfs-model/planted -p 'test_*.py'
"""

import os
import pathlib
import shutil
import stat
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[2]
sys.path.insert(0, str(HERE))
import mounts  # noqa: E402
import run  # noqa: E402

BUG = {"id": "b", "rule": "R1", "seeds": 64, "find": "let x = 1;", "replace": "let x = 2;"}


def libtest(test, where, message):
    """A libtest failure block of `test`, panicking at `where` with `message`."""
    return textwrap.dedent("""\
        running 1 test
        test {test} ... FAILED

        failures:

        ---- {test} stdout ----

        thread '{test}' (7) panicked at {where}:10:5:
        {message}

        failures:
            {test}

        test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
        """).format(test=test, where=where, message=message)


class CachedResults(unittest.TestCase):
    """A cached result is reused only for the same corpus entry, model and mode."""

    def test_a_changed_source_or_entry_runs_again(self):
        work = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, work)
        kept = run.result_path(work, BUG, "model-1", "fast")
        self.assertEqual(kept, run.result_path(work, dict(BUG), "model-1", "fast"))
        self.assertNotEqual(kept, run.result_path(work, BUG, "model-2", "fast"))
        self.assertNotEqual(kept, run.result_path(work, {**BUG, "replace": "let x = 3;"},
                                                  "model-1", "fast"))
        self.assertNotEqual(kept, run.result_path(work, BUG, "model-1", "full"))

    def test_the_model_hash_reads_sources_and_tests(self):
        model = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, model)
        for d in ("src", "tests"):
            (model / d).mkdir()
            (model / d / "a.rs").write_text("one")
        before = run.model_hash(model)
        (model / "tests" / "a.rs").write_text("two")
        self.assertNotEqual(before, run.model_hash(model))

    def test_a_cached_result_is_used_without_running(self):
        work = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, work)
        run.result_path(work, BUG, "m", "fast").write_text("cached\n")
        saved = run.copy_of
        run.copy_of = lambda *_: self.fail("a cached bug ran")
        try:
            self.assertEqual(run.run_bug(work, BUG, "m", "fast"), "cached\n")
        finally:
            run.copy_of = saved


class Stale(unittest.TestCase):
    """A stale anchor stops the run before any bug runs, a cached result or not."""

    def test_stale_fails_before_anything_runs(self):
        tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        (tmp / "src").mkdir()
        (tmp / "src" / "lib.rs").write_text("let y = 1;\n")
        (tmp / "corpus.toml").write_text(
            "[[bug]]\nid = 'b'\nrule = 'R1'\nseeds = 64\nfind = 'let x = 1;'\nreplace = 'x'\n")
        saved = run.run_bug
        run.run_bug = lambda *_: self.fail("a bug ran despite a stale anchor")
        try:
            code = run.main(["--work", str(tmp / "w"), "--corpus", str(tmp / "corpus.toml"),
                             "--src", str(tmp / "src")])
        finally:
            run.run_bug = saved
        self.assertEqual(code, 1)


class NeverACatch(unittest.TestCase):
    """A panic, a timeout and a spawn failure are never a catch."""

    def test_a_panic_in_the_model_during_a_scenario(self):
        out = libtest("r1_x", "crates/agentfs-model/src/world.rs", "git failed")
        self.assertEqual(run.classify(101, out)[0], "SURVIVED")

    def test_a_harness_panic_in_a_random_run(self):
        out = libtest("random_interleavings_full_lineage", run.INTERLEAVINGS,
                      "1 of 64 seeds failed:\n\nseed 3, step 4: harness panic: locks: boom")
        self.assertEqual(run.classify(101, out)[0], "SURVIVED")

    def test_a_timeout(self):
        self.assertEqual(run.classify(None, "")[0], "TIMEOUT")

    def test_a_spawn_failure(self):
        out = libtest("r1_x", run.SCENARIOS, "locks: Resource temporarily unavailable")
        self.assertEqual(run.classify(101, out)[0], "HARNESS")

    def test_a_named_check_and_a_scenario_assertion_are_catches(self):
        named = libtest("random_interleavings_full_lineage", run.INTERLEAVINGS,
                        "1 of 64 seeds failed:\n\nseed 3, step 4: full_lineage: lost")
        self.assertEqual(run.classify(101, named), ("caught", [
            ("random_interleavings_full_lineage", "full_lineage")]))
        asserted = libtest("r1_x", run.SCENARIOS, "assertion failed: collisions empty")
        self.assertEqual(run.classify(101, asserted), ("caught", [("r1_x", "assert")]))


class Compare(unittest.TestCase):
    """A missing digest or a missing shard fails ci/model-digest.sh compare."""

    def setUp(self):
        self.root = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root)
        (self.root / "ci").mkdir()
        shutil.copy(ROOT / "ci/model-digest.sh", self.root / "ci")
        (self.root / "ci/git-release").write_text("2.47.3\n")
        base = self.root / "crates/agentfs-model/baseline"
        base.mkdir(parents=True)
        (base / "digests.txt").write_text("# git 2.47.3\nr_a aaa\nt.seed1 bbb\n")

    def filled(self, name, runs, markers):
        d = self.root / name
        d.mkdir()
        for r, sha in runs.items():
            (d / f"{r}.digest").write_text(sha + "\n")
        for shard, of in markers:
            (d / f"shard-{shard}-of-{of}.ok").write_text(f"{shard} {of} 500\n")
        return str(d)

    def compare(self, *dirs):
        return subprocess.run(["bash", str(self.root / "ci/model-digest.sh"), "compare", *dirs],
                              capture_output=True, text=True).returncode

    def test_all_present_passes(self):
        self.assertEqual(self.compare(self.filled("d", {"r_a": "aaa", "t.seed1": "bbb"},
                                                  [(0, 1)])), 0)

    def test_a_missing_digest_fails(self):
        self.assertEqual(self.compare(self.filled("d", {"r_a": "aaa"}, [(0, 1)])), 1)

    def test_a_missing_marker_fails(self):
        self.assertEqual(self.compare(self.filled("d", {"r_a": "aaa", "t.seed1": "bbb"}, [])), 1)

    def test_a_missing_shard_fails(self):
        d0 = self.filled("d0", {"r_a": "aaa", "t.seed1": "bbb"}, [(0, 2)])
        self.assertEqual(self.compare(d0), 1)


class MountRefusal(unittest.TestCase):
    """TMPDIR or the work folder on a host file share is refused."""

    INFO = textwrap.dedent("""\
        194 134 0:57 / / rw,relatime - overlay overlay rw
        203 194 0:44 /Dock/t /target rw,relatime - virtiofs virtiofs1 rw
        204 194 254:1 /docker/volumes/w/_data /work rw,relatime - ext4 /dev/vda1 rw
        """)

    def test_a_host_share_is_refused_and_a_volume_is_not(self):
        self.assertIsNotNone(mounts.refuse(["/target/planted"], self.INFO))
        self.assertIsNone(mounts.refuse(["/work/planted"], self.INFO))
        self.assertIsNone(mounts.refuse(["/tmp"], self.INFO))

    def test_run_py_exits_2_on_a_host_share(self):
        tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        (tmp / "src").mkdir()
        (tmp / "src" / "lib.rs").write_text("let x = 1;\n")
        (tmp / "corpus.toml").write_text(
            "[[bug]]\nid = 'b'\nrule = 'R1'\nseeds = 64\nfind = 'let x = 1;'\nreplace = 'x'\n")
        saved = run.mounts.refuse
        run.mounts.refuse = lambda paths: "on a host file share"
        try:
            code = run.main(["--work", str(tmp / "w"), "--corpus", str(tmp / "corpus.toml"),
                             "--src", str(tmp / "src")])
        finally:
            run.mounts.refuse = saved
        self.assertEqual(code, 2)


class StallAlarm(unittest.TestCase):
    """ci/model-docker.sh kills a job whose unit never finishes, naming it, and exits 3."""

    def test_a_stalled_unit_exits_3(self):
        tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        fake = tmp / "docker"
        fake.write_text(textwrap.dedent(f"""\
            #!/usr/bin/env bash
            case $1 in
                run) echo "unit-start: slow-bug"; sleep 60 & echo $! > {tmp}/pid; wait ;;
                kill) kill "$(cat {tmp}/pid)" ;;
            esac
            """))
        fake.chmod(fake.stat().st_mode | stat.S_IEXEC)
        start = time.monotonic()
        out = subprocess.run(["bash", str(ROOT / "ci/model-docker.sh"), "--stall", "2", "--",
                              "true"], env={**os.environ, "MODEL_DOCKER": str(fake)},
                             capture_output=True, text=True, timeout=50)
        self.assertEqual(out.returncode, 3, out.stderr)
        self.assertIn("in flight: slow-bug", out.stderr)
        self.assertLess(time.monotonic() - start, 30)

    def test_a_budget_write_beside_another_container_is_refused(self):
        tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, tmp)
        fake = tmp / "docker"
        fake.write_text(textwrap.dedent(f"""\
            #!/usr/bin/env bash
            case $1 in
                ps) echo "50d6ef357d43 /w/other.sh" ;;
                run) touch {tmp}/ran ;;
            esac
            """))
        fake.chmod(fake.stat().st_mode | stat.S_IEXEC)
        out = subprocess.run(["bash", str(ROOT / "ci/model-docker.sh"), "--", "python3", "run.py",
                              "--write"], env={**os.environ, "MODEL_DOCKER": str(fake)},
                             capture_output=True, text=True, timeout=50)
        self.assertEqual(out.returncode, 5, out.stderr)
        self.assertFalse((tmp / "ran").exists(), "the job ran beside another container")


if __name__ == "__main__":
    unittest.main()
