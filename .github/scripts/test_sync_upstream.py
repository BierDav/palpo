"""Exercise the actual sync script against disposable local Git repositories."""

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("sync_upstream.sh").resolve()


class SyncUpstreamTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.upstream = self.root / "upstream"
        self.origin = self.root / "origin.git"
        self.fork = self.root / "fork"
        self.git(self.root, "init", "-b", "main", str(self.upstream))
        self.identity(self.upstream)
        self.write(self.upstream, "source.txt", "base\n")
        self.commit(self.upstream, "upstream base")
        self.base = self.git(self.upstream, "rev-parse", "HEAD").strip()
        self.git(self.upstream, "tag", "v0.6.0")
        self.git(self.root, "clone", "--bare", str(self.upstream), str(self.origin))
        self.git(self.root, "clone", str(self.origin), str(self.fork))
        self.identity(self.fork)
        self.git(self.fork, "checkout", "-b", "patched")
        self.write(self.fork, ".custom-upstream-revision", self.base + "\n")
        self.write(self.fork, ".custom-upstream-tag", "v0.6.0\n")
        self.commit(self.fork, "record custom upstream")
        # Mixed source/CI commits broke the former path-filtered cherry-picks.
        self.write(self.fork, "custom.txt", "retained custom code\n")
        self.write(self.fork, ".github/workflows/custom-release.yml", "custom CI\n")
        self.commit(self.fork, "mixed custom source and CI")
        self.custom = self.git(self.fork, "rev-parse", "HEAD").strip()
        self.git(self.fork, "push", "origin", "patched")
        self.git(self.fork, "config", f"url.{self.upstream}.insteadOf",
                 "https://github.com/palpo-im/palpo.git")
        self.bin = self.root / "bin"
        self.bin.mkdir()
        gh = self.bin / "gh"
        gh.write_text('#!/bin/sh\nif [ "$1" = api ]; then echo v0.6.0; '
                      'else printf "%s\\n" "$*" >> "$DISPATCH_LOG"; fi\n')
        gh.chmod(0o755)
        self.env = dict(os.environ, PATH=f"{self.bin}:{os.environ['PATH']}",
                        GITHUB_REPOSITORY="test/palpo",
                        GITHUB_STEP_SUMMARY=str(self.root / "summary"),
                        DISPATCH_LOG=str(self.root / "dispatch"))

    def git(self, cwd, *args):
        return subprocess.check_output(["git", *args], cwd=cwd, text=True,
                                       stderr=subprocess.STDOUT)

    def identity(self, repo):
        self.git(repo, "config", "user.name", "Sync test")
        self.git(repo, "config", "user.email", "sync-test@example.invalid")
        self.git(repo, "config", "commit.gpgsign", "false")

    def write(self, repo, name, text):
        path = repo / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    def commit(self, repo, message):
        self.git(repo, "add", ".")
        self.git(repo, "commit", "-m", message)

    def advance(self):
        self.write(self.upstream, "upstream.txt", "new upstream code\n")
        self.commit(self.upstream, "new upstream")
        return self.git(self.upstream, "rev-parse", "HEAD").strip()

    def run_sync(self, success=True):
        result = subprocess.run(["bash", str(SCRIPT)], cwd=self.fork, env=self.env,
                                text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT)
        self.assertEqual(result.returncode == 0, success, result.stdout)
        return result

    def remote_head(self):
        return self.git(self.root, "--git-dir", str(self.origin),
                        "rev-parse", "patched").strip()

    def test_merge_preserves_mixed_custom_commit_and_upstream(self):
        latest = self.advance()
        self.run_sync()
        tip = self.remote_head()
        for ancestor in (self.custom, latest):
            self.git(self.fork, "merge-base", "--is-ancestor", ancestor, tip)
        self.assertEqual((self.fork / "custom.txt").read_text(), "retained custom code\n")
        self.assertEqual((self.fork / ".github/workflows/custom-release.yml").read_text(),
                         "custom CI\n")
        self.assertEqual((self.fork / ".custom-upstream-revision").read_text(), latest + "\n")
        self.assertIn("release_tag=0.6.0-custom.1", (self.root / "dispatch").read_text())
        self.run_sync()
        self.assertEqual(tip, self.remote_head())
        self.assertEqual(len((self.root / "dispatch").read_text().splitlines()), 1)

    def test_current_upstream_is_a_noop(self):
        self.run_sync()
        self.assertEqual(self.remote_head(), self.custom)
        self.assertFalse((self.root / "dispatch").exists())

    def test_conflict_aborts_without_pushing_or_dispatching(self):
        self.write(self.fork, "source.txt", "custom conflicting edit\n")
        self.commit(self.fork, "custom edit")
        self.git(self.fork, "push", "origin", "patched")
        before = self.remote_head()
        self.write(self.upstream, "source.txt", "upstream conflicting edit\n")
        self.commit(self.upstream, "upstream edit")
        self.run_sync(success=False)
        self.assertEqual(self.remote_head(), before)
        self.assertEqual(self.git(self.fork, "status", "--porcelain"), "")
        self.assertFalse((self.fork / ".git/MERGE_HEAD").exists())
        self.assertFalse((self.root / "dispatch").exists())
        self.assertIn("source.txt", (self.root / "summary").read_text())

    def test_concurrent_update_is_never_overwritten(self):
        self.write(self.fork, "concurrent.txt", "another writer\n")
        self.commit(self.fork, "concurrent fork update")
        concurrent = self.git(self.fork, "rev-parse", "HEAD").strip()
        # Transfer the future writer's object without advancing patched yet.
        self.git(self.fork, "push", "origin", "HEAD:refs/heads/concurrent-fixture")
        self.git(self.fork, "checkout", "--detach", self.custom)
        hook = self.fork / ".git/hooks/pre-push"
        hook.write_text('#!/bin/sh\nwhile read local_ref local_sha remote_ref remote_sha; do\n'
                        'if [ "$remote_ref" = refs/heads/patched ]; then\n'
                        f'git --git-dir="{self.origin}" update-ref refs/heads/patched {concurrent}\n'
                        'fi\ndone\n')
        hook.chmod(0o755)
        self.advance()
        self.run_sync(success=False)
        self.assertEqual(self.remote_head(), concurrent)
        self.assertFalse((self.root / "dispatch").exists())


if __name__ == "__main__":
    unittest.main()
