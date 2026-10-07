//! A6d 真实网络冒烟(默认不跑):用真 `CurlFetcher`/`TarArchive` 从官方源装 Node 与
//! Python(uv + `uv python install`),断言能跑起来、受管解析器能找到。
//!
//! 只在人工要求时运行:
//! `cargo test -p bytehost-apps --all-features --test runtime_install_live -- --ignored`
//!
//! **没有网络时失败而不是跳过**——它本来就只有被显式 `--ignored` 才会跑。

#![cfg(unix)]

use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;

use bytehost_apps::proto::ManagedRuntime;
use bytehost_apps::runtime::managed::{
    CurlFetcher, ManagedResolver, RuntimeManager, RuntimeStore, TarArchive,
};
use bytehost_apps::runtime::{RuntimeResolver, SystemResolver};

fn wait_finished(mgr: &RuntimeManager, rt: ManagedRuntime, secs: u64) {
    let deadline = Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let jobs = mgr.jobs();
        if let Some(j) = jobs.iter().find(|j| j.runtime == rt)
            && j.finished
        {
            if let Some(why) = &j.failed {
                panic!("{rt:?} 安装失败: {why}");
            }
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{rt:?} 安装超时({secs}s);最后任务状态: {:?}",
            mgr.jobs()
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn run(bin: &PathBuf, args: &[&str]) -> String {
    let out = Command::new(bin)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("跑 {} {:?} 失败: {e:?}", bin.display(), args));
    assert!(
        out.status.success(),
        "{} {:?} 退出码非零: {}",
        bin.display(),
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
#[ignore = "真实网络下载,仅在人工要求时运行"]
fn installs_node_and_python_from_the_official_sources() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("runtimes");
    let mgr = RuntimeManager::new(&root);

    // ---- Node ----
    let t = Instant::now();
    let node_plan = mgr.plan(ManagedRuntime::Node).expect("出 Node 计划");
    mgr.start_install(&node_plan).expect("起 Node 安装");
    wait_finished(&mgr, ManagedRuntime::Node, 300);
    let node_secs = t.elapsed().as_secs();
    let node_versions = mgr.installed(ManagedRuntime::Node);
    assert_eq!(node_versions.len(), 1, "{node_versions:?}");

    // 受管解析器(优先于系统)能找到 node/npm/npx 并跑起来。
    let resolver = ManagedResolver::new(RuntimeStore::new(&root));
    let node = resolver
        .resolve("node")
        .unwrap_or_else(|e| panic!("解析受管 node 失败: {e:?}"));
    let ver = run(&node.program, &["--version"]);
    assert!(ver.starts_with('v'), "node --version 输出异常: {ver}");
    assert!(resolver.resolve("npm").is_ok(), "受管 node 应带 npm");
    eprintln!("[live] Node 安装耗时 {node_secs}s,版本 {ver}");

    // 链式解析:受管优先,系统兜底。
    let chain = bytehost_apps::runtime::managed::ChainResolver(vec![
        Arc::new(ManagedResolver::new(RuntimeStore::new(&root))) as Arc<dyn RuntimeResolver>,
        Arc::new(SystemResolver::new()) as Arc<dyn RuntimeResolver>,
    ]);
    assert!(chain.resolve("node").is_ok());

    // ---- Python(uv + `uv python install`) ----
    let t = Instant::now();
    let py_plan = mgr.plan(ManagedRuntime::Python).expect("出 Python 计划");
    mgr.start_install(&py_plan).expect("起 Python 安装");
    wait_finished(&mgr, ManagedRuntime::Python, 600);
    let py_secs = t.elapsed().as_secs();

    let py = resolver
        .resolve("python3")
        .unwrap_or_else(|e| panic!("解析受管 python3 失败: {e:?}"));
    let pyver = run(&py.program, &["--version"]);
    assert!(pyver.starts_with("Python 3.13"), "python 版本异常: {pyver}");
    eprintln!("[live] Python 安装耗时 {py_secs}s,版本 {pyver}");

    // 真下载/解压走的是系统 curl/tar(在这里只是引用,避免把类型判成未用)。
    let _ = (CurlFetcher, TarArchive);
}
