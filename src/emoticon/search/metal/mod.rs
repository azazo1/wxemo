//! macOS 原生 Metal compute 后端.
//!
//! 目录职责:
//!
//! - `kernel.metal`: MSL 源码, 每个 GPU 线程处理一个候选 seed.
//! - `host.rs`: 设备与管线管理, 分批提交, 在途 command buffer 与进度推进.
//!
//! MSL 在运行期由驱动自带的编译器编译, 不依赖构建期的 metal 编译器.

mod host;

#[cfg(test)]
mod tests;

pub use host::{device_name, run};

#[cfg(test)]
pub(crate) use host::derive_keys_probe;
