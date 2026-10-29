//! NTFS。

pub mod attr;
pub mod boot;
pub mod fixup;
pub mod fs;
pub mod index;
pub mod record;
pub mod runlist;

pub use boot::NtfsBoot;
pub use fs::Ntfs;
