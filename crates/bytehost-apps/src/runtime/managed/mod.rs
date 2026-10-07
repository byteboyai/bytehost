//! 受管运行时(A6d,`server` feature、unix):把 Node、uv(+ 由 uv 管理的 Python)装到 bytehost 自己的目录。
//!
//! - `pins`:**代码内常量表**(版本/URL/校验和由 `scripts/bytehost/pin-runtimes.sh` 从官方源生成、人审后提交);
//! - `store`:`<root>/runtimes/<name>/<version>/` 的布局与列目录/删除;
//! - (`fetch`/`install`/`manager`/`resolve` 见后续任务)

pub mod pins;
pub mod store;

pub use pins::{PYTHON_VERSION, Pin, Target, pin_for, pin_names_for};
pub use store::RuntimeStore;
