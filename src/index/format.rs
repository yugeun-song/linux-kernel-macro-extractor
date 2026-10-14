use std::io::{self, Write};

pub const MAGIC: &[u8; 8] = b"KMXIDX\0\x01";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag(pub [u8; 4]);

impl Tag {
    pub const STRS: Tag = Tag(*b"STRS");
    pub const FILE: Tag = Tag(*b"FILE");
    pub const DIRS: Tag = Tag(*b"DIRS");
    pub const NFST: Tag = Tag(*b"NFST");
    pub const POST: Tag = Tag(*b"POST");
    pub const PFST: Tag = Tag(*b"PFST");
    pub const ARCH: Tag = Tag(*b"ARCH");
    pub const TOMB: Tag = Tag(*b"TOMB");
    pub const CHNK: Tag = Tag(*b"CHNK");
}

pub const FILE_REC: usize = 64;
pub const DIR_REC: usize = 32;
pub const CHUNK_REC: usize = 32;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stat {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    pub ino: u64,
}

impl Stat {
    pub fn of(meta: &std::fs::Metadata) -> Stat {
        use std::os::unix::fs::MetadataExt;
        Stat {
            size: meta.len(),
            mtime_ns: meta.mtime() * 1_000_000_000 + meta.mtime_nsec(),
            ctime_ns: meta.ctime() * 1_000_000_000 + meta.ctime_nsec(),
            ino: meta.ino(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRec {
    pub path: String,
    pub flags: u32,
    pub arch: u16,
    pub stat: Stat,
    pub hash: u128,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirRec {
    pub path: String,
    pub flags: u32,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

pub fn get_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    let mut shift = 0;
    loop {
        let b = *buf.get(*pos)?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Some(v);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

#[inline]
pub fn rd_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

#[inline]
pub fn rd_u32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap_or([0; 4]))
}

#[inline]
pub fn rd_u64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or([0; 8]))
}

#[derive(Default)]
pub struct Writer {
    sections: Vec<(Tag, Vec<u8>)>,
}

impl Writer {
    pub fn section(&mut self, tag: Tag, data: Vec<u8>) {
        self.sections.push((tag, data));
    }

    pub fn size(&self) -> u64 {
        let header_len = 8 + 4 + 4 + self.sections.len() as u64 * 20;
        self.sections
            .iter()
            .fold(align8(header_len), |at, (_, d)| align8(at + d.len() as u64))
    }

    pub fn finish(self, out: &mut impl Write) -> io::Result<u64> {
        let header_len = 8 + 4 + 4 + self.sections.len() * 20;
        let mut offset = align8(header_len as u64);
        let mut table = Vec::with_capacity(self.sections.len());
        for (tag, data) in &self.sections {
            table.push((*tag, offset, data.len() as u64));
            offset = align8(offset + data.len() as u64);
        }
        let mut head = Vec::with_capacity(header_len);
        head.extend_from_slice(MAGIC);
        head.extend_from_slice(&crate::store::FORMAT_VERSION.to_le_bytes());
        head.extend_from_slice(&(self.sections.len() as u32).to_le_bytes());
        for (tag, off, len) in &table {
            head.extend_from_slice(&tag.0);
            head.extend_from_slice(&off.to_le_bytes());
            head.extend_from_slice(&len.to_le_bytes());
        }
        out.write_all(&head)?;
        let mut written = head.len() as u64;
        for ((_, data), (_, off, _)) in self.sections.into_iter().zip(&table) {
            let pad = (*off - written) as usize;
            out.write_all(&[0u8; 8][..pad])?;
            out.write_all(&data)?;
            written = off + data.len() as u64;
        }
        Ok(written)
    }
}

fn align8(v: u64) -> u64 {
    (v + 7) & !7
}

pub fn encode_files(files: &[FileRec], strs: &mut Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(files.len() * FILE_REC);
    for f in files {
        let off = strs.len() as u32;
        strs.extend_from_slice(f.path.as_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&(f.path.len() as u32).to_le_bytes());
        out.extend_from_slice(&f.flags.to_le_bytes());
        out.extend_from_slice(&f.arch.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&f.stat.size.to_le_bytes());
        out.extend_from_slice(&f.stat.mtime_ns.to_le_bytes());
        out.extend_from_slice(&f.stat.ctime_ns.to_le_bytes());
        out.extend_from_slice(&f.stat.ino.to_le_bytes());
        out.extend_from_slice(&(f.hash as u64).to_le_bytes());
        out.extend_from_slice(&((f.hash >> 64) as u64).to_le_bytes());
    }
    out
}

pub fn encode_dirs(dirs: &[DirRec], strs: &mut Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(dirs.len() * DIR_REC);
    for d in dirs {
        let off = strs.len() as u32;
        strs.extend_from_slice(d.path.as_bytes());
        out.extend_from_slice(&off.to_le_bytes());
        out.extend_from_slice(&(d.path.len() as u32).to_le_bytes());
        out.extend_from_slice(&d.flags.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&d.mtime_ns.to_le_bytes());
        out.extend_from_slice(&d.ctime_ns.to_le_bytes());
    }
    out
}

pub struct Sections<'a> {
    data: &'a [u8],
    table: Vec<(Tag, usize, usize)>,
}

impl<'a> Sections<'a> {
    pub fn parse(data: &'a [u8]) -> Option<Sections<'a>> {
        if data.len() < 16 || &data[..8] != MAGIC {
            return None;
        }
        if rd_u32(data, 8) != crate::store::FORMAT_VERSION {
            return None;
        }
        let n = rd_u32(data, 12) as usize;
        let mut table = Vec::with_capacity(n);
        for i in 0..n {
            let at = 16 + i * 20;
            if at + 20 > data.len() {
                return None;
            }
            let tag = Tag(data[at..at + 4].try_into().ok()?);
            let off = rd_u64(data, at + 4) as usize;
            let len = rd_u64(data, at + 12) as usize;
            if off.checked_add(len)? > data.len() {
                return None;
            }
            table.push((tag, off, len));
        }
        Some(Sections { data, table })
    }

    pub fn get(&self, tag: Tag) -> &'a [u8] {
        self.table
            .iter()
            .find(|(t, _, _)| *t == tag)
            .map(|&(_, off, len)| &self.data[off..off + len])
            .unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        let mut buf = Vec::new();
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            put_varint(&mut buf, v);
        }
        let mut pos = 0;
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            assert_eq!(get_varint(&buf, &mut pos), Some(v));
        }
        assert_eq!(get_varint(&buf, &mut pos), None);
    }

    #[test]
    fn container_roundtrip() {
        let mut w = Writer::default();
        w.section(Tag::STRS, b"hello".to_vec());
        w.section(Tag::POST, vec![1, 2, 3]);
        let mut out = Vec::new();
        w.finish(&mut out).unwrap();
        let s = Sections::parse(&out).unwrap();
        assert_eq!(s.get(Tag::STRS), b"hello");
        assert_eq!(s.get(Tag::POST), &[1, 2, 3]);
        assert!(s.get(Tag::NFST).is_empty());
        assert!(Sections::parse(&out[..10]).is_none());
    }
}
