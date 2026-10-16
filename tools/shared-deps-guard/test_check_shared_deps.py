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


if __name__ == "__main__":
    unittest.main()
