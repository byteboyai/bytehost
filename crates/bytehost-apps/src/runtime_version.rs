//! 运行时的版本号与版本要求(纯逻辑,默认 feature 可用)。
//!
//! 放在 crate 顶层而不是 `runtime/`:manifest 的校验在默认 feature 下就要用它,
//! 而 `runtime/` 属 `server` feature。
//!
//! 语法**固定且严格**(见 A6e 计划 Global Constraints):逗号分隔的比较子,
//! 每个是 `运算符 + X[.Y[.Z]]`,运算符只能是 `>=`、`>`、`<=`、`<`、`=`。
//! 没有运算符的裸版本号、空串、未知运算符一律拒绝(不猜)。

/// 三段版本号。缺省的 minor/patch 按 0 处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Triple(pub u64, pub u64, pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
    Eq,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Ge => ">=",
            Op::Gt => ">",
            Op::Le => "<=",
            Op::Lt => "<",
            Op::Eq => "=",
        }
    }
}

/// `=` 是**前缀匹配**:`=24.21` 匹配任意 `24.21.x`,`=24` 匹配任意 `24.x.x`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sub {
    op: Op,
    /// 请求里给出的段数(1–3),决定前缀匹配到第几段。
    parts: u8,
    v: Triple,
}

impl Sub {
    fn matches(&self, probe: Triple) -> bool {
        match self.op {
            Op::Eq => match self.parts {
                1 => probe.0 == self.v.0,
                2 => probe.0 == self.v.0 && probe.1 == self.v.1,
                _ => probe == self.v,
            },
            Op::Ge => probe >= self.v,
            Op::Gt => probe > self.v,
            Op::Le => probe <= self.v,
            Op::Lt => probe < self.v,
        }
    }

    fn display(&self) -> String {
        let v = match self.parts {
            1 => format!("{}", self.v.0),
            2 => format!("{}.{}", self.v.0, self.v.1),
            _ => format!("{}.{}.{}", self.v.0, self.v.1, self.v.2),
        };
        format!("{}{v}", self.op.as_str())
    }
}

/// 一个已解析的版本要求(一个或多个逗号分隔的比较子,全部满足才算匹配)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionReq {
    subs: Vec<Sub>,
}

impl VersionReq {
    /// 解析;错误文案给人看,含出错的片段。
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut subs = Vec::new();
        let mut any = false;
        for piece in text.split(',') {
            let piece = piece.trim();
            if piece.is_empty() {
                return Err(format!("版本要求里有一个空片段: {text:?}"));
            }
            any = true;
            subs.push(parse_sub(piece)?);
        }
        if !any {
            return Err(format!("版本要求不能为空: {text:?}"));
        }
        Ok(Self { subs })
    }

    /// 全部比较子都满足才算匹配(逗号是"且")。
    pub fn matches(&self, v: Triple) -> bool {
        self.subs.iter().all(|s| s.matches(v))
    }
}

impl std::fmt::Display for VersionReq {
    /// 规范化回显:`>=3.12, <4`。回显能被 [`VersionReq::parse`] 解析回等价的自身。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let joined = self
            .subs
            .iter()
            .map(|s| s.display())
            .collect::<Vec<_>>()
            .join(", ");
        f.write_str(&joined)
    }
}

fn parse_sub(piece: &str) -> Result<Sub, String> {
    let (op, rest) = if let Some(rest) = piece.strip_prefix(">=") {
        (Op::Ge, rest)
    } else if let Some(rest) = piece.strip_prefix("<=") {
        (Op::Le, rest)
    } else if let Some(rest) = piece.strip_prefix('>') {
        (Op::Gt, rest)
    } else if let Some(rest) = piece.strip_prefix('<') {
        (Op::Lt, rest)
    } else if let Some(rest) = piece.strip_prefix('=') {
        (Op::Eq, rest)
    } else {
        return Err(format!(
            "版本要求片段 {piece:?} 缺少运算符(只支持 >=、>、<=、<、=)"
        ));
    };
    let rest = rest.trim();
    if rest.is_empty() {
        return Err(format!("版本要求片段 {piece:?} 的运算符后面缺版本号"));
    }
    let parts: Vec<&str> = rest.split('.').collect();
    if parts.is_empty() || parts.len() > 3 {
        return Err(format!(
            "版本号 {rest:?} 必须是 1–3 段数字(如 3.12 或 3.13.16)"
        ));
    }
    let mut nums = [0u64; 3];
    for (i, part) in parts.iter().enumerate() {
        let n: u64 = part
            .parse()
            .map_err(|_| format!("版本号 {rest:?} 里的 {part:?} 不是有效数字"))?;
        nums[i] = n;
    }
    Ok(Sub {
        op,
        parts: parts.len() as u8,
        v: Triple(nums[0], nums[1], nums[2]),
    })
}

/// 从 `node --version`("v24.21.0")、`python3 --version`("Python 3.13.16")等输出里
/// 取前三段数字;`rc1`/`+local` 之类的后缀忽略。完全解析不出 → `None`。
///
/// 做法:扫描第一个"以数字开头的 token",取其中前三段数字(遇到非数字截止),至少 1 段才算。
pub fn parse_version_output(output: &str) -> Option<Triple> {
    let cleaned = strip_escapes(output);
    let bytes = cleaned.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        // 数字前紧挨着 `.` 的 token 是非法的(如 `v.1.2`):不是版本号的起点。
        if i > 0 && bytes[i - 1] == b'.' {
            return None;
        }
        // 找到一个数字开头的 token,开始解析最多三段。
        let start = i;
        let mut nums = [0u64; 3];
        let mut parts = 0usize;
        let mut cur: Option<u64> = None;
        while i < bytes.len() {
            let b = bytes[i];
            if b.is_ascii_digit() {
                let d = (b - b'0') as u64;
                cur = Some(cur.unwrap_or(0).saturating_mul(10).saturating_add(d));
                i += 1;
            } else if b == b'.' && parts < 2 && cur.is_some() {
                nums[parts] = cur.unwrap();
                parts += 1;
                cur = None;
                i += 1;
                // 需要下一个字符仍是数字,否则 `.` 后面的段无效(如 `v.1.2`)
                if i >= bytes.len() || !bytes[i].is_ascii_digit() {
                    return None;
                }
            } else {
                break;
            }
        }
        if let Some(n) = cur {
            nums[parts] = n;
            parts += 1;
        }
        if parts == 0 {
            return None;
        }
        let _ = start;
        return Some(Triple(nums[0], nums[1], nums[2]));
    }
    None
}

/// 剥掉 ANSI 转义序列(CSI `ESC [ … 终结符` 与 OSC `ESC ] … BEL/ST`)。
/// 版本输出正常不含转义,但剥掉能让 `parse_version_output` 不被转义里的数字骗到。
fn strip_escapes(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for e in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&e) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(e) = chars.next() {
                    if e == '\u{7}' {
                        break;
                    }
                    if e == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(a: u64, b: u64, c: u64) -> Triple {
        Triple(a, b, c)
    }
    fn req(s: &str) -> VersionReq {
        VersionReq::parse(s).unwrap()
    }

    #[test]
    fn comparison_is_numeric_not_textual() {
        assert!(req(">=9").matches(t(10, 0, 0)), "10 不能因字符串比较小于 9");
        assert!(req(">=3.12").matches(t(3, 13, 16)));
        assert!(!req(">=3.12").matches(t(3, 9, 0)));
        assert!(req("<4").matches(t(3, 13, 16)));
        assert!(!req("<4").matches(t(4, 0, 0)));
        assert!(req(">=24, <25").matches(t(24, 21, 0)));
        assert!(!req(">=24, <25").matches(t(25, 0, 0)));
        assert!(req(">3.12").matches(t(3, 12, 1)));
        assert!(!req(">3.12").matches(t(3, 12, 0)));
        assert!(req("<=3.12").matches(t(3, 12, 0)));
    }

    #[test]
    fn an_equals_requirement_is_a_prefix_match_on_the_parts_given() {
        assert!(req("=24.21").matches(t(24, 21, 0)));
        assert!(req("=24.21").matches(t(24, 21, 7)));
        assert!(!req("=24.21").matches(t(24, 22, 0)));
        assert!(req("=24").matches(t(24, 5, 5)));
        assert!(req("=24.21.0").matches(t(24, 21, 0)));
        assert!(!req("=24.21.0").matches(t(24, 21, 1)));
    }

    #[test]
    fn malformed_requirements_are_refused_with_the_offending_piece() {
        for bad in [
            "",
            "  ",
            "3.12",
            ">=",
            ">= ",
            ">=3..1",
            ">=3.12,",
            ",>=3",
            "~=3.12",
            "^3",
            ">=a.b",
            ">=1.2.3.4",
            ">=99999999999999999999",
            ">=3.12 <4",
        ] {
            let e = VersionReq::parse(bad).unwrap_err();
            assert!(!e.is_empty(), "{bad:?}");
        }
    }

    #[test]
    fn requirements_display_in_a_normalized_form_that_parses_back() {
        let r = req("  >=3.12 ,<4  ");
        assert_eq!(r.to_string(), ">=3.12, <4");
        assert_eq!(VersionReq::parse(&r.to_string()).unwrap(), r);
    }

    #[test]
    fn version_output_takes_the_first_three_numeric_parts_and_ignores_suffixes() {
        assert_eq!(parse_version_output("v24.21.0\n"), Some(t(24, 21, 0)));
        assert_eq!(parse_version_output("Python 3.13.16"), Some(t(3, 13, 16)));
        assert_eq!(parse_version_output("Python 3.14.0rc1"), Some(t(3, 14, 0)));
        assert_eq!(parse_version_output("Python 3.13.1+"), Some(t(3, 13, 1)));
        assert_eq!(parse_version_output("v22.1"), Some(t(22, 1, 0)));
        for bad in ["", "Python", "garbage \u{1b}[31m", "v.1.2"] {
            assert_eq!(parse_version_output(bad), None, "{bad:?}");
        }
    }
}
