//! 只读、有界、已清洗的应用日志末尾。给 GUI 看,不写 dozerd 日志、不广播事件。
//!
//! 读的是进程型应用的 `logs/app.log`,不够再前补轮转的 `app.log.1`。**从不整文件读入**:
//! 从每个文件的末尾按块向前读,读到够行数或够字节数为止(日志可达数 MiB)。

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

/// 最多返回多少行(服务端会把请求的 `max_lines` 夹到这个范围)。
pub const MAX_LINES: usize = 500;
/// 最多返回多少字节(取末尾)。
pub const MAX_BYTES: usize = 256 * 1024;
/// 单行最多多少字符,超了截断并以 `…` 结尾。
pub const MAX_LINE_CHARS: usize = 4096;

/// 一次日志读取的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tail {
    pub text: String,
    pub truncated: bool,
}

/// 读 `<logs_dir>/app.log`(不够再前补 `app.log.1`)的末尾,取 `max_lines` 与 [`MAX_BYTES`]。
///
/// `max_lines` 会被夹到 `1..=MAX_LINES`。文件不存在 → 空文本、`truncated = false`,不报错。
pub fn read_tail(logs_dir: &Path, max_lines: usize) -> io::Result<Tail> {
    let max_lines = max_lines.clamp(1, MAX_LINES);
    let mut lines: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut truncated = false;

    // 先读当前日志,不够再补轮转的那份(更新的在前)。
    for name in ["app.log", "app.log.1"] {
        if lines.len() >= max_lines || bytes >= MAX_BYTES {
            break;
        }
        let path = logs_dir.join(name);
        let Some((buf, per_file_truncated)) = read_newest_bytes(&path, MAX_BYTES - bytes)? else {
            continue;
        };
        truncated |= per_file_truncated;
        bytes += buf.len();
        let text = String::from_utf8_lossy(&buf);
        let mut file_lines: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        // 一个没有换行结尾的尾部,split 会多出一个空串;若文件以 \n 结尾则最后一段是空,丢掉。
        if file_lines.last().map(|s| s.is_empty()).unwrap_or(false) {
            file_lines.pop();
        }
        // 拼到前面:当前文件的行更新,应排在已有(更旧的 .1)之后……这里按名字顺序 app.log 在前,
        // 所以把两段按时间顺序拼成 [older..., newer...],最后统一取末尾。
        let mut combined = file_lines;
        combined.extend(lines);
        lines = combined;
    }

    // 逐行清洗、控单行长度。
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    for line in lines {
        let clean = sanitize(line.trim_end_matches('\r'));
        out.push(truncate_line(clean));
    }
    if out.len() > max_lines {
        out.drain(0..out.len() - max_lines);
        truncated = true;
    }
    let mut text = out.join("\n");
    if !text.is_empty() {
        text.push('\n');
    }
    Ok(Tail { text, truncated })
}

/// 从 `path` 末尾向前读,最多读 `byte_budget` 字节;文件不存在返回 `None`。
/// 返回 `(末尾原始字节, 是否因上限被截)`。
///
/// 只读末尾有界的一段(`seek` 到 `len - budget`),不整文件读入。
fn read_newest_bytes(path: &Path, byte_budget: usize) -> io::Result<Option<(Vec<u8>, bool)>> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let len = file.metadata()?.len();
    let budget = byte_budget.min(MAX_BYTES) as u64;
    let start = len.saturating_sub(budget);
    let truncated = len > budget;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity(budget as usize);
    file.read_to_end(&mut buf)?;
    Ok(Some((buf, truncated)))
}

/// 剥 ANSI 转义(CSI/OSC)与除 `\n`、`\t` 外的控制字符。手写状态机(不引正则依赖)。
pub fn sanitize(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            match chars.peek() {
                // CSI: `ESC [ ... 终止字节(0x40..=0x7e)`
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC: `ESC ] ... BEL` 或 `ESC ] ... ESC \`
                Some(']') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                // 其他两字符转义:跳过下一个字符。
                Some(_) => {
                    chars.next();
                }
                None => {}
            }
            continue;
        }
        match c {
            '\n' | '\t' => out.push(c),
            // 其余控制字符(含 \r、\0、\u{8})一律剥掉。
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// 单行超过 [`MAX_LINE_CHARS`] 时截断并以 `…` 结尾。
fn truncate_line(line: String) -> String {
    if line.chars().count() <= MAX_LINE_CHARS {
        return line;
    }
    let mut s: String = line.chars().take(MAX_LINE_CHARS).collect();
    s.push('…');
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn write_lines(path: &Path, prefix: &str, n: usize) {
        let mut f = fs::File::create(path).unwrap();
        for i in 0..n {
            writeln!(f, "{prefix}{i}").unwrap();
        }
    }

    #[test]
    fn the_tail_returns_only_the_last_lines_and_marks_truncation() {
        let dir = tempfile::tempdir().unwrap();
        write_lines(&dir.path().join("app.log"), "l", 1000);
        let tail = read_tail(dir.path(), 50).unwrap();
        let lines: Vec<&str> = tail.text.lines().collect();
        assert_eq!(lines.len(), 50);
        assert_eq!(lines[0], "l950");
        assert_eq!(lines[49], "l999");
        assert!(tail.truncated);
    }

    #[test]
    fn a_short_current_log_is_topped_up_from_the_rotated_one() {
        let dir = tempfile::tempdir().unwrap();
        write_lines(&dir.path().join("app.log.1"), "old", 100);
        write_lines(&dir.path().join("app.log"), "new", 3);
        let tail = read_tail(dir.path(), 10).unwrap();
        let lines: Vec<&str> = tail.text.lines().collect();
        assert_eq!(lines.len(), 10);
        // 前 7 行来自 .1 的末尾,后 3 行来自 app.log。
        assert_eq!(lines[0], "old93");
        assert_eq!(lines[6], "old99");
        assert_eq!(lines[7], "new0");
        assert_eq!(lines[9], "new2");
    }

    #[test]
    fn a_missing_log_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let tail = read_tail(dir.path(), 10).unwrap();
        assert_eq!(tail.text, "");
        assert!(!tail.truncated);
    }

    #[test]
    fn max_lines_is_clamped() {
        let dir = tempfile::tempdir().unwrap();
        write_lines(&dir.path().join("app.log"), "l", 1000);
        assert_eq!(read_tail(dir.path(), 0).unwrap().text.lines().count(), 1);
        assert_eq!(
            read_tail(dir.path(), 100000).unwrap().text.lines().count(),
            MAX_LINES
        );
    }

    #[test]
    fn byte_cap_wins_over_line_cap() {
        let dir = tempfile::tempdir().unwrap();
        // 500 行每行约 2 KiB → 远超 256 KiB。
        let line = "x".repeat(2000);
        let mut f = fs::File::create(dir.path().join("app.log")).unwrap();
        for _ in 0..500 {
            writeln!(f, "{line}").unwrap();
        }
        let tail = read_tail(dir.path(), 500).unwrap();
        assert!(tail.text.len() <= MAX_BYTES, "{}", tail.text.len());
        assert!(tail.truncated);
        // 整体是合法 UTF-8(已按有损解码)。
        assert!(std::str::from_utf8(tail.text.as_bytes()).is_ok());
    }

    #[test]
    fn sanitize_strips_ansi_osc_and_control_characters_but_keeps_text() {
        assert_eq!(sanitize("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(sanitize("\u{1b}]0;title\u{7}x"), "x");
        assert_eq!(sanitize("a\u{0}b\u{8}c\r\n"), "abc\n");
        assert_eq!(sanitize("keep\ttab"), "keep\ttab");
        assert_eq!(sanitize("中文保留"), "中文保留");
    }

    #[test]
    fn an_overlong_line_is_cut_and_marked() {
        let dir = tempfile::tempdir().unwrap();
        let line = "y".repeat(5000);
        fs::write(dir.path().join("app.log"), format!("{line}\n")).unwrap();
        let tail = read_tail(dir.path(), 10).unwrap();
        let first = tail.text.lines().next().unwrap();
        assert_eq!(first.chars().count(), MAX_LINE_CHARS + 1);
        assert!(first.ends_with('…'));
    }

    #[test]
    fn invalid_utf8_is_replaced_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = fs::File::create(dir.path().join("app.log")).unwrap();
        f.write_all(b"ok\xff\xfe\n").unwrap();
        let tail = read_tail(dir.path(), 10).unwrap();
        assert!(tail.text.contains("ok"));
        assert!(std::str::from_utf8(tail.text.as_bytes()).is_ok());
    }
}
