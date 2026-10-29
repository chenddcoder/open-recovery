//! 带页缓存的分区/卷读层。
//!
//! 文件系统解析是典型的随机小读（一遍解析可能要发几万次 4K 读），
//! 直接打到设备上会很慢；这里做一层 64 KiB 页缓存，并且切片出来的子读取器
//! 共享同一份缓存（整盘扫描 + 卷解析不会互相冷启）。

use crate::device::{read_full, BlockDevice};
use crate::error::Result;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const DEFAULT_PAGE: u64 = 64 * 1024;

struct Page {
    data: Vec<u8>,
    used: u64,
}

struct Cache {
    pages: HashMap<u64, Page>,
    tick: u64,
    capacity: usize,
    page_size: u64,
    /// 缓存命中统计，CLI `-v` 用得上
    hits: u64,
    misses: u64,
}

impl Cache {
    fn evict_if_needed(&mut self) {
        while self.pages.len() >= self.capacity {
            // 淘汰最久未使用页。容量 1024 量级，线性扫描成本可忽略。
            let victim = self
                .pages
                .iter()
                .min_by_key(|(_, p)| p.used)
                .map(|(k, _)| *k);
            match victim {
                Some(k) => {
                    self.pages.remove(&k);
                }
                None => break,
            }
        }
    }
}

/// 一个设备上的字节区间视图（整盘、分区、或任意偏移）。
pub struct Reader {
    dev: Arc<dyn BlockDevice>,
    base: u64,
    len: u64,
    cache: Arc<Mutex<Cache>>,
}

impl Reader {
    pub fn new(dev: Arc<dyn BlockDevice>) -> Self {
        let len = dev.size();
        Self::with_range(dev, 0, len)
    }

    pub fn with_range(dev: Arc<dyn BlockDevice>, base: u64, len: u64) -> Self {
        let page_size = page_size_for(dev.sector_size());
        let cache = Arc::new(Mutex::new(Cache {
            pages: HashMap::new(),
            tick: 0,
            capacity: capacity_for(page_size),
            page_size,
            hits: 0,
            misses: 0,
        }));
        let total = dev.size();
        let len = if base >= total { 0 } else { len.min(total - base) };
        Self { dev, base, len, cache }
    }

    /// 共享同一份页缓存，切出子区间（分区 → 文件系统）。
    pub fn slice(&self, base: u64, len: u64) -> Self {
        let base_abs = self.base.saturating_add(base);
        let len = len.min(self.len.saturating_sub(base));
        Self {
            dev: Arc::clone(&self.dev),
            base: base_abs,
            len,
            cache: Arc::clone(&self.cache),
        }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    pub fn sector_size(&self) -> u32 {
        self.dev.sector_size()
    }

    pub fn device(&self) -> &Arc<dyn BlockDevice> {
        &self.dev
    }

    pub fn cache_stats(&self) -> (u64, u64) {
        let c = self.cache.lock().unwrap();
        (c.hits, c.misses)
    }

    /// 读入 `buf`，越界部分补零（容忍截断镜像）。
    pub fn read(&self, off: u64, buf: &mut [u8]) -> Result<()> {
        if off >= self.len {
            for b in buf.iter_mut() {
                *b = 0;
            }
            return Ok(());
        }
        let want = buf.len().min((self.len - off) as usize);
        let mut done = 0usize;
        {
            let mut cache = self.cache.lock().unwrap();
            let ps = cache.page_size;
            while done < want {
                let abs = self.base + off + done as u64;
                let page_idx = abs / ps;
                let in_page = (abs % ps) as usize;
                if !cache.pages.contains_key(&page_idx) {
                    cache.evict_if_needed();
                    cache.tick += 1;
                    let used = cache.tick;
                    let mut data = vec![0u8; ps as usize];
                    let page_off = page_idx * ps;
                    // 直接落设备，绕过自己的缓存（否则递归）
                    let dev_off = page_off;
                    let _ = read_full(self.dev.as_ref(), dev_off, &mut data)?;
                    cache.misses += 1;
                    cache.pages.insert(page_idx, Page { data, used });
                } else {
                    cache.tick += 1;
                    let tick = cache.tick;
                    cache.hits += 1;
                    if let Some(p) = cache.pages.get_mut(&page_idx) {
                        p.used = tick;
                    }
                }
                let page = cache.pages.get(&page_idx).unwrap();
                let avail = ps as usize - in_page;
                let n = avail.min(want - done);
                buf[done..done + n].copy_from_slice(&page.data[in_page..in_page + n]);
                done += n;
            }
        }
        for b in &mut buf[want..] {
            *b = 0;
        }
        Ok(())
    }

    pub fn read_vec(&self, off: u64, len: usize) -> Result<Vec<u8>> {
        let mut v = vec![0u8; len];
        self.read(off, &mut v)?;
        Ok(v)
    }

    pub fn u8_at(&self, off: u64) -> Result<u8> {
        let mut b = [0u8; 1];
        self.read(off, &mut b)?;
        Ok(b[0])
    }

    pub fn u16_at(&self, off: u64) -> Result<u16> {
        let mut b = [0u8; 2];
        self.read(off, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }

    pub fn u16_be_at(&self, off: u64) -> Result<u16> {
        let mut b = [0u8; 2];
        self.read(off, &mut b)?;
        Ok(u16::from_be_bytes(b))
    }

    pub fn u32_at(&self, off: u64) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read(off, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    pub fn u32_be_at(&self, off: u64) -> Result<u32> {
        let mut b = [0u8; 4];
        self.read(off, &mut b)?;
        Ok(u32::from_be_bytes(b))
    }

    pub fn u64_at(&self, off: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read(off, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    pub fn u64_be_at(&self, off: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read(off, &mut b)?;
        Ok(u64::from_be_bytes(b))
    }
}

fn page_size_for(sector: u32) -> u64 {
    // 4K 扇区的盘上，页必须对齐扇区，否则跨扇区读会被设备拒绝。
    let s = sector.max(512) as u64;
    DEFAULT_PAGE.max(s).next_power_of_two()
}

/// 缓存上限约 64 MiB：够覆盖常规目录/inode 表的局部性，又不会在大卷上吃爆内存。
fn capacity_for(page_size: u64) -> usize {
    let want = 64 * 1024 * 1024u64 / page_size;
    want.clamp(16, 8192) as usize
}
