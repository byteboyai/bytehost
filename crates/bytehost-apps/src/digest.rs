//! 摘要(feature `digest`):manifest 摘要、源码目录摘要、每应用数据存储标识。
//! 审批绑定摘要——审批之后源码或 manifest 被替换,安装时摘要对不上就拒绝(防 TOCTOU)。
//!
//! **摘要覆盖整个应用包根目录**(含图标、lockfile、manifest 引用到的一切),不只是 `runtime.source`。
//! **防 TOCTOU 的前提是安装顺序:** 先把包拷到 staging,对 **staging 里的副本**算摘要并 `verify`,再改名/落位;
//! 对源目录先算摘要再拷贝仍有窗口。文件权限位(可执行位)不在摘要里(见 A0 计划 Review Focus 2)。

use std::fs;
use std::io;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::id::AppId;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 字节串的 SHA-256,小写十六进制(64 个字符)。
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// 目录树摘要:按相对路径(`/` 分隔)字典序遍历所有文件,逐个把"路径长度、路径、内容长度、内容"喂给
/// SHA-256。文件内容、文件名、文件增删、目录结构变化都会改变摘要;遇到符号链接直接报错
/// (`InvalidInput`)——链接可以指向包外,摘要无法代表它的内容。空目录不计入(只有文件)。
pub fn digest_tree(root: &Path) -> io::Result<String> {
    let mut files = Vec::new();
    collect(root, root, &mut files)?;
    files.sort();
    let mut hasher = Sha256::new();
    for rel in &files {
        let content = fs::read(root.join(rel))?;
        hasher.update((rel.len() as u64).to_le_bytes());
        hasher.update(rel.as_bytes());
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(&content);
    }
    Ok(hex(&hasher.finalize()))
}

/// 相对路径 → 以 `/` 分隔的字符串。**非 UTF-8 名字直接拒绝**:`to_string_lossy` 会把 `a\xff` 和真正叫
/// `a\u{FFFD}` 的文件变成同一个字符串,而后面是按这个字符串回读内容的——载荷文件的字节就可能根本没进摘要。
fn rel_string(rel: &Path) -> io::Result<String> {
    let mut parts = Vec::new();
    for c in rel.components() {
        match c.as_os_str().to_str() {
            Some(s) => parts.push(s),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("应用包里的文件名必须是 UTF-8: {}", rel.display()),
                ));
            }
        }
    }
    Ok(parts.join("/"))
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<String>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("应用包里不允许符号链接: {}", path.display()),
            ));
        }
        if file_type.is_dir() {
            collect(root, &path, out)?;
        } else {
            if !file_type.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "应用包里只允许普通文件和目录(不允许 FIFO/套接字/设备): {}",
                        path.display()
                    ),
                ));
            }
            out.push(rel_string(path.strip_prefix(root).expect("在 root 之下"))?);
        }
    }
    Ok(())
}

/// 每应用一个稳定的 WebView 数据存储标识(16 字节):由 app id 确定性派生,卸载程序再重装后不变
/// (因此保留下来的 `data/` 与浏览器存储仍然对得上)。不同 id 得到不同标识,实测不同标识即使 origin
/// 相同,localStorage/IndexedDB/Cookie 也互相隔离。
pub fn data_store_id(id: &AppId) -> [u8; 16] {
    let digest = Sha256::digest(format!("bytehost-app-data-store:{id}").as_bytes());
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// 同 [`data_store_id`],小写十六进制(32 个字符),落盘用。
pub fn data_store_id_hex(id: &AppId) -> String {
    hex(&data_store_id(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    #[test]
    fn sha256_matches_the_known_empty_and_abc_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn tree_digest_is_stable_and_independent_of_creation_order() {
        let a = tempfile::tempdir().unwrap();
        write(a.path(), "index.html", "<h1>x</h1>");
        write(a.path(), "js/app.js", "1");
        let b = tempfile::tempdir().unwrap();
        write(b.path(), "js/app.js", "1");
        write(b.path(), "index.html", "<h1>x</h1>");
        assert_eq!(
            digest_tree(a.path()).unwrap(),
            digest_tree(b.path()).unwrap()
        );
        assert_eq!(digest_tree(a.path()).unwrap().len(), 64);
    }

    #[test]
    fn tree_digest_changes_with_content_name_addition_and_removal() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "index.html", "a");
        let base = digest_tree(d.path()).unwrap();

        write(d.path(), "index.html", "b");
        let edited = digest_tree(d.path()).unwrap();
        assert_ne!(edited, base, "内容变化");

        // 同样长度的新名字:否则"路径长度"会掩盖"路径内容没进摘要"
        fs::rename(d.path().join("index.html"), d.path().join("other.html")).unwrap();
        let renamed = digest_tree(d.path()).unwrap();
        assert_ne!(renamed, edited, "仅改名");

        write(d.path(), "extra.txt", "");
        let added = digest_tree(d.path()).unwrap();
        assert_ne!(added, renamed, "新增一个空文件");

        fs::remove_file(d.path().join("extra.txt")).unwrap();
        assert_eq!(
            digest_tree(d.path()).unwrap(),
            renamed,
            "删回去就回到原摘要"
        );
    }

    #[test]
    fn tree_digest_cannot_be_confused_by_moving_bytes_between_name_and_content() {
        let a = tempfile::tempdir().unwrap();
        write(a.path(), "ab", "c");
        let b = tempfile::tempdir().unwrap();
        write(b.path(), "a", "bc");
        assert_ne!(
            digest_tree(a.path()).unwrap(),
            digest_tree(b.path()).unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn tree_digest_refuses_symlinks_because_they_can_point_outside_the_package() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "index.html", "a");
        std::os::unix::fs::symlink("/etc/hosts", d.path().join("link")).unwrap();
        let err = digest_tree(d.path()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn data_store_id_is_deterministic_per_app_and_differs_between_apps() {
        let a = AppId::new("excalidraw").unwrap();
        let b = AppId::new("drawio").unwrap();
        assert_eq!(data_store_id(&a), data_store_id(&a));
        assert_ne!(data_store_id(&a), data_store_id(&b));
        assert_eq!(data_store_id_hex(&a).len(), 32);
        assert!(data_store_id_hex(&a).bytes().all(|c| c.is_ascii_hexdigit()));
    }

    /// 用合成路径测(不依赖文件系统是否允许非 UTF-8 名字):`to_string_lossy` 会把 `a\xff` 和真正叫
    /// `a\u{FFFD}` 的文件变成同一个字符串,摘要就可能漏掉其中一个文件的内容。
    #[cfg(unix)]
    #[test]
    fn non_utf8_names_are_refused_not_converted_lossily() {
        use std::os::unix::ffi::OsStrExt;
        let raw = Path::new(std::ffi::OsStr::from_bytes(b"dir/a\xff"));
        assert_eq!(
            rel_string(raw).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(rel_string(Path::new("dir/a.txt")).unwrap(), "dir/a.txt");
    }

    #[cfg(unix)]
    #[test]
    fn tree_digest_refuses_fifos_instead_of_blocking_on_them() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "index.html", "a");
        let status = std::process::Command::new("mkfifo")
            .arg(d.path().join("pipe"))
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            digest_tree(d.path()).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
