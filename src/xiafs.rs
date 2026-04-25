//! Minimal xiafs (Q. Frank Xia, 1992) filesystem image writer.
//!
//! Layout (1024-byte zones throughout):
//!
//! ```text
//! zone 0:                       boot (512 bytes) + superblock (512 bytes)
//! zones 1..1+imap:              inode bitmap
//! zones 1+imap..1+imap+zmap:    zone bitmap
//! zones 1+imap+zmap..firstdz:   inode table (16 inodes per zone)
//! zones firstdz..:              data zones
//! ```
//!
//! Files use 8 direct zone pointers plus one single-indirect (256 entries
//! at 4 bytes each). Maximum file size is therefore (8 + 256) × 1024 ≈
//! 264 KB.
//!
//! The high byte of `i_zone[0..=2]` carries a 24-bit "blocks allocated"
//! count (low byte in `zone[0]`, mid in `zone[1]`, high in `zone[2]`),
//! which the kernel reads back as `i_blocks`.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const BLOCK_SIZE: usize = 1024;
pub const MAGIC: u32 = 0x012F_D16D;
const ROOT_INO: u32 = 1;
const BAD_INO: u32 = 2;
const NAME_LEN: usize = 248;
const NUM_ZONES: usize = 10;
const DIRECT_ZONES: usize = 8;
const INODES_PER_BLOCK: u32 = (BLOCK_SIZE / 64) as u32; // 16
const ADDRS_PER_ZONE: u32 = (BLOCK_SIZE / 4) as u32; // 256
const INODE_SIZE: usize = 64;

const S_IFREG: u32 = 0o100_000;
const S_IFDIR: u32 = 0o040_000;

/// Stats reported after writing.
pub struct Stats {
    pub nzones: u32,
    pub ninodes: u32,
    pub first_data_zone: u32,
    pub ndatazones: u32,
}

#[derive(Clone, Copy)]
struct Inode {
    mode: u16,
    nlinks: u16,
    uid: u16,
    gid: u16,
    size: u32,
    ctime: u32,
    atime: u32,
    mtime: u32,
    /// Raw zone pointers. The high byte of `zone[0..=2]` is overloaded
    /// to hold the packed `blocks_allocated` count; mask with 0x00FFFFFF
    /// to get the actual zone number.
    zone: [u32; NUM_ZONES],
}

impl Inode {
    const fn zero() -> Self {
        Self {
            mode: 0,
            nlinks: 0,
            uid: 0,
            gid: 0,
            size: 0,
            ctime: 0,
            atime: 0,
            mtime: 0,
            zone: [0; NUM_ZONES],
        }
    }
}

fn dir_rec_len(name_len: u8) -> u16 {
    let n = u16::from(name_len);
    (7 + n).div_ceil(4) * 4
}

fn set_inode_zone(inode: &mut Inode, file_zone: usize, disk_zone: u32) -> anyhow::Result<()> {
    if file_zone >= DIRECT_ZONES {
        anyhow::bail!("set_inode_zone: indirect zones must be handled by caller");
    }
    if file_zone < 3 {
        // Preserve the high byte (which holds packed block-count bits).
        inode.zone[file_zone] = (inode.zone[file_zone] & 0xFF00_0000) | disk_zone;
    } else {
        inode.zone[file_zone] = disk_zone;
    }
    Ok(())
}

fn set_inode_blocks(inode: &mut Inode, blocks: u32) {
    // Pack blocks (24 bits) across the high bytes of zone[0..=2].
    inode.zone[0] = (inode.zone[0] & 0x00FF_FFFF) | ((blocks << 24) & 0xFF00_0000);
    inode.zone[1] = (inode.zone[1] & 0x00FF_FFFF) | ((blocks << 16) & 0xFF00_0000);
    inode.zone[2] = (inode.zone[2] & 0x00FF_FFFF) | ((blocks << 8) & 0xFF00_0000);
}

pub struct Xiafs {
    image: Vec<u8>,
    nzones: u32,
    ninodes: u32,
    imap_zones: u32,
    zmap_zones: u32,
    first_data_zone: u32,
    inodes: Vec<Inode>,
    /// `inode_used[i]` is set when inode `i` is allocated; index 0 unused.
    inode_used: Vec<bool>,
    /// `zone_used[z]` is set when zone `z` is allocated.
    zone_used: Vec<bool>,
    next_zone: u32,
    next_inode: u32,
    now: u32,
}

impl Xiafs {
    pub fn new(size_mb: u32) -> anyhow::Result<Self> {
        if size_mb == 0 {
            anyhow::bail!("size must be positive");
        }
        let image_size = u64::from(size_mb) * 1024 * 1024;
        if image_size > u64::from(u32::MAX) {
            anyhow::bail!("image size exceeds 4 GB");
        }
        let nzones = (image_size / BLOCK_SIZE as u64) as u32;
        if nzones < 32 {
            anyhow::bail!("filesystem too small");
        }
        if nzones >= (1u32 << 24) {
            anyhow::bail!("filesystem exceeds 16 GB zone-number limit");
        }

        // Layout (matching the original mkxfs heuristic).
        let inode_zones = (nzones >> 2) / INODES_PER_BLOCK + 1;
        let ninodes = inode_zones * INODES_PER_BLOCK;
        let imap_zones = ninodes / (BLOCK_SIZE as u32 * 8) + 1;
        let zmap_zones = nzones / (BLOCK_SIZE as u32 * 8) + 1;
        let first_data_zone = 1 + imap_zones + zmap_zones + inode_zones;

        if nzones <= first_data_zone + 1 {
            anyhow::bail!("filesystem too small for layout");
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as u32)
            .unwrap_or(0);

        let mut inode_used = vec![false; (ninodes + 1) as usize];
        inode_used[ROOT_INO as usize] = true;
        inode_used[BAD_INO as usize] = true;

        let mut zone_used = vec![false; nzones as usize];
        for slot in zone_used.iter_mut().take(first_data_zone as usize) {
            *slot = true;
        }

        let mut xiafs = Self {
            image: vec![0u8; image_size as usize],
            nzones,
            ninodes,
            imap_zones,
            zmap_zones,
            first_data_zone,
            inodes: vec![Inode::zero(); ninodes as usize],
            inode_used,
            zone_used,
            next_zone: first_data_zone,
            next_inode: BAD_INO + 1,
            now,
        };

        // Set up root inode (inode 1) and its initial directory block.
        let root_block = xiafs.alloc_zone()?;
        {
            let root = &mut xiafs.inodes[(ROOT_INO - 1) as usize];
            root.mode = (S_IFDIR | 0o755) as u16;
            root.nlinks = 2;
            root.size = BLOCK_SIZE as u32;
            root.ctime = now;
            root.atime = now;
            root.mtime = now;
            set_inode_zone(root, 0, root_block)?;
            set_inode_blocks(root, 1);
        }
        xiafs.init_dir_block(root_block, ROOT_INO, ROOT_INO);

        // Bad blocks inode (inode 2): empty regular file placeholder.
        {
            let bad = &mut xiafs.inodes[(BAD_INO - 1) as usize];
            bad.mode = S_IFREG as u16;
            bad.nlinks = 1;
            bad.ctime = now;
            bad.atime = now;
            bad.mtime = now;
        }

        Ok(xiafs)
    }

    pub fn populate_root(&mut self, src: &Path) -> anyhow::Result<()> {
        self.populate_dir(ROOT_INO, src)
    }

    fn populate_dir(&mut self, dir_ino: u32, src_path: &Path) -> anyhow::Result<()> {
        for entry in fs::read_dir(src_path)? {
            let entry = entry?;
            let path = entry.path();
            let meta = fs::symlink_metadata(&path)?;
            let raw_name = entry.file_name();
            let name = raw_name
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF8 filename: {:?}", raw_name))?;

            if name.len() > NAME_LEN {
                anyhow::bail!("name too long (max {NAME_LEN}): {name}");
            }
            if !meta.is_file() && !meta.is_dir() {
                continue;
            }

            let child_ino = self.alloc_inode()?;
            // The C version writes st.st_mode (full mode incl. type bits)
            // into i_mode unconditionally first, then overwrites only for
            // dirs/files via the kernel-style S_IF* path. Reproduce that
            // by copying the mode here, then we always overwrite below.
            {
                let child = &mut self.inodes[(child_ino - 1) as usize];
                child.mode = mode_from_meta(&meta) as u16;
                child.uid = 0;
                child.gid = 0;
                child.ctime = self.now;
                child.atime = self.now;
                child.mtime = self.now;
            }

            if meta.is_dir() {
                let block = self.alloc_zone()?;
                {
                    let child = &mut self.inodes[(child_ino - 1) as usize];
                    child.nlinks = 2;
                    child.size = BLOCK_SIZE as u32;
                    set_inode_zone(child, 0, block)?;
                    set_inode_blocks(child, 1);
                }
                self.init_dir_block(block, child_ino, dir_ino);
                self.add_dir_entry(dir_ino, name, child_ino)?;
                self.inodes[(dir_ino - 1) as usize].nlinks += 1;
                self.populate_dir(child_ino, &path)?;
            } else {
                self.inodes[(child_ino - 1) as usize].nlinks = 1;
                self.add_dir_entry(dir_ino, name, child_ino)?;
                self.write_file_data(child_ino, &path)?;
            }
        }
        Ok(())
    }

    fn write_file_data(&mut self, ino: u32, path: &Path) -> anyhow::Result<()> {
        let mut data = Vec::new();
        File::open(path)?.read_to_end(&mut data)?;
        let size = u32::try_from(data.len())
            .map_err(|_| anyhow::anyhow!("file exceeds 4 GB: {}", path.display()))?;
        self.inodes[(ino - 1) as usize].size = size;

        let mut indirect = [0u32; ADDRS_PER_ZONE as usize];
        let mut have_indirect = false;
        let mut blocks: u32 = 0;

        for (file_zone, chunk) in data.chunks(BLOCK_SIZE).enumerate() {
            let disk_zone = self.alloc_zone()?;
            let off = (disk_zone as usize) * BLOCK_SIZE;
            self.image[off..off + chunk.len()].copy_from_slice(chunk);

            if file_zone < DIRECT_ZONES {
                set_inode_zone(&mut self.inodes[(ino - 1) as usize], file_zone, disk_zone)?;
            } else {
                let idx = file_zone - DIRECT_ZONES;
                if idx >= ADDRS_PER_ZONE as usize {
                    anyhow::bail!("file too large (needs double indirect)");
                }
                indirect[idx] = disk_zone;
                have_indirect = true;
            }
            blocks += 1;
        }

        if have_indirect {
            let ind_zone = self.alloc_zone()?;
            self.inodes[(ino - 1) as usize].zone[8] = ind_zone;
            let off = (ind_zone as usize) * BLOCK_SIZE;
            for (i, z) in indirect.iter().enumerate() {
                let p = off + i * 4;
                self.image[p..p + 4].copy_from_slice(&z.to_le_bytes());
            }
        }

        set_inode_blocks(&mut self.inodes[(ino - 1) as usize], blocks);
        Ok(())
    }

    fn add_dir_entry(&mut self, dir_ino: u32, name: &str, child_ino: u32) -> anyhow::Result<()> {
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len() as u8;
        let need = dir_rec_len(name_len);

        let dir = self.inodes[(dir_ino - 1) as usize]; // Copy
        let n_blocks = upper(dir.size as usize, BLOCK_SIZE);

        for b in 0..n_blocks {
            let disk_block = dir.zone[b] & 0x00FF_FFFF;
            let block_off = (disk_block as usize) * BLOCK_SIZE;
            let mut off = 0usize;
            while off < BLOCK_SIZE {
                let de_off = block_off + off;
                let d_rec_len =
                    u16::from_le_bytes(self.image[de_off + 4..de_off + 6].try_into().unwrap());
                let d_name_len = self.image[de_off + 6];
                let actual_len = dir_rec_len(d_name_len);
                let slack = d_rec_len.saturating_sub(actual_len);

                if slack >= need {
                    // Shrink the existing entry to its actual length.
                    self.image[de_off + 4..de_off + 6].copy_from_slice(&actual_len.to_le_bytes());
                    // Write the new entry in the freed slack space.
                    let new_off = de_off + actual_len as usize;
                    self.image[new_off..new_off + 4].copy_from_slice(&child_ino.to_le_bytes());
                    self.image[new_off + 4..new_off + 6].copy_from_slice(&slack.to_le_bytes());
                    self.image[new_off + 6] = name_len;
                    self.image[new_off + 7..new_off + 7 + name_bytes.len()]
                        .copy_from_slice(name_bytes);
                    return Ok(());
                }
                off += d_rec_len as usize;
            }
        }

        // No slack — need a fresh directory block.
        let block_idx = n_blocks;
        if block_idx >= DIRECT_ZONES {
            anyhow::bail!("directory too large");
        }
        let new_block = self.alloc_zone()?;
        set_inode_zone(
            &mut self.inodes[(dir_ino - 1) as usize],
            block_idx,
            new_block,
        )?;
        self.inodes[(dir_ino - 1) as usize].size = ((block_idx + 1) * BLOCK_SIZE) as u32;

        let off = (new_block as usize) * BLOCK_SIZE;
        // (image is zero-init, so no explicit clear needed)
        self.image[off..off + 4].copy_from_slice(&child_ino.to_le_bytes());
        self.image[off + 4..off + 6].copy_from_slice(&(BLOCK_SIZE as u16).to_le_bytes());
        self.image[off + 6] = name_len;
        self.image[off + 7..off + 7 + name_bytes.len()].copy_from_slice(name_bytes);
        Ok(())
    }

    fn init_dir_block(&mut self, block: u32, self_ino: u32, parent_ino: u32) {
        let off = (block as usize) * BLOCK_SIZE;
        let dot_len = dir_rec_len(1);

        // dot entry
        self.image[off..off + 4].copy_from_slice(&self_ino.to_le_bytes());
        self.image[off + 4..off + 6].copy_from_slice(&dot_len.to_le_bytes());
        self.image[off + 6] = 1;
        self.image[off + 7] = b'.';

        // dotdot entry, fills the remainder of the block
        let dotdot_off = off + dot_len as usize;
        let dotdot_len = BLOCK_SIZE as u16 - dot_len;
        self.image[dotdot_off..dotdot_off + 4].copy_from_slice(&parent_ino.to_le_bytes());
        self.image[dotdot_off + 4..dotdot_off + 6].copy_from_slice(&dotdot_len.to_le_bytes());
        self.image[dotdot_off + 6] = 2;
        self.image[dotdot_off + 7] = b'.';
        self.image[dotdot_off + 8] = b'.';
    }

    fn alloc_zone(&mut self) -> anyhow::Result<u32> {
        while self.next_zone < self.nzones {
            let z = self.next_zone as usize;
            self.next_zone += 1;
            if !self.zone_used[z] {
                self.zone_used[z] = true;
                return Ok(z as u32);
            }
        }
        anyhow::bail!("no free zones");
    }

    fn alloc_inode(&mut self) -> anyhow::Result<u32> {
        while self.next_inode <= self.ninodes {
            let i = self.next_inode as usize;
            self.next_inode += 1;
            if !self.inode_used[i] {
                self.inode_used[i] = true;
                return Ok(i as u32);
            }
        }
        anyhow::bail!("no free inodes");
    }

    pub fn write_to(mut self, path: &Path) -> anyhow::Result<Stats> {
        self.write_superblock();
        self.write_inode_bitmap();
        self.write_zone_bitmap();
        self.write_inode_table();

        let mut f = File::create(path)?;
        f.write_all(&self.image)?;
        f.sync_all()?;

        Ok(Stats {
            nzones: self.nzones,
            ninodes: self.ninodes,
            first_data_zone: self.first_data_zone,
            ndatazones: self.nzones - self.first_data_zone,
        })
    }

    fn write_superblock(&mut self) {
        let max_size =
            ((ADDRS_PER_ZONE + 1) * ADDRS_PER_ZONE + DIRECT_ZONES as u32) * BLOCK_SIZE as u32;

        // Boot area is bytes 0..512 (zeros). Superblock metadata starts at 512.
        let sb = &mut self.image[512..1024];
        sb[0..4].copy_from_slice(&(BLOCK_SIZE as u32).to_le_bytes()); // s_zone_size
        sb[4..8].copy_from_slice(&self.nzones.to_le_bytes()); // s_nzones
        sb[8..12].copy_from_slice(&self.ninodes.to_le_bytes()); // s_ninodes
        sb[12..16].copy_from_slice(&(self.nzones - self.first_data_zone).to_le_bytes()); // s_ndatazones
        sb[16..20].copy_from_slice(&self.imap_zones.to_le_bytes()); // s_imap_zones
        sb[20..24].copy_from_slice(&self.zmap_zones.to_le_bytes()); // s_zmap_zones
        sb[24..28].copy_from_slice(&self.first_data_zone.to_le_bytes()); // s_firstdatazone
        sb[28..32].copy_from_slice(&0u32.to_le_bytes()); // s_zone_shift
        sb[32..36].copy_from_slice(&max_size.to_le_bytes()); // s_max_size
                                                             // s_reserved[4] at 36..52 stays zero
        sb[52..56].copy_from_slice(&self.first_data_zone.to_le_bytes()); // s_firstkernzone
        sb[56..60].copy_from_slice(&0u32.to_le_bytes()); // s_kernzones
        sb[60..64].copy_from_slice(&MAGIC.to_le_bytes()); // s_magic
    }

    fn write_inode_bitmap(&mut self) {
        let off = BLOCK_SIZE;
        let bytes = (self.imap_zones as usize) * BLOCK_SIZE;
        // ninodes + 1 entries (bit 0 unused), but we trust inode_used.len() == ninodes + 1
        for (i, &used) in self.inode_used.iter().enumerate() {
            if used {
                self.image[off + i / 8] |= 1 << (i % 8);
            }
        }
        // Trailing bits beyond inode_used.len() are already zero in the
        // imap area thanks to the zero-init image.
        debug_assert!((self.inode_used.len() / 8) <= bytes);
    }

    fn write_zone_bitmap(&mut self) {
        let off = (1 + self.imap_zones as usize) * BLOCK_SIZE;
        let bytes = (self.zmap_zones as usize) * BLOCK_SIZE;
        for (z, &used) in self.zone_used.iter().enumerate() {
            if used {
                self.image[off + z / 8] |= 1 << (z % 8);
            }
        }
        debug_assert!((self.zone_used.len() / 8) <= bytes);
    }

    fn write_inode_table(&mut self) {
        let table_off = (1 + self.imap_zones as usize + self.zmap_zones as usize) * BLOCK_SIZE;
        for (idx, inode) in self.inodes.iter().enumerate() {
            let off = table_off + idx * INODE_SIZE;
            let buf = &mut self.image[off..off + INODE_SIZE];
            buf[0..2].copy_from_slice(&inode.mode.to_le_bytes());
            buf[2..4].copy_from_slice(&inode.nlinks.to_le_bytes());
            buf[4..6].copy_from_slice(&inode.uid.to_le_bytes());
            buf[6..8].copy_from_slice(&inode.gid.to_le_bytes());
            buf[8..12].copy_from_slice(&inode.size.to_le_bytes());
            buf[12..16].copy_from_slice(&inode.ctime.to_le_bytes());
            buf[16..20].copy_from_slice(&inode.atime.to_le_bytes());
            buf[20..24].copy_from_slice(&inode.mtime.to_le_bytes());
            for (i, z) in inode.zone.iter().enumerate() {
                let p = 24 + i * 4;
                buf[p..p + 4].copy_from_slice(&z.to_le_bytes());
            }
        }
    }
}

fn upper(n: usize, d: usize) -> usize {
    n.div_ceil(d)
}

#[cfg(unix)]
fn mode_from_meta(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

#[cfg(not(unix))]
fn mode_from_meta(meta: &fs::Metadata) -> u32 {
    if meta.is_dir() {
        S_IFDIR | 0o755
    } else {
        S_IFREG | 0o644
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn read_u32_le(buf: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
    }

    #[test]
    fn empty_image_has_correct_superblock() {
        let dir = TempDir::new().unwrap();
        let img = dir.path().join("test.xiafs");
        Xiafs::new(2).unwrap().write_to(&img).unwrap();

        let bytes = fs::read(&img).unwrap();
        assert_eq!(bytes.len(), 2 * 1024 * 1024);
        assert_eq!(read_u32_le(&bytes, 512), BLOCK_SIZE as u32);
        assert_eq!(read_u32_le(&bytes, 512 + 4), 2048);
        assert_eq!(read_u32_le(&bytes, 512 + 60), MAGIC);
    }

    #[test]
    fn root_block_has_dot_and_dotdot() {
        let dir = TempDir::new().unwrap();
        let img = dir.path().join("test.xiafs");
        Xiafs::new(2).unwrap().write_to(&img).unwrap();

        let bytes = fs::read(&img).unwrap();
        // Layout for a 2 MB image: nzones=2048, inode_zones=33, ninodes=528,
        // imap_zones=1, zmap_zones=1. Inode table starts at zone 3.
        // Root inode (ino 1) is at the very start of the inode table.
        let imap_zones = read_u32_le(&bytes, 512 + 16);
        let zmap_zones = read_u32_le(&bytes, 512 + 20);
        let inode_table_zone = 1 + imap_zones + zmap_zones;
        let root_off = (inode_table_zone as usize) * BLOCK_SIZE;
        // root.zone[0] is at byte 24 of the inode
        let root_zone0 = read_u32_le(&bytes, root_off + 24) & 0x00FF_FFFF;
        let dir_off = (root_zone0 as usize) * BLOCK_SIZE;
        // dot entry
        assert_eq!(read_u32_le(&bytes, dir_off), ROOT_INO);
        assert_eq!(bytes[dir_off + 6], 1);
        assert_eq!(bytes[dir_off + 7], b'.');
    }

    #[test]
    fn populate_walks_subdirs() {
        let src = TempDir::new().unwrap();
        let mut f = File::create(src.path().join("hello.txt")).unwrap();
        f.write_all(b"hi\n").unwrap();
        drop(f);
        fs::create_dir(src.path().join("sub")).unwrap();
        File::create(src.path().join("sub").join("nested.txt")).unwrap();

        let dst = TempDir::new().unwrap();
        let img = dst.path().join("out.xiafs");
        let mut xiafs = Xiafs::new(2).unwrap();
        xiafs.populate_root(src.path()).unwrap();
        let stats = xiafs.write_to(&img).unwrap();
        assert!(stats.ndatazones > 0);
    }

    #[test]
    fn name_too_long_is_rejected() {
        let src = TempDir::new().unwrap();
        let long_name = "x".repeat(NAME_LEN + 1);
        File::create(src.path().join(&long_name)).unwrap();
        let mut xiafs = Xiafs::new(2).unwrap();
        let err = xiafs.populate_root(src.path()).unwrap_err();
        assert!(err.to_string().contains("name too long"));
    }
}
