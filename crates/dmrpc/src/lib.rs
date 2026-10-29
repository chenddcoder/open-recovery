//! `dmrpc` —— dmde-rs 的会话 / 协议 / 特权 helper 支撑层。
//!
//! * [`session`]：打开源、列目录、预览、读文件（GUI 与 helper 共用同一实现）
//! * [`proto`]：GUI ↔ helper 的行协议（JSON 行 + 分块字节流）
//! * [`server`]：特权 helper 端（只读）
//! * [`client`]：GUI 端
//! * [`backend`]：本地 / 远程两种后端的统一门面

pub mod backend;
pub mod client;
pub mod model;
pub mod proto;
pub mod server;
pub mod session;

pub use backend::{open_local, Backend};
pub use client::Client;
pub use model::{DiskDto, EntryDto, PartDto, PreviewDto, RecoverDto, SourceDto};
pub use session::Session;

/// helper 与 GUI 约定的版本号：两边不一致时 GUI 会提示重建
pub const PROTO_VERSION: &str = env!("CARGO_PKG_VERSION");
