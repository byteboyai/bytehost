//! 固定版本表(A6d):每个可装物的**唯一** `Pin`——版本、来源 URL、SHA-256、解压后进程的相对路径。
//!
//! **数据段 `PINS` 由 `scripts/bytehost/pin-runtimes.sh` 从官方源直接 `curl` 生成,不许手写哈希、
//! 不许模型转述**;见脚本头注释。这里只放类型、查表与纯测试。
//!
//! Python 本身**不在表里**:它由已装好的 uv 执行 `uv python install`(来源 python-build-standalone,
//! 哈希由 uv 内置校验),安装计划里会如实写明这一点。

use crate::proto::ManagedRuntime;

/// 官方只发这两个 macOS 目标;其他平台 `pin_for` 返回 `None`(界面显示"此平台暂不支持自动安装")。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Aarch64Apple,
    X86_64Apple,
}

impl Target {
    /// 当前运行的目标(只有 macOS 的 arm64/x86_64 支持)。
    pub fn current() -> Option<Target> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        if cfg!(target_arch = "aarch64") {
            Some(Target::Aarch64Apple)
        } else if cfg!(target_arch = "x86_64") {
            Some(Target::X86_64Apple)
        } else {
            None
        }
    }
}

/// 一个可装物的固定标识。所有字符串都是 `&'static`(来自编译期表,不是运行时拼出来的)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pin {
    /// `"node"` 或 `"uv"`。
    pub name: &'static str,
    pub version: &'static str,
    pub target: Target,
    pub url: &'static str,
    pub sha256: &'static str,
    /// 解压时用 `tar --strip-components` 去掉几层。
    pub strip_components: u32,
    /// 解压根下的可执行文件相对路径(如 `bin/node`、`uv`)。
    pub bin_rel: &'static str,
}

/// 传给 `uv python install` 的 Python 版本。
pub const PYTHON_VERSION: &str = "3.13";

/// 按名字与目标查固定版本。
pub fn pin_for(name: &str, target: Target) -> Option<&'static Pin> {
    PINS.iter().find(|p| p.name == name && p.target == target)
}

/// 某个 `ManagedRuntime` 需要装的可装物名字(按安装顺序):Python 先装 uv。
pub fn pin_names_for(rt: ManagedRuntime) -> &'static [&'static str] {
    match rt {
        ManagedRuntime::Node => &["node"],
        ManagedRuntime::Python => &["uv"],
    }
}

// ---- 数据段(由 scripts/bytehost/pin-runtimes.sh 生成,请勿手写)----

/// 本数据段由 scripts/bytehost/pin-runtimes.sh 从官方源生成,请勿手写。
/// 生成命令: scripts/bytehost/pin-runtimes.sh 24.21.0 0.12.23
/// Node 24.21.0 LTS, uv 0.12.23。
pub const PINS: &[Pin] = &[
    Pin {
        name: "node",
        version: "24.21.0",
        target: Target::Aarch64Apple,
        url: "https://nodejs.org/dist/v24.21.0/node-v24.21.0-darwin-arm64.tar.gz",
        sha256: "bed7eea5325e1108f32ce5228ddd6a5f0f08a499ee42aa7442aea583702f6057",
        strip_components: 1,
        bin_rel: "bin/node",
    },
    Pin {
        name: "node",
        version: "24.21.0",
        target: Target::X86_64Apple,
        url: "https://nodejs.org/dist/v24.21.0/node-v24.21.0-darwin-x64.tar.gz",
        sha256: "1462cb3b3046b815cf8ea436d3da450ec1a9f11dac7e5a46b0ada5305d7e8097",
        strip_components: 1,
        bin_rel: "bin/node",
    },
    Pin {
        name: "uv",
        version: "0.12.23",
        target: Target::Aarch64Apple,
        url: "https://github.com/astral-sh/uv/releases/download/0.12.23/uv-aarch64-apple-darwin.tar.gz",
        sha256: "50487ae565ccd96e499056b4674d438f4c53170202617b4c759defe0c6a1b544",
        strip_components: 1,
        bin_rel: "uv",
    },
    Pin {
        name: "uv",
        version: "0.12.23",
        target: Target::X86_64Apple,
        url: "https://github.com/astral-sh/uv/releases/download/0.12.23/uv-x86_64-apple-darwin.tar.gz",
        sha256: "960da44cb4b73685206ddd250b19e0a117fa41095710c1038f081f5cb613efb4",
        strip_components: 1,
        bin_rel: "uv",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_pin_is_https_and_has_a_64_hex_checksum_and_a_relative_bin() {
        for t in [Target::Aarch64Apple, Target::X86_64Apple] {
            for name in ["node", "uv"] {
                let p = pin_for(name, t).unwrap_or_else(|| panic!("{name} {t:?}"));
                assert!(p.url.starts_with("https://"), "{}", p.url);
                assert_eq!(p.sha256.len(), 64);
                assert!(
                    p.sha256
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
                );
                assert!(!p.bin_rel.starts_with('/') && !p.bin_rel.contains(".."));
                assert_eq!(p.target, t);
            }
        }
    }

    #[test]
    fn the_two_architectures_never_share_a_url_or_a_checksum() {
        for name in ["node", "uv"] {
            let a = pin_for(name, Target::Aarch64Apple).unwrap();
            let x = pin_for(name, Target::X86_64Apple).unwrap();
            assert_ne!(a.url, x.url);
            assert_ne!(a.sha256, x.sha256);
            assert_eq!(a.version, x.version, "两个架构装同一个版本");
        }
    }

    #[test]
    fn urls_match_the_official_sources() {
        let n = pin_for("node", Target::Aarch64Apple).unwrap();
        assert!(n.url.starts_with("https://nodejs.org/dist/v"));
        let u = pin_for("uv", Target::Aarch64Apple).unwrap();
        assert!(
            u.url
                .starts_with("https://github.com/astral-sh/uv/releases/download/")
        );
    }

    #[test]
    fn the_version_in_the_url_matches_the_declared_version() {
        for t in [Target::Aarch64Apple, Target::X86_64Apple] {
            for name in ["node", "uv"] {
                let p = pin_for(name, t).unwrap();
                assert!(
                    p.url.contains(p.version),
                    "{} 的 URL 里应含版本 {}",
                    p.name,
                    p.version
                );
            }
        }
    }

    #[test]
    fn unknown_names_have_no_pin() {
        assert!(pin_for("python", Target::Aarch64Apple).is_none());
        assert!(pin_for("", Target::Aarch64Apple).is_none());
    }
}
