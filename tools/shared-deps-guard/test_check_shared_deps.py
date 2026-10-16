import datetime
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_shared_deps as g  # noqa: E402

# 以下两份由真实 Cargo.toml 裁剪而来:Digger 把依赖集中在 [workspace.dependencies],
# Dozer 则在每个 crate 里各写各的。
DIGGER_ROOT_TOML = """\
[workspace]
resolver = "3"
members = ["crates/*"]

[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
bytehost-core = { git = "https://github.com/byteboyai/bytehost", tag = "v0.4.1" }
bytehost-desktop = { git = "https://github.com/byteboyai/bytehost", tag = "v0.4.1" }
byteui = { git = "https://github.com/byteboyai/byteui", tag = "v0.4.4" }
dozer-model = { git = "https://github.com/byteboyai/dozer", tag = "libs-2026-10-16" }
dozer-services = { git = "https://github.com/byteboyai/dozer", tag = "libs-2026-10-16" }
"""

DOZER_APP_TOML = """\
[package]
name = "dozer-app"
version = "0.1.0"

[dependencies]
bytehost-apps = { git = "https://github.com/byteboyai/bytehost", tag = "v0.4.0" }
bytehost-desktop = { git = "https://github.com/byteboyai/bytehost", tag = "v0.4.0", features = ["overlay"] }
byteui = { git = "https://github.com/byteboyai/byteui", tag = "v0.4.4" }
bytegit = { git = "https://github.com/byteboyai/bytegit", tag = "v0.7.0", features = ["watch"] }
tokio = { version = "1" }

[dev-dependencies]
bytegit = { git = "https://github.com/byteboyai/bytegit", tag = "v0.7.0", features = ["testutil"] }

[target.'cfg(target_os = "macos")'.dependencies]
bytehost-webview = { git = "https://github.com/byteboyai/bytehost", tag = "v0.4.0" }
"""


def write(root, rel, text):
    path = Path(root) / rel
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


class RepoNameTests(unittest.TestCase):
    def test_accepts_plain_and_dot_git_and_lock_forms(self):
        for url in (
            "https://github.com/byteboyai/bytehost",
            "https://github.com/byteboyai/bytehost.git",
            "https://github.com/byteboyai/bytehost/",
            "git+https://github.com/byteboyai/bytehost?tag=v0.4.0#abc123",
        ):
            self.assertEqual(g.repo_name(url), "bytehost", url)

    def test_rejects_other_orgs_and_registries(self):
        self.assertIsNone(g.repo_name("https://github.com/someone/bytehost"))
        self.assertIsNone(g.repo_name("registry+https://github.com/rust-lang/crates.io-index"))


class CollectDepsTests(unittest.TestCase):
    def test_workspace_dependencies_shape(self):
        with tempfile.TemporaryDirectory() as tmp:
            write(tmp, "Cargo.toml", DIGGER_ROOT_TOML)
            deps = g.collect_deps(Path(tmp))
        by_crate = {d.crate: d for d in deps}
        self.assertEqual(set(by_crate), {"bytehost-core", "bytehost-desktop", "byteui",
                                         "dozer-model", "dozer-services"})
        self.assertEqual((by_crate["byteui"].repo, by_crate["byteui"].ref_kind, by_crate["byteui"].ref),
                         ("byteui", "tag", "v0.4.4"))
        self.assertEqual(by_crate["dozer-model"].ref, "libs-2026-10-16")

    def test_per_crate_shape_with_dev_and_target_tables(self):
        with tempfile.TemporaryDirectory() as tmp:
            write(tmp, "crates/dozer-app/Cargo.toml", DOZER_APP_TOML)
            deps = g.collect_deps(Path(tmp))
        self.assertEqual(sorted({d.crate for d in deps}),
                         ["bytegit", "bytehost-apps", "bytehost-desktop", "bytehost-webview", "byteui"])
        self.assertTrue(all(d.manifest == "crates/dozer-app/Cargo.toml" for d in deps))
        self.assertEqual(len([d for d in deps if d.crate == "bytegit"]), 2)  # deps + dev-deps

    def test_package_rename_and_missing_ref_and_foreign_git_and_skipped_dirs(self):
        with tempfile.TemporaryDirectory() as tmp:
            write(tmp, "Cargo.toml", """\
[dependencies]
core = { git = "https://github.com/byteboyai/bytehost", package = "bytehost-core" }
other = { git = "https://github.com/someone/else", tag = "v1.0.0" }
pinned = { git = "https://github.com/byteboyai/bytegit", rev = "abc1234def" }
""")
            write(tmp, "target/debug/Cargo.toml", """\
[dependencies]
ignored = { git = "https://github.com/byteboyai/bytehost", tag = "v9.9.9" }
""")
            deps = g.collect_deps(Path(tmp))
        self.assertEqual([(d.crate, d.ref_kind, d.ref) for d in deps],
                         [("bytehost-core", "none", ""), ("pinned", "rev", "abc1234def")])


class WorkspaceGraphTests(unittest.TestCase):
    def test_excluded_workspace_is_its_own_graph(self):
        with tempfile.TemporaryDirectory() as tmp:
            write(tmp, "Cargo.toml", '[workspace]\nmembers = ["crates/*"]\nexclude = ["spike/x"]\n')
            write(tmp, "crates/a/Cargo.toml", """\
[dependencies]
bytehost-core = { git = "https://github.com/byteboyai/bytehost", tag = "v0.4.1" }
""")
            write(tmp, "spike/x/Cargo.toml", """\
[workspace]
[dependencies]
bytehost-apps = { git = "https://github.com/byteboyai/bytehost", tag = "v0.2.0" }
""")
            write(tmp, "spike/y/Cargo.toml", """\
[dependencies]
byteui = { git = "https://github.com/byteboyai/byteui", tag = "v0.4.0" }
""")
            deps = {d.crate: d.graph for d in g.collect_deps(Path(tmp))}
        # spike/y 没有自己的 [workspace]:按 cargo 的规则归属向上最近的工作区(根)
        self.assertEqual(deps, {"bytehost-core": ".", "bytehost-apps": "spike/x", "byteui": "."})

    def test_package_without_any_workspace_ancestor_is_standalone(self):
        with tempfile.TemporaryDirectory() as tmp:
            write(tmp, "tools/a/Cargo.toml", """\
[dependencies]
byteui = { git = "https://github.com/byteboyai/byteui", tag = "v0.4.0" }
""")
            deps = {d.crate: d.graph for d in g.collect_deps(Path(tmp))}
        self.assertEqual(deps, {"byteui": "tools/a"})


class PatchLockPinLoadTests(unittest.TestCase):
    def test_local_patches_only_for_byteboyai_paths(self):
        with tempfile.TemporaryDirectory() as tmp:
            write(tmp, "Cargo.toml", """\
[patch."https://github.com/byteboyai/bytehost"]
bytehost-core = { path = "../bytehost-chrome/crates/bytehost-core" }

[patch.crates-io]
foo = { path = "../foo" }

[patch."https://github.com/byteboyai/byteui"]
byteui = { git = "https://github.com/byteboyai/byteui", branch = "x" }
""")
            patches = g.find_local_patches(Path(tmp))
        self.assertEqual(patches, [("Cargo.toml", "bytehost", "bytehost-core")])

    def test_missing_lock_and_manifest_are_empty(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.assertEqual(g.load_lock(Path(tmp)), [])
            self.assertEqual(g.load_pins(Path(tmp)), {})

    def test_pins_parse_dates_and_strings(self):
        with tempfile.TemporaryDirectory() as tmp:
            write(tmp, "shared-deps.toml", """\
[pin.dozer]
reason = "等一次性同步"
review_by = 2026-11-15

[pin.bytegit]
reason = "x"
review_by = "2026-12-01"
""")
            pins = g.load_pins(Path(tmp))
        self.assertEqual(pins["dozer"], g.Pin("等一次性同步", datetime.date(2026, 11, 15)))
        self.assertEqual(pins["bytegit"].review_by, datetime.date(2026, 12, 1))



def dep(crate, repo="bytehost", kind="tag", ref="v0.4.0", manifest="Cargo.toml", graph="."):
    return g.Dep(crate, repo, kind, ref, manifest, graph)


class OfflineRuleTests(unittest.TestCase):
    def test_local_patch_blocks(self):
        issues = g.check_local_patch([("Cargo.toml", "bytehost", "bytehost-core")])
        self.assertEqual([(i.level, i.rule) for i in issues], [("block", "local-patch")])
        self.assertIn("bytehost-core", issues[0].message)
        self.assertEqual(g.check_local_patch([]), [])

    def test_local_patch_is_one_issue_per_manifest_and_repo(self):
        patches = [("Cargo.toml", "bytehost", "bytehost-core"), ("Cargo.toml", "bytehost", "bytehost-apps"),
                   ("Cargo.toml", "byteui", "byteui")]
        issues = g.check_local_patch(patches)
        self.assertEqual(len(issues), 2)
        self.assertIn("bytehost-apps, bytehost-core", issues[0].message)

    def test_lock_not_git_is_one_issue_listing_every_crate(self):
        deps = [dep("bytehost-core"), dep("bytehost-apps")]
        packages = [{"name": "bytehost-core", "version": "0.1.0"}, {"name": "bytehost-apps", "version": "0.1.0"}]
        issues = g.check_lock(deps, packages)
        self.assertEqual(len(issues), 1)
        self.assertIn("bytehost-apps, bytehost-core", issues[0].message)

    def test_lock_not_git_flags_shared_crate_without_git_source(self):
        deps = [dep("bytehost-core")]
        packages = [
            {"name": "bytehost-core", "version": "0.1.0"},  # 被本地 patch,没有 source
            {"name": "serde", "version": "1.0.0", "source": "registry+https://github.com/rust-lang/crates.io-index"},
            {"name": "digger-desktop", "version": "0.1.0"},  # 本仓库成员,不是共享库
        ]
        issues = g.check_lock(deps, packages)
        self.assertEqual([(i.level, i.rule) for i in issues], [("block", "lock-not-git")])
        self.assertIn("bytehost-core", issues[0].message)

    def test_lock_with_git_source_passes(self):
        packages = [{"name": "bytehost-core", "version": "0.1.0",
                     "source": "git+https://github.com/byteboyai/bytehost?tag=v0.4.0#dc88133d"}]
        self.assertEqual(g.check_lock([dep("bytehost-core")], packages), [])

    def test_lock_duplicate_blocks_two_git_sources_for_one_crate(self):
        packages = [
            {"name": "bytehost-core", "version": "0.1.0",
             "source": "git+https://github.com/byteboyai/bytehost?tag=v0.4.0#dc88133d2e959355bd7d1bd4c6138177074c168e"},
            {"name": "bytehost-core", "version": "0.1.0",
             "source": "git+https://github.com/byteboyai/bytehost?tag=v0.4.1#79212c08834ca946148fd7977d3d56d72b34c008"},
            {"name": "byteui", "version": "0.4.4",
             "source": "git+https://github.com/byteboyai/byteui?tag=v0.4.4#aaaaaaaaaaaaaaaa"},
        ]
        issues = g.check_lock_duplicates(packages)
        self.assertEqual([(i.level, i.rule) for i in issues], [("block", "lock-duplicate")])
        self.assertIn("bytehost-core", issues[0].message)
        self.assertIn("bytehost?tag=v0.4.0#dc88133", issues[0].message)
        self.assertIn("bytehost?tag=v0.4.1#79212c0", issues[0].message)

    def test_lock_duplicate_ignores_registry_crates_and_single_sources(self):
        packages = [
            {"name": "syn", "version": "1.0.0", "source": "registry+https://github.com/rust-lang/crates.io-index"},
            {"name": "syn", "version": "2.0.0", "source": "registry+https://github.com/rust-lang/crates.io-index"},
            {"name": "byteui", "version": "0.4.4", "source": "git+https://github.com/byteboyai/byteui?tag=v0.4.4#abc"},
        ]
        self.assertEqual(g.check_lock_duplicates(packages), [])

    def test_mismatch_blocks_when_one_repo_has_two_refs(self):
        deps = [dep("dozer-model", "dozer", "rev", "aaa"), dep("dozer-services", "dozer", "rev", "bbb"),
                dep("bytehost-core")]
        issues = g.check_mismatch(deps)
        self.assertEqual([(i.level, i.rule) for i in issues], [("block", "ref-mismatch")])
        self.assertIn("dozer", issues[0].message)
        self.assertNotIn("bytehost", issues[0].message)

    def test_mismatch_passes_when_consistent_even_across_manifests(self):
        deps = [dep("bytehost-core", manifest="a/Cargo.toml"), dep("bytehost-apps", manifest="b/Cargo.toml")]
        self.assertEqual(g.check_mismatch(deps), [])

    def test_pins_expired_and_stale(self):
        deps = [dep("dozer-model", "dozer", "rev", "aaa")]
        pins = {"dozer": g.Pin("等同步", datetime.date(2026, 11, 15)),
                "gone": g.Pin("旧的", None)}
        today = datetime.date(2026, 11, 16)
        issues = g.check_pins(deps, pins, today)
        self.assertEqual(sorted((i.level, i.rule) for i in issues),
                         [("warn", "manifest-stale"), ("warn", "pin-expired")])

    def test_pin_not_expired_on_the_review_day(self):
        deps = [dep("dozer-model", "dozer", "rev", "aaa")]
        pins = {"dozer": g.Pin("等同步", datetime.date(2026, 11, 15))}
        self.assertEqual(g.check_pins(deps, pins, datetime.date(2026, 11, 15)), [])

    def test_mismatch_is_per_workspace(self):
        # 排除在主工作区之外的 spike 自带 [workspace]、自带 Cargo.lock,旧版本不会和主图冲突
        deps = [dep("bytehost-apps", ref="v0.4.1"),
                dep("bytehost-apps", ref="v0.2.0", manifest="spike/x/Cargo.toml", graph="spike/x")]
        self.assertEqual(g.check_mismatch(deps), [])
        deps.append(dep("bytehost-core", ref="v0.4.0", manifest="spike/x/Cargo.toml", graph="spike/x"))
        issues = g.check_mismatch(deps)
        self.assertEqual([i.rule for i in issues], ["ref-mismatch"])
        self.assertIn("spike/x", issues[0].message)


class FakeRemote:
    """refs_by_repo: {"bytehost": {"refs/tags/v0.4.0": "sha", ...}};缺失的仓库视为无法联网。"""

    def __init__(self, refs_by_repo, commits=None):
        self.refs_by_repo = refs_by_repo
        self.commits = commits or {}

    def refs(self, repo):
        return self.refs_by_repo.get(repo)

    def has_commit(self, repo, rev):
        return self.commits.get((repo, rev))


def tags(*names, **extra):
    refs = {f"refs/tags/{n}": f"sha-{n}" for n in names}
    refs.update(extra)
    return refs


class SemverTests(unittest.TestCase):
    def test_parse(self):
        self.assertEqual(g.parse_semver("v0.4.1"), (0, 4, 1))
        self.assertEqual(g.parse_semver("1.2.3"), (1, 2, 3))
        self.assertIsNone(g.parse_semver("libs-2026-10-16"))
        self.assertIsNone(g.parse_semver("v1.2.3-rc1"))

    def test_latest_ignores_peeled_and_non_semver_and_compares_numerically(self):
        refs = tags("v0.9.0", "v0.10.0", "libs-2026-10-16", **{"refs/tags/v0.10.0^{}": "x",
                                                              "refs/heads/v9.9.9": "y"})
        self.assertEqual(g.latest_semver_tag(refs), "v0.10.0")
        self.assertIsNone(g.latest_semver_tag(tags("libs-2026-10-16")))

    def test_gap_wording(self):
        self.assertEqual(g.describe_gap((0, 2, 0), (0, 4, 0)), "2 个 minor")
        self.assertEqual(g.describe_gap((0, 4, 0), (1, 0, 0)), "1 个 major")
        self.assertEqual(g.describe_gap((0, 4, 0), (0, 4, 3)), "3 个 patch")


class RemoteRuleTests(unittest.TestCase):
    def levels(self, issues):
        return sorted((i.level, i.rule) for i in issues)

    def test_missing_tag_blocks(self):
        remote = FakeRemote({"bytehost": tags("v0.4.0")})
        issues = g.check_remote([dep("bytehost-core", ref="v0.9.9")], {}, remote)
        self.assertIn(("block", "ref-missing"), self.levels(issues))

    def test_missing_branch_blocks_and_existing_branch_passes(self):
        remote = FakeRemote({"bytehost": {"refs/heads/main": "s"}})
        self.assertEqual(g.check_remote([dep("a", kind="branch", ref="main")], {}, remote), [])
        issues = g.check_remote([dep("a", kind="branch", ref="gone")], {}, remote)
        self.assertEqual(self.levels(issues), [("block", "ref-missing")])

    def test_rev_matching_a_remote_tip_passes_without_fetch(self):
        remote = FakeRemote({"dozer": {"refs/heads/main": "7cdbffd0178c50891d4c39f8260224e6b76b79e5"}})
        d = dep("dozer-model", "dozer", "rev", "7cdbffd0178c50891d4c39f8260224e6b76b79e5")
        self.assertEqual(g.check_remote([d], {"dozer": g.Pin("x", None)}, remote), [])

    def test_rev_not_at_a_tip_falls_back_to_has_commit(self):
        sha = "67ab0e657d75135b882ace2f3aa7a56617776c60"
        d = dep("dozer-model", "dozer", "rev", sha)
        refs = {"dozer": {"refs/heads/main": "ffffffffffffffffffffffffffffffffffffffff"}}
        pins = {"dozer": g.Pin("x", None)}
        found = g.check_remote([d], pins, FakeRemote(refs, {("dozer", sha): True}))
        missing = g.check_remote([d], pins, FakeRemote(refs, {("dozer", sha): False}))
        unknown = g.check_remote([d], pins, FakeRemote(refs, {}))
        self.assertEqual(found, [])
        self.assertEqual(self.levels(missing), [("block", "ref-missing")])
        self.assertEqual(self.levels(unknown), [("warn", "ref-unverified")])

    def test_unreachable_remote_warns_and_never_blocks(self):
        issues = g.check_remote([dep("bytehost-core")], {}, FakeRemote({}))
        self.assertEqual(self.levels(issues), [("warn", "ref-unverified")])

    def test_behind_warns_with_gap(self):
        remote = FakeRemote({"bytehost": tags("v0.2.0", "v0.4.0")})
        issues = g.check_remote([dep("bytehost-core", ref="v0.2.0")], {}, remote)
        self.assertEqual(self.levels(issues), [("warn", "behind")])
        self.assertIn("v0.2.0 -> 最新 v0.4.0", issues[0].message)
        self.assertIn("2 个 minor", issues[0].message)

    def test_up_to_date_is_silent(self):
        remote = FakeRemote({"bytehost": tags("v0.2.0", "v0.4.0")})
        self.assertEqual(g.check_remote([dep("bytehost-core", ref="v0.4.0")], {}, remote), [])

    def test_pinned_repo_is_never_behind(self):
        remote = FakeRemote({"bytehost": tags("v0.2.0", "v0.4.0")})
        pins = {"bytehost": g.Pin("等同步", None)}
        self.assertEqual(g.check_remote([dep("bytehost-core", ref="v0.2.0")], pins, remote), [])

    def test_non_semver_tag_is_checked_for_existence_only(self):
        remote = FakeRemote({"dozer": tags("libs-2026-10-16", "v0.5.153")})
        d = dep("dozer-model", "dozer", "tag", "libs-2026-10-16")
        self.assertEqual(g.check_remote([d], {}, remote), [])

    def test_guard_source_info_only_when_canonical_has_newer_semver_tag(self):
        remote = FakeRemote({"bytehost": tags("v0.4.1", "v0.5.0")})
        issues = g.check_guard_source(remote, "v0.4.1")
        self.assertEqual([(i.level, i.rule) for i in issues], [("info", "guard-source")])
        self.assertEqual(g.check_guard_source(remote, "v0.5.0"), [])
        self.assertEqual(g.check_guard_source(remote, "unreleased"), [])
        self.assertEqual(g.check_guard_source(FakeRemote({}), "v0.4.1"), [])


class GitRemoteHelperTests(unittest.TestCase):
    def test_parse_ls_remote_keeps_peeled_entries(self):
        out = ("aaa\trefs/heads/main\n"
               "bbb\trefs/tags/v0.4.0\n"
               "ccc\trefs/tags/v0.4.0^{}\n")
        self.assertEqual(g.parse_ls_remote(out),
                         {"refs/heads/main": "aaa", "refs/tags/v0.4.0": "bbb", "refs/tags/v0.4.0^{}": "ccc"})
        self.assertEqual(g.parse_ls_remote(""), {})

    def test_fetch_failure_classification(self):
        self.assertIs(g.classify_fetch_failure("fatal: remote error: upload-pack: not our ref abc"), False)
        self.assertIs(g.classify_fetch_failure("fatal: couldn't find remote ref abc"), False)
        self.assertIsNone(g.classify_fetch_failure("fatal: unable to access: LibreSSL SSL_connect: SSL_ERROR_SYSCALL"))
        self.assertIsNone(g.classify_fetch_failure("fatal: could not read Username"))

    def test_short_rev_is_unverifiable_without_network(self):
        self.assertIsNone(g.GitRemote().has_commit("dozer", "67ab0e6"))


class RenderAndDowngradeTests(unittest.TestCase):
    ISSUES = [g.Issue(g.INFO, "i", "info msg"), g.Issue(g.WARN, "w", "warn msg"),
              g.Issue(g.BLOCK, "b", "block msg")]

    def test_render_sorts_blocks_first_and_counts(self):
        text = g.render(self.ISSUES)
        lines = text.splitlines()
        self.assertTrue(lines[0].startswith("BLOCK"))
        self.assertTrue(lines[1].startswith("WARN"))
        self.assertTrue(lines[2].startswith("INFO"))
        self.assertEqual(lines[-1], "shared-deps: 1 个阻断, 1 个警告")
        self.assertEqual(g.render([]), "shared-deps: 没有发现问题")

    def test_downgrade_turns_only_blocks_into_warnings(self):
        out = g.downgrade_blocks(self.ISSUES)
        self.assertEqual([i.level for i in out], ["info", "warn", "warn"])
        self.assertIn("降级", out[2].message)


class MainTests(unittest.TestCase):
    TODAY = datetime.date(2026, 10, 16)

    def project(self, tmp, cargo=DIGGER_ROOT_TOML, lock=None, pins=None):
        write(tmp, "Cargo.toml", cargo)
        if lock is not None:
            write(tmp, "Cargo.lock", lock)
        if pins is not None:
            write(tmp, "shared-deps.toml", pins)

    def remote(self):
        return FakeRemote({
            "bytehost": tags("v0.4.0", "v0.4.1"),
            "byteui": tags("v0.4.4"),
            "dozer": tags("libs-2026-10-16"),
        })

    def run_main(self, tmp, *flags, remote=None):
        import contextlib
        import io
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            code = g.main(["--root", tmp, *flags], remote=remote or self.remote(), today=self.TODAY)
        return code, buf.getvalue()

    def test_clean_project_exits_zero(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.project(tmp)
            code, out = self.run_main(tmp)
        self.assertEqual(code, 0, out)
        self.assertIn("没有发现问题", out)

    def test_local_patch_blocks_and_downgrade_lets_it_through(self):
        patched = DIGGER_ROOT_TOML + """
[patch."https://github.com/byteboyai/bytehost"]
bytehost-core = { path = "../bytehost-chrome/crates/bytehost-core" }
"""
        with tempfile.TemporaryDirectory() as tmp:
            self.project(tmp, cargo=patched)
            strict_code, strict_out = self.run_main(tmp)
            lenient_code, lenient_out = self.run_main(tmp, "--downgrade-blocks")
        self.assertEqual(strict_code, 1)
        self.assertIn("BLOCK  local-patch", strict_out)
        self.assertEqual(lenient_code, 0)
        self.assertIn("WARN   local-patch", lenient_out)

    def test_lock_pointing_at_local_path_blocks(self):
        lock = '[[package]]\nname = "bytehost-core"\nversion = "0.1.0"\n'
        with tempfile.TemporaryDirectory() as tmp:
            self.project(tmp, lock=lock)
            code, out = self.run_main(tmp)
        self.assertEqual(code, 1)
        self.assertIn("lock-not-git", out)

    def test_offline_skips_remote_rules(self):
        cargo = DIGGER_ROOT_TOML.replace("v0.4.1", "v9.9.9")
        with tempfile.TemporaryDirectory() as tmp:
            self.project(tmp, cargo=cargo)
            online, _ = self.run_main(tmp)
            offline, out = self.run_main(tmp, "--offline")
        self.assertEqual(online, 1)  # tag 不存在
        self.assertEqual(offline, 0, out)

    def test_unreachable_remote_never_blocks(self):
        with tempfile.TemporaryDirectory() as tmp:
            self.project(tmp)
            code, out = self.run_main(tmp, remote=FakeRemote({}))
        self.assertEqual(code, 0, out)
        self.assertIn("ref-unverified", out)

    def test_expired_pin_warns_but_exits_zero(self):
        pins = '[pin.dozer]\nreason = "等同步"\nreview_by = 2026-10-01\n'
        with tempfile.TemporaryDirectory() as tmp:
            self.project(tmp, pins=pins)
            code, out = self.run_main(tmp)
        self.assertEqual(code, 0, out)
        self.assertIn("pin-expired", out)


class RevModeTests(unittest.TestCase):
    def git(self, tmp, *args):
        import subprocess
        subprocess.run(["git", "-C", tmp, "-c", "user.name=t", "-c", "user.email=t@t", *args],
                       check=True, capture_output=True)

    def test_rev_checks_committed_content_not_the_working_tree(self):
        import contextlib
        import io
        good_lock = ('[[package]]\nname = "bytehost-core"\nversion = "0.1.0"\n'
                     'source = "git+https://github.com/byteboyai/bytehost?tag=v0.4.1#abc"\n')
        dev_lock = '[[package]]\nname = "bytehost-core"\nversion = "0.1.0"\n'  # 本地 patch 下的样子
        remote = FakeRemote({"bytehost": tags("v0.4.1"), "byteui": tags("v0.4.4"), "dozer": tags("libs-2026-10-16")})
        with tempfile.TemporaryDirectory() as tmp:
            write(tmp, "Cargo.toml", DIGGER_ROOT_TOML)
            write(tmp, "Cargo.lock", good_lock)
            self.git(tmp, "init", "-q", "-b", "main")
            self.git(tmp, "add", "-A")
            self.git(tmp, "commit", "-q", "-m", "init")
            write(tmp, "Cargo.lock", dev_lock)  # 未提交的本地联调状态

            def run(*flags):
                buf = io.StringIO()
                with contextlib.redirect_stdout(buf):
                    code = g.main(["--root", tmp, *flags], remote=remote, today=datetime.date(2026, 10, 16))
                return code, buf.getvalue()

            worktree_code, worktree_out = run()
            rev_code, rev_out = run("--rev", "HEAD")
            bad_code, _ = run("--rev", "no-such-rev")
        self.assertEqual(worktree_code, 1, worktree_out)
        self.assertIn("lock-not-git", worktree_out)
        self.assertEqual(rev_code, 0, rev_out)
        self.assertEqual(bad_code, 2)


class AheadHintTests(unittest.TestCase):
    def test_reports_commits_not_yet_on_origin_main_even_in_rev_mode(self):
        import contextlib
        import io
        import subprocess
        with tempfile.TemporaryDirectory() as tmp:
            remote_dir, work = f"{tmp}/remote.git", f"{tmp}/work"
            subprocess.run(["git", "init", "-q", "--bare", remote_dir], check=True)
            subprocess.run(["git", "init", "-q", "-b", "main", work], check=True)

            def git(*args):
                subprocess.run(["git", "-C", work, "-c", "user.name=t", "-c", "user.email=t@t", *args],
                               check=True, capture_output=True)

            write(work, "Cargo.toml", "[workspace]\nmembers = []\n")
            git("add", "-A")
            git("commit", "-q", "-m", "one")
            git("remote", "add", "origin", remote_dir)
            git("push", "-q", "origin", "main")
            git("commit", "-q", "--allow-empty", "-m", "two")
            git("commit", "-q", "--allow-empty", "-m", "three")
            buf = io.StringIO()
            with contextlib.redirect_stdout(buf):
                code = g.main(["--root", work, "--rev", "HEAD", "--offline"], today=datetime.date(2026, 10, 16))
        self.assertEqual(code, 0)
        self.assertIn("INFO   ahead-of-remote", buf.getvalue())
        self.assertIn("领先 origin/main 2 个提交", buf.getvalue())


if __name__ == "__main__":
    unittest.main()
