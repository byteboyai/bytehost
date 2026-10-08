//! 压缩包来源的安全解压(feature `server`)。
//!
//! 输入**不可信**(可能来自网络),所以解压**必须在进程内逐条目校验**,绝不把路径交给系统 `tar`/`unzip`——
//! 那些工具面对 zip-slip(`../../etc/x`)、绝对路径、符号链接、压缩炸弹时行为不受本程序控制。
//!
//! 规则(见 `docs/superpowers/plans/2026-10-08-bytehost-a6h-archive-and-url-sources.md`):
//! - 条目只允许普通文件与目录;符号链接、硬链接、设备、FIFO 一律拒绝(与 `copy_tree`/`digest_tree` 同口径)。
//! - 拒绝绝对路径、含 `..`/空段/`.` 的路径、反斜杠、非 UTF-8 名字、NUL。
//! - 同一规范化路径出现两次(含大小写折叠后相同)拒绝。
//! - 先遍历全部条目元数据做校验(路径、类型、重复、条目数、声明的未压缩总量),再逐个写出;
//!   写出时用**实际读到的字节数**累计并随时对 `max_file`/`max_total`/压缩比设防(不信任头部声明的大小)。
//! - 失败时清掉已写出的内容,不留残骸。
//! - 解出后权限统一 `0o644`(目录 `0o755`),**不继承归档里的 setuid/可执行位**。

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

/// 归档类型(按扩展名判断;其它扩展名不支持)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    TarGz,
}

impl ArchiveKind {
    /// 从路径扩展名判断归档类型(大小写不敏感)。`.zip` → `Zip`;`.tar.gz`/`.tgz` → `TarGz`;其余 `None`。
    pub fn from_path(path: &Path) -> Option<Self> {
        let name = path.file_name()?.to_str()?;
        let lower = name.to_ascii_lowercase();
        if lower.ends_with(".zip") {
            Some(Self::Zip)
        } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
            Some(Self::TarGz)
        } else {
            None
        }
    }
}

/// 解压硬限制。默认值给的是生产口径;测试用小的自定义值,不必真造 200 MiB。
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// 压缩包文件本身 ≤ 此值(在解压前按文件大小检查)。
    pub max_archive: u64,
    /// 解压后总量 ≤ 此值。
    pub max_total: u64,
    /// 条目数 ≤ 此值。
    pub max_entries: usize,
    /// 单文件 ≤ 此值。
    pub max_file: u64,
    /// 路径深度(段数)≤ 此值。
    pub max_depth: usize,
    /// 压缩比上限:超过 `max_ratio:1` 且解压量 > `ratio_floor` 视为压缩炸弹。
    pub max_ratio: u64,
    /// 触发压缩比检查的最小解压量。
    pub ratio_floor: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_archive: 200 * 1024 * 1024,
            max_total: 1024 * 1024 * 1024,
            max_entries: 20_000,
            max_file: 512 * 1024 * 1024,
            max_depth: 32,
            max_ratio: 100,
            ratio_floor: 64 * 1024 * 1024,
        }
    }
}

/// 解压失败的原因。
#[derive(Debug)]
pub enum ArchiveError {
    /// 扩展名不支持(理论上 `ArchiveKind::from_path` 已挡住,防御性保留)。
    Unsupported(String),
    /// 超过某条限制,`&str` 指出是哪一条:archive/entries/total/file/depth。
    TooLarge(&'static str),
    /// 条目不安全(路径穿越、绝对路径、链接、非 UTF-8、NUL 等),`why` 说明原因。
    UnsafeEntry { name: String, why: &'static str },
    /// 规范化路径重复。
    Duplicate(String),
    /// 压缩炸弹。
    Bomb,
    /// 根上没有 `manifest.toml`(剥层后仍没有)。
    NoManifest,
    /// I/O 错误。
    Io(io::Error),
    /// 归档损坏(无法解析、数据截断、CRC 不符等)。
    Corrupt(String),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(s) => write!(f, "不支持的压缩包类型:{s}"),
            Self::TooLarge(what) => write!(f, "压缩包超过限制:{what}"),
            Self::UnsafeEntry { name, why } => {
                write!(f, "压缩包里有不安全的条目 {name:?}:{why}")
            }
            Self::Duplicate(name) => write!(f, "压缩包里同一路径出现多次:{name:?}"),
            Self::Bomb => write!(f, "压缩包疑似压缩炸弹,已拒绝"),
            Self::NoManifest => write!(f, "压缩包根目录没有 manifest.toml"),
            Self::Io(e) => write!(f, "解压出错:{e}"),
            Self::Corrupt(s) => write!(f, "压缩包已损坏:{s}"),
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<io::Error> for ArchiveError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// 解压结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractReport {
    /// 写出的普通文件数。
    pub files: usize,
    /// 写出的总字节数。
    pub bytes: u64,
    /// 若剥掉了唯一顶层目录,这里记它的名字。
    pub stripped_top_dir: Option<String>,
}

/// 单个条目的类型(仅普通文件与目录;其它在校验阶段就拒绝)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Dir,
    File,
}

/// 校验通过的一个条目:`rel` 是最终相对路径(`None` 表示剥层后落在根上、跳过),
/// `kind` 决定写盘动作,`order` 与归档里的顺序一致。
struct PlannedEntry {
    raw: String,
    rel: Option<PathBuf>,
    kind: EntryKind,
}

fn corrupt(e: impl fmt::Display) -> ArchiveError {
    ArchiveError::Corrupt(e.to_string())
}

/// 校验并规范化条目名 → 相对路径的各段(用 `/` 分隔,已确认无 traversal/绝对/反斜杠)。
fn safe_segments(name: &str, max_depth: usize) -> Result<Vec<String>, ArchiveError> {
    let unsafe_entry = |why: &'static str| ArchiveError::UnsafeEntry {
        name: name.to_string(),
        why,
    };
    if name.contains('\0') {
        return Err(unsafe_entry("含 NUL 字节"));
    }
    if name.contains('\\') {
        return Err(unsafe_entry("含反斜杠(可能是 Windows 路径穿越)"));
    }
    let trimmed = name.trim_end_matches('/');
    if trimmed.is_empty() {
        return Err(unsafe_entry("空路径"));
    }
    if trimmed.starts_with('/') {
        return Err(unsafe_entry("绝对路径"));
    }
    let mut segs = Vec::new();
    for seg in trimmed.split('/') {
        if seg.is_empty() {
            return Err(unsafe_entry("含空段(如 `a//b`)"));
        }
        if seg == "." {
            return Err(unsafe_entry("含 `.` 段"));
        }
        if seg == ".." {
            return Err(unsafe_entry("含 `..` 段(路径穿越)"));
        }
        if seg.len() > 255 {
            return Err(unsafe_entry("单段名字过长"));
        }
        segs.push(seg.to_string());
    }
    if segs.len() > max_depth {
        return Err(ArchiveError::TooLarge("depth"));
    }
    Ok(segs)
}

/// 路径的规范化比较键:大小写折叠(macOS 默认大小写不敏感,避免 `Web/a` 与 `web/a` 撞车)。
fn fold(segs: &[String]) -> String {
    segs.iter()
        .map(|s| s.to_lowercase())
        .collect::<Vec<_>>()
        .join("/")
}

/// 收集到的原始条目(名字 + 类型 + 声明的未压缩大小)。
#[derive(Debug)]
struct RawEntry {
    name: String,
    kind: EntryKind,
    declared: u64,
}

/// 校验原始条目列表,做全部静态检查,返回对齐的 `PlannedEntry` 与是否剥层。
fn plan_entries(
    raws: &[RawEntry],
    limits: &Limits,
) -> Result<(Vec<PlannedEntry>, Option<String>), ArchiveError> {
    if raws.len() > limits.max_entries {
        return Err(ArchiveError::TooLarge("entries"));
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut segs_list: Vec<Vec<String>> = Vec::with_capacity(raws.len());
    let mut has_manifest_at_root = false;
    let mut declared_total = 0u64;
    for raw in raws {
        let segs = safe_segments(&raw.name, limits.max_depth)?;
        let key = fold(&segs);
        if !seen.insert(key) {
            return Err(ArchiveError::Duplicate(raw.name.clone()));
        }
        // 声明的大小可以伪造,真正的防线是写出时的实际字节累计;这里只做一次廉价的提前拦截。
        declared_total = declared_total.saturating_add(raw.declared);
        if declared_total > limits.max_total {
            return Err(ArchiveError::TooLarge("total"));
        }
        if segs.len() == 1 && segs[0] == "manifest.toml" && raw.kind == EntryKind::File {
            has_manifest_at_root = true;
        }
        segs_list.push(segs);
    }

    // 是否剥掉唯一顶层目录:根上没有 manifest.toml 且所有条目共享同一顶层段、且至少一个文件在其下。
    let stripped: Option<String> = if has_manifest_at_root {
        None
    } else {
        shared_top(&segs_list)
    };

    let mut planned = Vec::with_capacity(raws.len());
    let mut root_has_manifest_after = false;
    for (raw, segs) in raws.iter().zip(&segs_list) {
        let rel = match &stripped {
            Some(_) => {
                if segs.len() < 2 {
                    None // 顶层目录自身(或根上的散件),剥层后落在根,跳过。
                } else {
                    Some(join_path(&segs[1..]))
                }
            }
            None => Some(join_path(segs)),
        };
        if let Some(p) = &rel
            && p == Path::new("manifest.toml")
            && raw.kind == EntryKind::File
        {
            root_has_manifest_after = true;
        }
        planned.push(PlannedEntry {
            raw: raw.name.clone(),
            rel,
            kind: raw.kind,
        });
    }
    if !root_has_manifest_after {
        return Err(ArchiveError::NoManifest);
    }
    Ok((planned, stripped))
}

fn join_path(segs: &[String]) -> PathBuf {
    segs.iter().fold(PathBuf::new(), |p, s| p.join(s))
}

/// 若所有条目都在同一个顶层段之下、且至少有一个是文件(不是只有那个目录),返回该顶层段。
fn shared_top(segs_list: &[Vec<String>]) -> Option<String> {
    let mut top: Option<&str> = None;
    let mut has_file = false;
    for segs in segs_list {
        let first = segs.first()?.as_str();
        match top {
            None => top = Some(first),
            Some(t) if t == first => {}
            Some(_) => return None,
        }
        if segs.len() >= 2 {
            has_file = true;
        }
    }
    if has_file {
        top.map(|s| s.to_string())
    } else {
        None
    }
}

/// 解压归档到 `into`(必须不存在或为空目录)。失败时把已写出的内容清掉。
pub fn extract(
    archive: &Path,
    kind: ArchiveKind,
    into: &Path,
    limits: &Limits,
) -> Result<ExtractReport, ArchiveError> {
    let meta = fs::metadata(archive)?;
    if meta.len() > limits.max_archive {
        return Err(ArchiveError::TooLarge("archive"));
    }
    if into.exists() {
        let mut rd = fs::read_dir(into)?;
        if rd.next().is_some() {
            return Err(ArchiveError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("目标目录非空: {}", into.display()),
            )));
        }
    } else {
        fs::create_dir_all(into)?;
    }

    let result = match kind {
        ArchiveKind::Zip => extract_zip(archive, into, limits),
        ArchiveKind::TarGz => extract_targz(archive, into, limits),
    };
    match result {
        Ok(report) => Ok(report),
        Err(e) => {
            let _ = fs::remove_dir_all(into);
            let _ = fs::create_dir_all(into);
            Err(e)
        }
    }
}

/// 压缩包文件字节数,用于压缩比判定。
fn compressed_bytes(archive: &Path) -> u64 {
    fs::metadata(archive).map(|m| m.len()).unwrap_or(0)
}

/// 写出用累加器:实际字节累计 + 单文件/总量/压缩比设防。
struct Sink<'a> {
    into: &'a Path,
    limits: &'a Limits,
    compressed: u64,
    total: u64,
    files: usize,
}

impl Sink<'_> {
    fn ensure_parent(&self, dest: &Path) -> Result<(), ArchiveError> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(())
    }

    fn write_file<R: Read>(&mut self, reader: &mut R, dest: &Path) -> Result<(), ArchiveError> {
        self.ensure_parent(dest)?;
        // 写之前再确认目标仍在 `into` 之下(canonicalize 比较)。
        let canon_into = fs::canonicalize(self.into)?;
        let canon_parent = fs::canonicalize(dest.parent().unwrap_or(self.into))?;
        if !canon_parent.starts_with(&canon_into) {
            return Err(ArchiveError::UnsafeEntry {
                name: dest.to_string_lossy().into_owned(),
                why: "越出目标目录",
            });
        }
        let mut out = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)
            .map_err(|e| {
                if e.kind() == io::ErrorKind::AlreadyExists {
                    ArchiveError::Duplicate(dest.to_string_lossy().into_owned())
                } else {
                    ArchiveError::Io(e)
                }
            })?;
        let mut file_bytes = 0u64;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = reader
                .read(&mut buf)
                .map_err(|e| ArchiveError::Corrupt(e.to_string()))?;
            if n == 0 {
                break;
            }
            file_bytes = file_bytes.saturating_add(n as u64);
            if file_bytes > self.limits.max_file {
                return Err(ArchiveError::TooLarge("file"));
            }
            self.total = self.total.saturating_add(n as u64);
            if self.total > self.limits.max_total {
                return Err(ArchiveError::TooLarge("total"));
            }
            if self.total >= self.limits.ratio_floor
                && self.total > self.compressed.max(1).saturating_mul(self.limits.max_ratio)
            {
                return Err(ArchiveError::Bomb);
            }
            out.write_all(&buf[..n])?;
        }
        drop(out);
        set_file_mode(dest)?;
        self.files += 1;
        Ok(())
    }

    fn make_dir(&self, dest: &Path) -> Result<(), ArchiveError> {
        fs::create_dir_all(dest)?;
        set_dir_mode(dest)?;
        Ok(())
    }
}

fn extract_zip(
    archive: &Path,
    into: &Path,
    limits: &Limits,
) -> Result<ExtractReport, ArchiveError> {
    // 第一遍:收集元数据并做全部静态校验。
    let raws = {
        let file = fs::File::open(archive)?;
        let mut zip = zip::ZipArchive::new(file).map_err(corrupt)?;
        let n = zip.len();
        if n > limits.max_entries {
            return Err(ArchiveError::TooLarge("entries"));
        }
        let mut raws = Vec::with_capacity(n);
        for i in 0..n {
            let entry = zip.by_index(i).map_err(corrupt)?;
            let name = entry.name().to_string();
            if entry.is_symlink() {
                return Err(ArchiveError::UnsafeEntry {
                    name,
                    why: "符号链接",
                });
            }
            let (kind, declared) = if entry.is_dir() {
                (EntryKind::Dir, 0)
            } else {
                if let Some(mode) = entry.unix_mode() {
                    let ifmt = mode & 0o170000;
                    if ifmt != 0 && ifmt != 0o100000 && ifmt != 0o040000 {
                        return Err(ArchiveError::UnsafeEntry {
                            name,
                            why: "非常规文件类型(链接/设备/FIFO)",
                        });
                    }
                }
                (EntryKind::File, entry.size())
            };
            raws.push(RawEntry {
                name,
                kind,
                declared,
            });
        }
        raws
    };
    let (planned, stripped) = plan_entries(&raws, limits)?;

    // 第二遍:按同序写出。
    let file = fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).map_err(corrupt)?;
    let mut sink = Sink {
        into,
        limits,
        compressed: compressed_bytes(archive),
        total: 0,
        files: 0,
    };
    for (i, p) in planned.iter().enumerate() {
        let Some(rel) = &p.rel else { continue };
        let dest = into.join(rel);
        let mut entry = zip.by_index(i).map_err(corrupt)?;
        match p.kind {
            EntryKind::Dir => sink.make_dir(&dest)?,
            EntryKind::File => sink.write_file(&mut entry, &dest)?,
        }
        let _ = &p.raw;
    }
    Ok(ExtractReport {
        files: sink.files,
        bytes: sink.total,
        stripped_top_dir: stripped,
    })
}

/// tar 的第一遍:只读条目头,**每读到一个头就立刻按限制拒绝**(条目数、单文件声明大小、累计声明大小)。
/// 扫描 tar 必须把前一个条目的数据流"读过去"才能到下一个头(gzip 流没法 seek),所以不能等扫完再检查:
/// 压缩比极高的炸弹会让这一遍白白解压几十 GB。声明大小可以说谎,但说谎只会让扫描**更短**
/// (按声明大小跳过),不会更长。
fn scan_tar_entries<R: Read>(reader: R, limits: &Limits) -> Result<Vec<RawEntry>, ArchiveError> {
    let mut tar = tar::Archive::new(reader);
    let mut raws = Vec::new();
    let mut declared_total = 0u64;
    let entries = tar
        .entries()
        .map_err(|e| ArchiveError::Corrupt(e.to_string()))?;
    for entry in entries {
        let entry = entry.map_err(|e| ArchiveError::Corrupt(e.to_string()))?;
        let header = entry.header();
        let ty = header.entry_type();
        let raw = entry
            .path()
            .map_err(|e| ArchiveError::Corrupt(e.to_string()))?;
        let name = match raw.to_str() {
            Some(s) => s.to_string(),
            None => {
                return Err(ArchiveError::UnsafeEntry {
                    name: raw.to_string_lossy().into_owned(),
                    why: "非 UTF-8 文件名",
                });
            }
        };
        if ty.is_symlink() || ty.is_hard_link() {
            return Err(ArchiveError::UnsafeEntry {
                name,
                why: "链接条目",
            });
        }
        let (kind, declared) = if ty.is_dir() {
            (EntryKind::Dir, 0)
        } else if ty.is_file() {
            (EntryKind::File, header.size().unwrap_or(0))
        } else {
            return Err(ArchiveError::UnsafeEntry {
                name,
                why: "非常规文件类型(设备/FIFO/其它)",
            });
        };
        if raws.len() >= limits.max_entries {
            return Err(ArchiveError::TooLarge("entries"));
        }
        if declared > limits.max_file {
            return Err(ArchiveError::TooLarge("file"));
        }
        declared_total = declared_total.saturating_add(declared);
        if declared_total > limits.max_total {
            return Err(ArchiveError::TooLarge("total"));
        }
        raws.push(RawEntry {
            name,
            kind,
            declared,
        });
    }
    Ok(raws)
}

fn extract_targz(
    archive: &Path,
    into: &Path,
    limits: &Limits,
) -> Result<ExtractReport, ArchiveError> {
    // 第一遍:收集元数据并做全部静态校验(逐头提前拒绝,见 `scan_tar_entries`)。
    let raws = {
        let file = fs::File::open(archive)?;
        scan_tar_entries(flate2::read::GzDecoder::new(file), limits)?
    };
    let (planned, stripped) = plan_entries(&raws, limits)?;

    let file = fs::File::open(archive)?;
    let gz = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(gz);
    let mut sink = Sink {
        into,
        limits,
        compressed: compressed_bytes(archive),
        total: 0,
        files: 0,
    };
    let entries = tar
        .entries()
        .map_err(|e| ArchiveError::Corrupt(e.to_string()))?;
    for (i, entry) in entries.enumerate() {
        let mut entry = entry.map_err(|e| ArchiveError::Corrupt(e.to_string()))?;
        let Some(p) = planned.get(i) else { break };
        let Some(rel) = &p.rel else { continue };
        let dest = into.join(rel);
        match p.kind {
            EntryKind::Dir => sink.make_dir(&dest)?,
            EntryKind::File => sink.write_file(&mut entry, &dest)?,
        }
        let _ = &p.raw;
    }
    Ok(ExtractReport {
        files: sink.files,
        bytes: sink.total,
        stripped_top_dir: stripped,
    })
}

fn set_file_mode(path: &Path) -> Result<(), ArchiveError> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o644))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn set_dir_mode(path: &Path) -> Result<(), ArchiveError> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// 造一个 zip:每个条目给 (name, is_dir, content, unix_mode)。
    fn make_zip(path: &Path, entries: &[(String, bool, Vec<u8>, Option<u32>)]) {
        let file = fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, is_dir, content, mode) in entries {
            let opts = match mode {
                Some(m) => opts.unix_permissions(*m),
                None => opts,
            };
            if *is_dir {
                zw.add_directory(name.clone(), opts).unwrap();
            } else {
                zw.start_file(name.clone(), opts).unwrap();
                zw.write_all(content).unwrap();
            }
        }
        zw.finish().unwrap();
    }

    /// 造一个 zip,其中一个条目是符号链接(指向 `target`)。
    fn make_zip_with_symlink(path: &Path, link_name: &str, target: &str) {
        let file = fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("manifest.toml", opts).unwrap();
        zw.write_all(b"x").unwrap();
        zw.add_symlink(link_name, target, opts).unwrap();
        zw.finish().unwrap();
    }

    /// 造一个 tar.gz。
    fn make_targz(path: &Path, entries: &[(String, Vec<u8>, EntryKind2)]) {
        let file = fs::File::create(path).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tb = tar::Builder::new(gz);
        for (name, content, kind) in entries {
            match kind {
                EntryKind2::Dir => {
                    let mut h = tar::Header::new_gnu();
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_size(0);
                    h.set_mode(0o755);
                    h.set_cksum();
                    tb.append_data(&mut h, name, std::io::empty()).unwrap();
                }
                EntryKind2::File => {
                    let mut h = tar::Header::new_gnu();
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_size(content.len() as u64);
                    h.set_mode(0o644);
                    h.set_cksum();
                    tb.append_data(&mut h, name, content.as_slice()).unwrap();
                }
            }
        }
        tb.into_inner().unwrap().finish().unwrap();
    }

    #[derive(Clone)]
    enum EntryKind2 {
        Dir,
        File,
    }

    fn limits_small() -> Limits {
        Limits {
            max_archive: 1 << 20,
            max_total: 1 << 20,
            max_entries: 100,
            max_file: 1 << 20,
            max_depth: 32,
            max_ratio: 100,
            ratio_floor: 1 << 20,
        }
    }

    #[test]
    fn kind_from_path_is_case_insensitive() {
        assert_eq!(
            ArchiveKind::from_path(Path::new("a.zip")),
            Some(ArchiveKind::Zip)
        );
        assert_eq!(
            ArchiveKind::from_path(Path::new("A.ZIP")),
            Some(ArchiveKind::Zip)
        );
        assert_eq!(
            ArchiveKind::from_path(Path::new("a.tar.gz")),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(
            ArchiveKind::from_path(Path::new("a.TGZ")),
            Some(ArchiveKind::TarGz)
        );
        assert_eq!(ArchiveKind::from_path(Path::new("a.tar.xz")), None);
        assert_eq!(ArchiveKind::from_path(Path::new("a.txt")), None);
    }

    #[test]
    fn zip_and_targz_extract_the_same_tree() {
        let d = tmp();
        let zip = d.path().join("a.zip");
        let tgz = d.path().join("a.tar.gz");
        make_zip(
            &zip,
            &[
                ("manifest.toml".into(), false, b"id = \"x\"".to_vec(), None),
                ("web".into(), true, vec![], None),
                (
                    "web/index.html".into(),
                    false,
                    b"<h1>hi</h1>".to_vec(),
                    None,
                ),
            ],
        );
        make_targz(
            &tgz,
            &[
                (
                    "manifest.toml".into(),
                    b"id = \"x\"".to_vec(),
                    EntryKind2::File,
                ),
                ("web/".into(), vec![], EntryKind2::Dir),
                (
                    "web/index.html".into(),
                    b"<h1>hi</h1>".to_vec(),
                    EntryKind2::File,
                ),
            ],
        );
        for (path, kind) in [(&zip, ArchiveKind::Zip), (&tgz, ArchiveKind::TarGz)] {
            let into = d.path().join(format!("out-{kind:?}"));
            let report = extract(path, kind, &into, &limits_small()).unwrap();
            assert_eq!(report.files, 2);
            assert_eq!(report.stripped_top_dir, None);
            assert_eq!(
                fs::read_to_string(into.join("manifest.toml")).unwrap(),
                "id = \"x\""
            );
            assert_eq!(
                fs::read_to_string(into.join("web/index.html")).unwrap(),
                "<h1>hi</h1>"
            );
        }
    }

    #[test]
    fn github_style_single_top_dir_is_stripped() {
        let d = tmp();
        let zip = d.path().join("a.zip");
        make_zip(
            &zip,
            &[
                (
                    "app-1.0/manifest.toml".into(),
                    false,
                    b"id=\"x\"".to_vec(),
                    None,
                ),
                ("app-1.0/web/index.html".into(), false, b"hi".to_vec(), None),
            ],
        );
        let into = d.path().join("out");
        let report = extract(&zip, ArchiveKind::Zip, &into, &limits_small()).unwrap();
        assert_eq!(report.stripped_top_dir.as_deref(), Some("app-1.0"));
        assert!(into.join("manifest.toml").exists());
        assert!(into.join("web/index.html").exists());
    }

    #[test]
    fn multiple_top_dirs_without_root_manifest_is_refused() {
        let d = tmp();
        let zip = d.path().join("a.zip");
        make_zip(
            &zip,
            &[
                ("a/x".into(), false, b"1".to_vec(), None),
                ("b/y".into(), false, b"2".to_vec(), None),
            ],
        );
        let into = d.path().join("out");
        assert!(matches!(
            extract(&zip, ArchiveKind::Zip, &into, &limits_small()),
            Err(ArchiveError::NoManifest)
        ));
    }

    #[test]
    fn path_traversal_and_absolute_paths_are_refused() {
        let d = tmp();
        let outer = d.path().join("outer");
        fs::create_dir_all(&outer).unwrap();
        let cases = [
            "../evil",
            "a/../../evil",
            "/abs/evil",
            "C:\\evil",
            "web\\..\\..\\evil",
            "a//b",
            "./a",
        ];
        for c in cases {
            let zip = d.path().join("a.zip");
            make_zip(&zip, &[(c.into(), false, b"x".to_vec(), None)]);
            let into = outer.join("out");
            let err = extract(&zip, ArchiveKind::Zip, &into, &limits_small()).unwrap_err();
            assert!(
                matches!(err, ArchiveError::UnsafeEntry { .. }),
                "{c}: {err:?}"
            );
            // 父目录里除空的 out 之外没有新文件。
            let mut entries: Vec<_> = fs::read_dir(&outer)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            entries.sort();
            assert_eq!(entries, vec!["out".to_string()], "staging 之外有写入: {c}");
            // out 被清空(失败清理)。
            let leftover: Vec<_> = fs::read_dir(&into).unwrap().collect();
            assert!(leftover.is_empty(), "{c}: out 未清空");
        }
    }

    #[test]
    fn nul_in_name_is_refused() {
        let d = tmp();
        let zip = d.path().join("a.zip");
        make_zip(&zip, &[("a\0b".into(), false, b"x".to_vec(), None)]);
        let into = d.path().join("out");
        assert!(matches!(
            extract(&zip, ArchiveKind::Zip, &into, &limits_small()),
            Err(ArchiveError::UnsafeEntry { .. })
        ));
    }

    #[test]
    fn zip_symlink_entry_is_refused() {
        let d = tmp();
        let zip = d.path().join("a.zip");
        make_zip_with_symlink(&zip, "web/leak", "/etc/hosts");
        let into = d.path().join("out");
        let err = extract(&zip, ArchiveKind::Zip, &into, &limits_small()).unwrap_err();
        assert!(matches!(err, ArchiveError::UnsafeEntry { .. }), "{err:?}");
        let leftover: Vec<_> = fs::read_dir(&into).unwrap().collect();
        assert!(leftover.is_empty(), "失败后 out 应清空");
    }

    #[test]
    fn tar_symlink_hardlink_device_fifo_are_refused() {
        let d = tmp();
        for kind in [
            tar::EntryType::Symlink,
            tar::EntryType::Link,
            tar::EntryType::Char,
            tar::EntryType::Fifo,
        ] {
            let tgz = d.path().join("a.tar.gz");
            let file = fs::File::create(&tgz).unwrap();
            let gz = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            let mut tb = tar::Builder::new(gz);
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(kind);
            h.set_size(0);
            h.set_mode(0o777);
            if matches!(kind, tar::EntryType::Symlink | tar::EntryType::Link) {
                h.set_link_name("/etc/hosts").unwrap();
            }
            h.set_cksum();
            tb.append_data(&mut h, "web/leak", std::io::empty())
                .unwrap();
            tb.into_inner().unwrap().finish().unwrap();
            let into = d.path().join(format!("out-{kind:?}"));
            let err = extract(&tgz, ArchiveKind::TarGz, &into, &limits_small()).unwrap_err();
            assert!(
                matches!(err, ArchiveError::UnsafeEntry { .. }),
                "{kind:?}: {err:?}"
            );
        }
    }

    /// 计数读取器:记下上游被读走了多少字节。
    struct Counting<R> {
        inner: R,
        read: std::rc::Rc<std::cell::Cell<u64>>,
    }
    impl<R: Read> Read for Counting<R> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.read.set(self.read.get() + n as u64);
            Ok(n)
        }
    }

    /// tar 炸弹必须在条目头处就被拒绝,而不是把声明的几十 GB 数据读过去之后才拒绝。
    /// 用一个"头声明 100 MiB、后面真有 100 MiB 零"的流,上限设 1 MiB:扫描应当只读走头那几百字节。
    #[test]
    fn a_tar_bomb_is_refused_at_its_header_without_reading_the_body() {
        for (label, limits) in [
            (
                "file",
                Limits {
                    max_file: 1 << 20,
                    ..Limits::default()
                },
            ),
            (
                "total",
                Limits {
                    max_total: 1 << 20,
                    ..Limits::default()
                },
            ),
        ] {
            let size: u64 = 100 << 20;
            let mut h = tar::Header::new_gnu();
            h.set_path("manifest.toml").unwrap();
            h.set_size(size);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            let read = std::rc::Rc::new(std::cell::Cell::new(0u64));
            let stream = Counting {
                inner: io::Cursor::new(h.as_bytes().to_vec())
                    .chain(io::repeat(0).take(size + 1024)),
                read: read.clone(),
            };
            let err = scan_tar_entries(stream, &limits).unwrap_err();
            assert!(matches!(err, ArchiveError::TooLarge(_)), "{label}: {err:?}");
            assert!(
                read.get() < 64 * 1024,
                "{label}: 拒绝前读走了 {} 字节(应只读头)",
                read.get()
            );
        }
    }

    #[test]
    fn a_tar_with_too_many_entries_is_refused_while_scanning_headers() {
        let mut builder = tar::Builder::new(Vec::new());
        for i in 0..10 {
            let mut h = tar::Header::new_gnu();
            h.set_path(format!("f{i}")).unwrap();
            h.set_size(0);
            h.set_entry_type(tar::EntryType::Regular);
            h.set_cksum();
            builder.append(&h, io::empty()).unwrap();
        }
        let bytes = builder.into_inner().unwrap();
        let limits = Limits {
            max_entries: 3,
            ..Limits::default()
        };
        assert!(matches!(
            scan_tar_entries(io::Cursor::new(bytes), &limits),
            Err(ArchiveError::TooLarge("entries"))
        ));
    }

    #[test]
    fn corrupt_archives_are_refused() {
        let d = tmp();
        let zip = d.path().join("a.zip");
        fs::write(&zip, b"not a zip at all").unwrap();
        let into = d.path().join("out");
        assert!(matches!(
            extract(&zip, ArchiveKind::Zip, &into, &limits_small()),
            Err(ArchiveError::Corrupt(_))
        ));
        let tgz = d.path().join("a.tar.gz");
        fs::write(&tgz, b"\x1f\x8b\x08 garbage").unwrap();
        let into2 = d.path().join("out2");
        assert!(matches!(
            extract(&tgz, ArchiveKind::TarGz, &into2, &limits_small()),
            Err(ArchiveError::Corrupt(_))
        ));
    }

    #[test]
    fn too_many_entries_is_refused() {
        let d = tmp();
        let zip = d.path().join("a.zip");
        let entries: Vec<_> = (0..150)
            .map(|i| (format!("f{i}"), false, vec![], None))
            .collect();
        make_zip(&zip, &entries);
        let mut lim = limits_small();
        lim.max_entries = 100;
        let into = d.path().join("out");
        assert!(matches!(
            extract(&zip, ArchiveKind::Zip, &into, &lim),
            Err(ArchiveError::TooLarge("entries"))
        ));
    }

    #[test]
    fn archive_larger_than_limit_is_refused() {
        let d = tmp();
        let zip = d.path().join("a.zip");
        make_zip(
            &zip,
            &[("manifest.toml".into(), false, vec![0u8; 100], None)],
        );
        let mut lim = limits_small();
        lim.max_archive = 10;
        let into = d.path().join("out");
        assert!(matches!(
            extract(&zip, ArchiveKind::Zip, &into, &lim),
            Err(ArchiveError::TooLarge("archive"))
        ));
    }

    #[test]
    fn declared_size_lies_but_actual_bytes_are_capped() {
        // 头部声明 10 字节、实际流 100 MiB;单文件上限设小,必须在超限那一刻中止。
        let d = tmp();
        let zip = d.path().join("a.zip");
        // zip 的 size 由写入的内容真实决定,无法伪造头部;这里直接用大内容 + 小 max_file 验证实际累计。
        let big = vec![0u8; 256 * 1024];
        make_zip(
            &zip,
            &[
                ("manifest.toml".into(), false, b"x".to_vec(), None),
                ("big.bin".into(), false, big, None),
            ],
        );
        let mut lim = limits_small();
        lim.max_file = 64 * 1024;
        let into = d.path().join("out");
        let err = extract(&zip, ArchiveKind::Zip, &into, &lim).unwrap_err();
        assert!(matches!(err, ArchiveError::TooLarge("file")), "{err:?}");
        let leftover: Vec<_> = fs::read_dir(&into).unwrap().collect();
        assert!(leftover.is_empty(), "失败后 out 应清空");
    }

    #[test]
    fn compression_bomb_is_refused() {
        // 高压缩比:多份全零文件,总解压量超过 ratio_floor,且 > max_ratio × 压缩包字节。
        let d = tmp();
        let zip = d.path().join("a.zip");
        let zero = vec![0u8; 512 * 1024];
        let mut entries = vec![("manifest.toml".into(), false, b"x".to_vec(), None)];
        for i in 0..16 {
            entries.push((format!("z{i}.bin"), false, zero.clone(), None));
        }
        make_zip(&zip, &entries);
        let mut lim = limits_small();
        lim.ratio_floor = 64 * 1024;
        lim.max_ratio = 50;
        lim.max_total = 64 * 1024 * 1024;
        lim.max_file = 64 * 1024 * 1024;
        let into = d.path().join("out");
        let err = extract(&zip, ArchiveKind::Zip, &into, &lim).unwrap_err();
        assert!(matches!(err, ArchiveError::Bomb), "{err:?}");
    }

    #[test]
    fn duplicate_paths_are_refused_case_insensitively() {
        // zip/tar 写入器本身会拒绝重复名,所以直接测纯函数 `plan_entries`。
        let raws = vec![
            RawEntry {
                name: "manifest.toml".into(),
                kind: EntryKind::File,
                declared: 1,
            },
            RawEntry {
                name: "manifest.toml".into(),
                kind: EntryKind::File,
                declared: 1,
            },
        ];
        assert!(matches!(
            plan_entries(&raws, &limits_small()),
            Err(ArchiveError::Duplicate(_))
        ));

        let raws = vec![
            RawEntry {
                name: "Web/a".into(),
                kind: EntryKind::File,
                declared: 1,
            },
            RawEntry {
                name: "web/a".into(),
                kind: EntryKind::File,
                declared: 1,
            },
        ];
        assert!(matches!(
            plan_entries(&raws, &limits_small()),
            Err(ArchiveError::Duplicate(_))
        ));
    }

    #[test]
    fn too_deep_path_is_refused() {
        let d = tmp();
        let zip = d.path().join("a.zip");
        let deep = (0..40)
            .map(|i| format!("d{i}"))
            .collect::<Vec<_>>()
            .join("/");
        make_zip(
            &zip,
            &[(format!("{deep}/manifest.toml"), false, b"x".to_vec(), None)],
        );
        let mut lim = limits_small();
        lim.max_depth = 32;
        let into = d.path().join("out");
        assert!(matches!(
            extract(&zip, ArchiveKind::Zip, &into, &lim),
            Err(ArchiveError::TooLarge("depth"))
        ));
    }

    #[test]
    fn extracted_files_have_fixed_permissions_regardless_of_archived_mode() {
        // 归档里给 0o755(可执行),解出后必须是 0o644(不继承归档位)。
        let d = tmp();
        let zip = d.path().join("a.zip");
        make_zip(
            &zip,
            &[("manifest.toml".into(), false, b"x".to_vec(), Some(0o755))],
        );
        let into = d.path().join("out");
        extract(&zip, ArchiveKind::Zip, &into, &limits_small()).unwrap();
        #[cfg(unix)]
        {
            let mode = fs::metadata(into.join("manifest.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o644);
        }
    }

    #[test]
    fn non_empty_target_is_refused() {
        let d = tmp();
        let zip = d.path().join("a.zip");
        make_zip(
            &zip,
            &[("manifest.toml".into(), false, b"x".to_vec(), None)],
        );
        let into = d.path().join("out");
        fs::create_dir_all(&into).unwrap();
        fs::write(into.join("preexisting"), b"x").unwrap();
        assert!(matches!(
            extract(&zip, ArchiveKind::Zip, &into, &limits_small()),
            Err(ArchiveError::Io(_))
        ));
    }
}
