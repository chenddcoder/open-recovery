//! ext2 / ext3 / ext4。

pub mod dir;
pub mod extent;
pub mod fs;
pub mod inode;
pub mod superblock;

pub use fs::Ext;
pub use superblock::ExtSuper;
