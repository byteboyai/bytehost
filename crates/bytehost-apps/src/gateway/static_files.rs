//! 静态文件服务:URL 路径 → 应用包里的文件。**路径解析是安全边界**——任何一种写法都不能走出站点根目录。

use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resolved {
    File(PathBuf),
    NotFound,
    /// 试图走出根目录(`..`、符号链接逃逸)。
    Forbidden,
    /// 编码非法、含 NUL 或反斜杠。
    BadRequest,
}

/// 百分号解码。非法序列(`%` 后不是两位十六进制)或解出来不是 UTF-8 返回 `None`。
pub(crate) fn percent_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hi = hex_val(*bytes.get(i + 1)?)?;
            let lo = hex_val(*bytes.get(i + 2)?)?;
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 把 URL 路径(不含查询串)解析成 `root` 之下的文件。目录与 `/` 落到 `index.html`。
pub(crate) fn resolve(root: &Path, url_path: &str) -> Resolved {
    let Some(decoded) = percent_decode(url_path) else {
        return Resolved::BadRequest;
    };
    if decoded.contains('\0') || decoded.contains('\\') {
        return Resolved::BadRequest;
    }
    let mut candidate = root.to_path_buf();
    for seg in decoded.split('/') {
        match seg {
            "" | "." => {}
            ".." => return Resolved::Forbidden,
            s => candidate.push(s),
        }
    }
    if candidate.is_dir() {
        candidate.push("index.html");
    }
    let (Ok(real), Ok(real_root)) = (candidate.canonicalize(), root.canonicalize()) else {
        return Resolved::NotFound;
    };
    if !real.starts_with(&real_root) {
        return Resolved::Forbidden;
    }
    if real.is_file() {
        Resolved::File(real)
    } else {
        Resolved::NotFound
    }
}

pub(crate) fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("html" | "htm") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json" | "map") => "application/json",
        Some("txt") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::write_files;
    use std::fs;

    fn site() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        write_files(
            &root,
            &[
                ("index.html", "<h1>home</h1>"),
                ("assets/app.js", "1"),
                ("docs/index.html", "docs"),
                ("a b/c.txt", "space"),
            ],
        );
        write_files(tmp.path(), &[("secret.txt", "TOP SECRET")]);
        (tmp, root)
    }

    fn file(root: &Path, rel: &str) -> Resolved {
        Resolved::File(root.canonicalize().unwrap().join(rel))
    }

    #[test]
    fn percent_decoding_handles_valid_invalid_and_non_utf8_input() {
        assert_eq!(
            percent_decode("/a%20b/%E4%B8%AD").as_deref(),
            Some("/a b/中")
        );
        assert_eq!(percent_decode("/plain").as_deref(), Some("/plain"));
        assert_eq!(percent_decode("/%2e%2E").as_deref(), Some("/.."));
        for bad in ["/%", "/%a", "/%zz", "/%g1", "/%FF%FE"] {
            assert_eq!(percent_decode(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_root_and_directories_resolve_to_their_index_html() {
        let (_tmp, root) = site();
        assert_eq!(resolve(&root, "/"), file(&root, "index.html"));
        assert_eq!(resolve(&root, ""), file(&root, "index.html"));
        assert_eq!(resolve(&root, "/docs"), file(&root, "docs/index.html"));
        assert_eq!(resolve(&root, "/docs/"), file(&root, "docs/index.html"));
        assert_eq!(
            resolve(&root, "/assets/app.js"),
            file(&root, "assets/app.js")
        );
        assert_eq!(resolve(&root, "/a%20b/c.txt"), file(&root, "a b/c.txt"));
        assert_eq!(
            resolve(&root, "/./assets//app.js"),
            file(&root, "assets/app.js"),
            "空段与 . 段被忽略"
        );
    }

    #[test]
    fn missing_files_and_directories_without_an_index_are_not_found() {
        let (_tmp, root) = site();
        assert_eq!(resolve(&root, "/nope.html"), Resolved::NotFound);
        assert_eq!(
            resolve(&root, "/assets"),
            Resolved::NotFound,
            "assets/ 没有 index.html"
        );
    }

    #[test]
    fn no_spelling_of_a_parent_reference_can_leave_the_site_root() {
        let (_tmp, root) = site();
        for target in [
            "/../secret.txt",
            "/%2e%2e/secret.txt",
            "/%2E%2E/secret.txt",
            "/assets/../../secret.txt",
            "/assets/%2e%2e/%2e%2e/secret.txt",
            "/..",
        ] {
            assert_eq!(resolve(&root, target), Resolved::Forbidden, "{target}");
        }
        // 编码后的斜杠只是普通分隔符,不会绕过 .. 检查
        assert_eq!(resolve(&root, "/%2e%2e%2fsecret.txt"), Resolved::Forbidden);
        // 反斜杠、NUL、坏编码直接 400
        for target in ["/..%5csecret.txt", "/a%5cb", "/%00", "/%zz", "/%FF"] {
            assert_eq!(resolve(&root, target), Resolved::BadRequest, "{target}");
        }
        // 双重编码只是一个普通文件名,找不到
        assert_eq!(resolve(&root, "/%252e%252e/secret.txt"), Resolved::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_pointing_outside_the_root_is_forbidden() {
        let (tmp, root) = site();
        std::os::unix::fs::symlink(tmp.path().join("secret.txt"), root.join("leak.txt")).unwrap();
        std::os::unix::fs::symlink(tmp.path(), root.join("up")).unwrap();
        assert_eq!(resolve(&root, "/leak.txt"), Resolved::Forbidden);
        assert_eq!(resolve(&root, "/up/secret.txt"), Resolved::Forbidden);
        // 指向根内部的链接没问题
        std::os::unix::fs::symlink(root.join("assets/app.js"), root.join("inner.js")).unwrap();
        assert_eq!(resolve(&root, "/inner.js"), file(&root, "assets/app.js"));
        let _ = fs::remove_file(root.join("inner.js"));
    }

    #[test]
    fn mime_types_cover_what_web_apps_ship_and_default_to_octet_stream() {
        let m = |name: &str| mime_for(Path::new(name));
        assert_eq!(m("a.html"), "text/html; charset=utf-8");
        assert_eq!(m("A.HTM"), "text/html; charset=utf-8");
        assert_eq!(m("a.mjs"), "text/javascript; charset=utf-8");
        assert_eq!(m("a.css"), "text/css; charset=utf-8");
        assert_eq!(m("a.json"), "application/json");
        assert_eq!(m("a.svg"), "image/svg+xml");
        assert_eq!(m("a.woff2"), "font/woff2");
        assert_eq!(m("a.wasm"), "application/wasm");
        assert_eq!(m("a.png"), "image/png");
        assert_eq!(m("noext"), "application/octet-stream");
        assert_eq!(m("a.unknown"), "application/octet-stream");
    }
}
