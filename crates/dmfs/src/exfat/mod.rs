//! exFAT。

pub mod boot;
pub mod dir;
pub mod fs;

pub use boot::ExFatBoot;
pub use fs::ExFat;
