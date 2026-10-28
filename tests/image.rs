//! Read serialized images independently of the writer's private types/helpers.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::process::Command;

fn u16_at(bytes: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(bytes[off..off + 2].try_into().unwrap())
}

fn u32_at(bytes: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap())
}

struct Reader {
    bytes: Vec<u8>,
    inodes: BTreeSet<u32>,
    zones: BTreeSet<u32>,
    files: BTreeMap<String, Vec<u8>>,
}

impl Reader {
    fn zone(&mut self, number: u32) -> Vec<u8> {
        assert!(number >= u32_at(&self.bytes, 536));
        assert!(number < u32_at(&self.bytes, 516));
        assert!(self.zones.insert(number), "zone {number} referenced twice");
        self.bytes[number as usize * 1024..(number as usize + 1) * 1024].to_vec()
    }

    fn walk(&mut self, ino: u32, parent: u32, path: &str) {
        assert!((1..=u32_at(&self.bytes, 520)).contains(&ino));
        assert!(self.inodes.insert(ino));
        let table = (1 + u32_at(&self.bytes, 528) + u32_at(&self.bytes, 532)) as usize * 1024;
        let off = table + (ino as usize - 1) * 64;
        let mode = u16_at(&self.bytes, off);
        let links = u16_at(&self.bytes, off + 2);
        let size = u32_at(&self.bytes, off + 8) as usize;
        let raw: Vec<u32> = (0..10)
            .map(|i| u32_at(&self.bytes, off + 24 + i * 4))
            .collect();
        let sectors: u32 = (0..3).map(|i| (raw[i] >> 24) << (8 * i)).sum();
        let pointers: Vec<u32> = raw.iter().map(|z| z & 0xffffff).collect();
        let count = size.div_ceil(1024);
        let mut data = Vec::new();
        for &zone in &pointers[..count.min(8)] {
            data.extend(self.zone(zone));
        }
        for &zone in &pointers[count.min(8)..8] {
            assert_eq!(zone, 0);
        }
        if count > 8 {
            let indirect = self.zone(pointers[8]);
            for i in 0..count - 8 {
                data.extend(self.zone(u32_at(&indirect, i * 4)));
            }
            assert!(indirect[(count - 8) * 4..].iter().all(|b| *b == 0));
        } else {
            assert_eq!(pointers[8], 0);
        }
        assert_eq!(pointers[9], 0);
        assert_eq!(sectors as usize, (count + usize::from(count > 8)) * 2);
        assert!(data[size..].iter().all(|b| *b == 0));
        data.truncate(size);
        match mode & 0o170000 {
            0o100000 => {
                assert_eq!(links, 1);
                assert!(self.files.insert(path.to_owned(), data).is_none());
            }
            0o040000 => {
                assert_eq!(size % 1024, 0);
                assert_eq!(u16_at(&data, 4), 12);
                let mut entries = BTreeMap::new();
                for block in (0..size).step_by(1024) {
                    let mut pos = block;
                    while pos < block + 1024 {
                        let child = u32_at(&data, pos);
                        let rec = u16_at(&data, pos + 4) as usize;
                        let len = data[pos + 6] as usize;
                        assert!(rec >= 12 && rec % 4 == 0 && pos + rec <= block + 1024);
                        assert!((1..=248).contains(&len) && len + 8 <= rec);
                        assert_eq!(data[pos + 7 + len], 0);
                        let name = std::str::from_utf8(&data[pos + 7..pos + 7 + len]).unwrap();
                        assert!(entries.insert(name.to_owned(), child).is_none());
                        pos += rec;
                    }
                    assert_eq!(pos, block + 1024);
                }
                assert_eq!(entries.remove("."), Some(ino));
                assert_eq!(entries.remove(".."), Some(parent));
                let mut subdirs = 0;
                for (name, child) in entries {
                    let child_off = table + (child as usize - 1) * 64;
                    if u16_at(&self.bytes, child_off) & 0o170000 == 0o040000 {
                        subdirs += 1;
                    }
                    self.walk(child, ino, &format!("{path}/{name}"));
                }
                assert_eq!(links, 2 + subdirs);
            }
            mode => panic!("unexpected file type {mode:o}"),
        }
    }

    fn verify_bitmaps(&self) {
        let imap = u32_at(&self.bytes, 528) as usize;
        let zmap = u32_at(&self.bytes, 532) as usize;
        let ninodes = u32_at(&self.bytes, 520);
        let first = u32_at(&self.bytes, 536);
        let ndata = u32_at(&self.bytes, 524);
        for bit in 0..imap * 8192 {
            let occupied = self.bytes[1024 + bit / 8] & (1 << (bit % 8)) != 0;
            let ino = bit as u32;
            assert_eq!(
                occupied,
                ino == 0 || ino == 2 || ino > ninodes || self.inodes.contains(&ino)
            );
        }
        for bit in 0..zmap * 8192 {
            let occupied = self.bytes[(1 + imap) * 1024 + bit / 8] & (1 << (bit % 8)) != 0;
            assert_eq!(
                occupied,
                bit == 0 || bit > ndata as usize || self.zones.contains(&(first + bit as u32 - 1))
            );
        }
    }
}

#[test]
fn cli_image_round_trips_nested_files_and_allocation_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source");
    fs::create_dir_all(src.join("sub/nested")).unwrap();
    let mut expected = BTreeMap::new();
    for (i, size) in [0, 1, 1023, 1024, 8192, 8193, 270336]
        .into_iter()
        .enumerate()
    {
        let name = format!("sub/nested/file{i}");
        let data: Vec<u8> = (0..size).map(|n| (n % 251) as u8).collect();
        fs::write(src.join(&name), &data).unwrap();
        expected.insert(format!("/{name}"), data);
    }
    for i in 0..40 {
        let name = format!("{i:03}{}", "x".repeat(120));
        fs::write(src.join(&name), b"payload").unwrap();
        expected.insert(format!("/{name}"), b"payload".to_vec());
    }
    let output = dir.path().join("out.xiafs");
    let result = Command::new(env!("CARGO_BIN_EXE_mkfs-xiafs"))
        .arg("-d")
        .arg(&src)
        .arg(&output)
        .arg("2")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bytes = fs::read(output).unwrap();
    assert_eq!(bytes.len(), 2 * 1024 * 1024);
    assert_eq!(u32_at(&bytes, 512), 1024);
    assert_eq!(u32_at(&bytes, 572), 0x012f_d16d);
    let mut reader = Reader {
        bytes,
        inodes: BTreeSet::new(),
        zones: BTreeSet::new(),
        files: BTreeMap::new(),
    };
    reader.walk(1, 1, "");
    reader.verify_bitmaps();
    assert_eq!(reader.files, expected);
}
