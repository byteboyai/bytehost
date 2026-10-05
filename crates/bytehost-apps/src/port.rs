//! gateway 固定端口:首次启动从高端口段随机选一个并持久化(`<root>/gateway.json`),跨重启不变。
//! **端口一变,所有应用的本地存储(origin 含端口)都会丢**——所以坏文件、越界端口一律报错,
//! 绝不"顺手重新选一个";被占用的处理见 `GatewayError::PortInUse`(同样不静默换)。

use std::fs;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// 随机选取的范围:**低于 macOS(49152–65535)与 Linux(32768–60999)的临时端口段**——任何监听端口 0 的程序都从
/// 那里拿端口,dozerd 停着的时候可能占走固定端口;同时避开 3000/8080 之类的常用开发端口。
pub const PORT_MIN: u16 = 20000;
pub const PORT_MAX: u16 = 32767;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GatewaySettings {
    port: u16,
}

/// 在 `PORT_MIN..=PORT_MAX` 里随机选一个。
pub fn pick_port() -> u16 {
    let bytes = *uuid::Uuid::new_v4().as_bytes();
    let n = u16::from_le_bytes([bytes[0], bytes[1]]);
    PORT_MIN + n % (PORT_MAX - PORT_MIN + 1)
}

/// 读 `<root>/gateway.json` 里持久化的端口;文件不存在就选一个、原子写入、返回。
pub fn load_or_choose_port(root: &Path) -> io::Result<u16> {
    let path = root.join("gateway.json");
    match fs::read_to_string(&path) {
        Ok(text) => {
            let settings: GatewaySettings = serde_json::from_str(&text).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("{}: {e}", path.display()),
                )
            })?;
            if settings.port < 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "{}: 端口 {} 不在允许范围(>= 1024)",
                        path.display(),
                        settings.port
                    ),
                ));
            }
            Ok(settings.port)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let port = pick_port();
            fs::create_dir_all(root)?;
            let json = serde_json::to_string_pretty(&GatewaySettings { port })
                .map_err(io::Error::other)?;
            let tmp = path.with_extension("tmp");
            {
                let mut file = fs::File::create(&tmp)?;
                io::Write::write_all(&mut file, json.as_bytes())?;
                file.sync_all()?; // 先落盘再改名,避免断电后留下空文件(空文件会变成永久的 InvalidData)
            }
            fs::rename(&tmp, &path)?;
            Ok(port)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picked_ports_stay_inside_the_dynamic_range() {
        for _ in 0..2000 {
            let p = pick_port();
            assert!((PORT_MIN..=PORT_MAX).contains(&p), "{p}");
        }
    }

    #[test]
    fn the_first_call_chooses_and_persists_and_later_calls_return_the_same_port() {
        let tmp = tempfile::tempdir().unwrap();
        let first = load_or_choose_port(tmp.path()).unwrap();
        assert!((PORT_MIN..=PORT_MAX).contains(&first));
        assert!(tmp.path().join("gateway.json").is_file());
        for _ in 0..5 {
            assert_eq!(load_or_choose_port(tmp.path()).unwrap(), first);
        }
        assert!(
            !tmp.path().join("gateway.tmp").exists(),
            "原子写不留临时文件"
        );
    }

    #[test]
    fn a_missing_root_directory_is_created() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("a/b");
        let port = load_or_choose_port(&root).unwrap();
        assert_eq!(load_or_choose_port(&root).unwrap(), port);
    }

    #[test]
    fn corrupt_or_out_of_range_settings_are_errors_not_a_silent_new_port() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gateway.json");
        for bad in [
            "{not json",
            r#"{"port": 80}"#,
            r#"{"port": 50000, "extra": 1}"#,
            r#"{"port": 70000}"#,
            "{}",
        ] {
            fs::write(&path, bad).unwrap();
            let err = load_or_choose_port(tmp.path()).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{bad}");
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                bad,
                "坏文件原样保留,不被覆盖"
            );
        }
    }

    #[test]
    fn an_explicitly_persisted_port_is_returned_as_is() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("gateway.json"), r#"{"port": 51234}"#).unwrap();
        assert_eq!(load_or_choose_port(tmp.path()).unwrap(), 51234);
    }

    /// macOS 的临时端口段是 49152–65535,Linux 是 32768–60999:任何监听端口 0 的程序都从那里拿端口。
    /// 固定端口要避开它们,否则 dozerd 停着的时候别的程序可能占走它。
    #[test]
    fn the_range_stays_below_both_ephemeral_port_ranges() {
        const {
            assert!(PORT_MIN >= 1024);
            assert!(PORT_MAX < 32768);
            assert!(PORT_MIN < PORT_MAX);
        }
    }
}
