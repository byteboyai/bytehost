//! 浏览器面板里**任意网页**的导航策略(纯函数,不依赖 wry/iced)。
//!
//! 与 [`crate::AppOrigin`] 不是一回事:应用 webview 只能留在自己的 origin 里;浏览器要能去任意站点,
//! 所以这里不比 origin,而是**按 scheme 圈出能去哪类地址**,把"能执行代码 / 能冒充内部页面 / 会拉起
//! 外部程序"的 scheme 全部挡掉。页面内容不可信:`file://` 页面也不例外,不得拿到任何宿主权限
//! (浏览器子视图不注册自定义协议、不装 IPC,见 `bytehost-desktop::web_child`)。
//!
//! 放行:`http`、`https`、`file`(仅本机)、`about:blank`。其余一律拒绝,包括 `javascript:`、`data:`、
//! `blob:`、`view-source:`、`ws(s):`、`ftp:`、`tel:`、`mailto:`,以及任何产品内部 scheme
//! (`dozer://`、`digger-*://`)。

use url::Url;

/// 这个 URL 能不能作为浏览器页签的导航目标。
///
/// 解析之前先拒绝三类字符/形态,因为 WHATWG 解析器会"帮忙"修正它们,于是危险输入被当成合法的:
/// - 控制字符(含制表符、换行、`U+0085` 等):解析器会**悄悄剥掉**制表符和换行,`java\nscript:` 会被还原;
/// - 反斜杠:对 http(s) 被当成 `/`,`https://a.com\@evil.com` 会被当成 host=a.com;
/// - 首尾空白:解析器会 trim,但导航回调给的 URL 不会带,带了说明来路不对。
///
/// 长度不设上限(地址栏接受任意长 URL)。
pub fn allows_browse_navigation(url: &str) -> bool {
    if url.is_empty() || url.trim() != url || url.chars().any(|c| c.is_control() || c == '\\') {
        return false;
    }
    let Ok(parsed) = Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "http" | "https" => parsed.host_str().is_some_and(|host| !host.is_empty()),
        // 只认本机文件。`file://host/share` 是网络共享(SMB 等),解析器会保留 host,这里拒绝;
        // `file://localhost/...` 被解析器规范成空 host,放行。
        "file" => parsed.host_str().is_none_or(str::is_empty),
        // 只放行精确的 `about:blank`(带 `#`/`?` 的、`about:srcdoc` 等都不要)。
        "about" => url.eq_ignore_ascii_case("about:blank"),
        _ => false,
    }
}

/// 交给 wry 之前把 URL 规范成合法的、已百分号编码的形式。
///
/// wry 的 `navigate_to_url` 里是 `NSURL::URLWithString(..).unwrap()`:含空格、中文的 URL 在较老的 macOS
/// 上会让 `URLWithString` 返回 nil,然后 panic。macOS 26 上它会自动编码、不 panic(探针实测),但旧系统
/// 没法测,所以宿主自己编一遍,不依赖系统行为。用标准解析器序列化:空格 → `%20`、中文路径 → UTF-8
/// 百分号编码、中文域名 → punycode、已编码的部分不动(**幂等**)、空路径补 `/`(WebKit 视为同一地址)。
///
/// **危险输入原样返回**(控制字符、反斜杠、首尾空白):标准解析器会"帮忙"修正它们——把 `\` 当成 `/`、
/// 剥掉制表符——那样 `https://a.com\@evil.com` 会被洗成一个合法 URL 而逃过 [`allows_browse_navigation`]。
/// 原样返回,它在导航策略那一步就会被拒绝。解析不了的输入同样原样返回,不 panic。
pub fn encode_for_wry(url: &str) -> String {
    if url.trim() != url || url.chars().any(|c| c.is_control() || c == '\\') {
        return url.to_owned();
    }
    match Url::parse(url) {
        Ok(parsed) => parsed.as_str().to_owned(),
        Err(_) => url.to_owned(),
    }
}

/// 页面请求新窗口(`target="_blank"` / `window.open`)时能不能开成新页签:与导航走**同一张表**,
/// 不另开一份,免得两处判断分叉。
pub fn allows_new_tab(url: &str) -> bool {
    allows_browse_navigation(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_navigation_allows_only_the_listed_schemes() {
        for url in [
            "https://a.com",
            "https://a.com/",
            "http://a.com:8080/x?y#z",
            "HTTPS://A.COM",
            "https://user:pw@a.com/p",
            "https://[::1]:3000/x",
            "http://127.0.0.1:18765/media.html",
            "file:///Users/me/My%20File.html",
            "file:///Users/me/中文.html",
            // 地址栏里用户直接敲的未编码形式(空格)也算合法的本地文件地址。
            "file:///Users/me/My File.html",
            "file://localhost/Users/me/a.html",
            "about:blank",
            "ABOUT:BLANK",
            // 解析器把多余的斜杠吞掉,规范成 http://x/(WebKit 同样处理),策略与实际导航目标一致。
            "http:///x",
        ] {
            assert!(allows_browse_navigation(url), "应放行: {url:?}");
        }
    }

    #[test]
    fn browse_navigation_rejects_everything_else() {
        for url in [
            // 代码 / 内联内容 / 内部 scheme
            "javascript:alert(1)",
            "JavaScript:alert(1)",
            "data:text/html,<script>1</script>",
            "blob:https://a.com/00000000-0000-0000-0000-000000000000",
            "view-source:https://a.com",
            "dozer://a/b",
            "digger-review://review-trace/host.html",
            "digger-todo://localhost/host.html",
            // 外部应用 / 其它协议
            "tel:123",
            "mailto:a@b.c",
            "ftp://a/b",
            "ws://a.com/",
            "wss://a.com/",
            // 只放行 about:blank,别的 about: 一律不要
            "about:srcdoc",
            "about:blank#x",
            "about:blank?x",
            // 非本机主机的 file(网络共享)
            "file://evil/share/x.html",
            "file://192.168.1.2/share/x.html",
            // 畸形
            "",
            "   ",
            "https://",
            "https://:80/",
            "not a url",
            "//a.com/x",
            "/Users/me/a.html",
        ] {
            assert!(!allows_browse_navigation(url), "应拒绝: {url:?}");
        }
    }

    /// WHATWG 解析器会**悄悄剥掉**制表符/换行,并把反斜杠当斜杠;不先拒绝它们,
    /// `https://a.com\@evil` 会被当成 host=a.com 放行,`java\tscript:` 之类也会被还原。
    #[test]
    fn control_characters_and_backslashes_are_rejected_before_parsing() {
        for url in [
            "https://a.com/\n",
            "https://a.com/\r\nx",
            "https://a.com/\tx",
            "htt\tps://a.com",
            "java\nscript:alert(1)",
            "https://a.com\\@evil.com/",
            "https:\\\\a.com",
            "file:///Users/me\\x.html",
            "https://a.com/\u{0}",
            "https://a.com/\u{7f}",
            "https://a.com/\u{85}",
        ] {
            assert!(!allows_browse_navigation(url), "应拒绝: {url:?}");
        }
    }

    /// 解析器还会 trim 首尾空白;导航回调给的 URL 不会带,带了说明来路不对。
    #[test]
    fn leading_and_trailing_whitespace_is_rejected() {
        for url in [
            " https://a.com",
            "https://a.com ",
            "\u{a0}https://a.com",
            "https://a.com\u{3000}",
        ] {
            assert!(!allows_browse_navigation(url), "应拒绝: {url:?}");
        }
    }

    /// 超长 URL 不 panic、不因长度被拒(地址栏黄金测试里 10 万字符的 URL 是被接受的)。
    #[test]
    fn very_long_urls_are_judged_by_shape_not_length() {
        let long = format!("https://a.com/{}", "x".repeat(100_000));
        assert!(allows_browse_navigation(&long));
        let long_bad = format!("javascript:{}", "x".repeat(100_000));
        assert!(!allows_browse_navigation(&long_bad));
    }

    /// 新窗口请求(`target=_blank` / `window.open`)走同一张表:能开新页签的 URL 恰好是能导航的 URL。
    #[test]
    fn new_tab_requests_use_the_same_policy() {
        assert!(allows_new_tab("https://a.com/x"));
        assert!(!allows_new_tab("javascript:void(0)"));
        assert!(!allows_new_tab(""));
    }

    // —— encode_for_wry ——

    #[test]
    fn urls_are_percent_encoded_before_reaching_wry() {
        for (input, expected) in [
            // 空格、中文:wry 的 `NSURL::URLWithString(..).unwrap()` 在旧系统上会因此 panic。
            ("https://a.com/x y", "https://a.com/x%20y"),
            (
                "https://a.com/p?q=a b&r=中",
                "https://a.com/p?q=a%20b&r=%E4%B8%AD",
            ),
            (
                "file:///Users/me/My File.html",
                "file:///Users/me/My%20File.html",
            ),
            (
                "file:///Users/me/中文.html",
                "file:///Users/me/%E4%B8%AD%E6%96%87.html",
            ),
            // 已经编码的不再动(否则 %20 会变成 %2520)。
            (
                "file:///Users/me/My%20File.html",
                "file:///Users/me/My%20File.html",
            ),
            ("https://a.com/%E4%B8%AD", "https://a.com/%E4%B8%AD"),
            // 中文域名变成 punycode,而不是被百分号编码。
            ("https://中文.com/", "https://xn--fiq228c.com/"),
            // 标准解析器的规范化:空路径补 `/`(WebKit 视为同一地址)。
            ("https://a.com", "https://a.com/"),
            ("HTTPS://A.COM/X", "https://a.com/X"),
            ("about:blank", "about:blank"),
        ] {
            assert_eq!(encode_for_wry(input), expected, "{input:?}");
        }
    }

    #[test]
    fn encoding_is_idempotent() {
        for input in [
            "https://a.com/x y",
            "file:///Users/me/中文 a.html",
            "https://a.com/p?q=a b#frag 1",
            "https://中文.com/路径",
            "https://a.com/%20 %",
            "about:blank",
            "https://a.com",
        ] {
            let once = encode_for_wry(input);
            assert_eq!(encode_for_wry(&once), once, "{input:?}");
        }
    }

    /// 解析不了的输入原样返回(调用方已经用 `allows_browse_navigation` 拦过一道,走到这里的都能解析;
    /// 这里不 panic、不吞掉即可)。
    #[test]
    fn unparsable_input_is_returned_unchanged() {
        for input in ["", "not a url", "//a.com/x", "http://"] {
            assert_eq!(encode_for_wry(input), input, "{input:?}");
        }
    }

    /// 编码结果必须仍然通过导航策略:编码不能把一个被拒的 URL 变成被放行的,反过来也不能。
    #[test]
    fn encoding_never_changes_the_navigation_verdict() {
        for input in [
            "https://a.com/x y",
            "file:///Users/me/中文.html",
            "file://evil/share/中文.html",
            "javascript:alert(1)",
            "about:blank",
            "https://a.com/\n",
        ] {
            assert_eq!(
                allows_browse_navigation(&encode_for_wry(input)),
                allows_browse_navigation(input),
                "{input:?}"
            );
        }
    }
}
