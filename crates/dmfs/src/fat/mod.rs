//! FAT12/16/32。

pub mod boot;
pub mod dir;
pub mod fs;
pub mod table;

pub use boot::{FatBoot, FatType};
pub use fs::Fat;
