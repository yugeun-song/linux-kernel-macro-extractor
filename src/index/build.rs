use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use rayon::prelude::*;
use rustc_hash::FxHashSet;
use xxhash_rust::xxh3::xxh3_128;

use super::extract::{Extracted, extract};
use super::format::{DirRec, FileRec, Stat, Tag, Writer, encode_dirs, encode_files, put_varint};
use super::sources::SourceFile;
use super::{FileClass, FileId, USE_PREFIX, classify, flag, path_key};
use crate::store::IndexStats;
use crate::timing::Timer;
use crate::tree::{Layout, Root};

struct Processed {
    rec: FileRec,
    found: Extracted,
    bytes: u64,
    failed: bool,
}

fn read_stable(path: &Path, data: &mut Vec<u8>) -> std::io::Result<(Stat, bool)> {
    use std::io::Read;
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(path)?;
        data.clear();
        data.extend_from_slice(target.as_os_str().as_encoded_bytes());
        return Ok((Stat::of(&meta), true));
    }
    let mut file = std::fs::File::open(path)?;
    let mut before = Stat::of(&file.metadata()?);
    for _ in 0..3 {
        data.clear();
        file.read_to_end(data)?;
        let after = Stat::of(&file.metadata()?);
        if before == after {
            return Ok((after, false));
        }
        before = after;
        file = std::fs::File::open(path)?;
    }
    Ok((before, false))
}

thread_local! {
    static READ_BUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn process(layout: &Layout, f: &SourceFile, arches: &[String]) -> Option<Processed> {
    READ_BUF.with_borrow_mut(|data| {
        let done = process_with(layout, f, arches, data);
        if data.capacity() > 4 << 20 {
            *data = Vec::new();
        }
        done
    })
}

fn process_with(
    layout: &Layout,
    f: &SourceFile,
    arches: &[String],
    data: &mut Vec<u8>,
) -> Option<Processed> {
    let abs = layout.abs(f.root, &f.path);
    let (mut flags, arch) = classify(&f.path, f.root, arches);
    let (stat, is_link) = read_stable(&abs, data).ok()?;
    let class = FileClass::of_name(f.path.rsplit('/').next().unwrap_or(&f.path));
    let mut found = Extracted::default();
    let mut failed = false;
    if !is_link && class.is_preprocessed() {
        match catch_unwind(AssertUnwindSafe(|| {
            let mut tmp = Extracted::default();
            let guarded = extract(data, class, &mut tmp);
            (tmp, guarded)
        })) {
            Ok((mut tmp, guarded)) => {
                tmp.stored = Default::default();
                tmp.names.shrink_to_fit();
                tmp.posts.shrink_to_fit();
                found = tmp;
                if guarded {
                    flags |= flag::GUARDED;
                }
            }
            Err(_) => failed = true,
        }
    }
    Some(Processed {
        rec: FileRec {
            path: f.path.clone(),
            flags,
            arch,
            stat,
            hash: xxh3_128(data),
        },
        found,
        bytes: data.len() as u64,
        failed,
    })
}

pub struct Built {
    pub bytes: Vec<u8>,
    pub stats: IndexStats,
    pub failed: Vec<String>,
}

fn arch_table(layout: &Layout) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(layout.srctree.join("arch"))
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn dir_records(layout: &Layout, files: &[SourceFile]) -> Vec<DirRec> {
    let mut dirs: FxHashSet<(Root, String)> = FxHashSet::default();
    for f in files {
        let mut p = f.path.as_str();
        while let Some((parent, _)) = p.rsplit_once('/') {
            if !dirs.insert((f.root, parent.to_string())) {
                break;
            }
            p = parent;
        }
        dirs.insert((f.root, String::new()));
    }
    let mut list: Vec<(Root, String)> = dirs.into_iter().collect();
    list.sort();
    list.par_iter()
        .filter_map(|(root, rel)| {
            let meta = std::fs::metadata(layout.abs(*root, rel)).ok()?;
            let st = Stat::of(&meta);
            Some(DirRec {
                path: rel.clone(),
                flags: if *root == Root::Obj {
                    flag::OBJ_ROOT
                } else {
                    0
                },
                mtime_ns: st.mtime_ns,
                ctime_ns: st.ctime_ns,
            })
        })
        .collect()
}

type Entry<'a> = (&'a str, u32, u32, u32, u8);

pub fn build(
    layout: &Layout,
    list: &[SourceFile],
    all_files: &[SourceFile],
    first_id: FileId,
    tombstones: &[FileId],
) -> Built {
    let mut timer = Timer::new();
    let arches = arch_table(layout);
    let processed: Vec<(&SourceFile, Processed)> = list
        .par_iter()
        .filter_map(|f| process(layout, f, &arches).map(|p| (f, p)))
        .collect();
    timer.mark("read+hash+scan");
    let mut stats = IndexStats::default();
    let mut failed = Vec::new();
    for (sf, p) in &processed {
        stats.files += 1;
        if FileClass::of_name(sf.path.rsplit('/').next().unwrap_or(&sf.path)).is_preprocessed() {
            stats.source_files += 1;
            stats.source_bytes += p.bytes;
        }
        stats.definitions += p.found.defs;
        if p.failed {
            failed.push(sf.path.clone());
        }
    }
    let mut entries: Vec<Entry<'_>> = Vec::with_capacity(stats.definitions as usize);
    for (i, (_, p)) in processed.iter().enumerate() {
        let id = first_id + i as u32;
        for rp in &p.found.posts {
            let name = &p.found.names[rp.name_at as usize..(rp.name_at + rp.name_len) as usize];
            entries.push((name, id, rp.line, rp.off, rp.kd));
        }
    }
    timer.mark("gather postings");
    entries.par_sort_unstable_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    timer.mark("sort postings");
    let groups: Vec<&[Entry<'_>]> = entries.chunk_by(|a, b| a.0 == b.0).collect();
    stats.names = groups
        .iter()
        .filter(|g| !g[0].0.starts_with(USE_PREFIX))
        .count() as u64;
    let parts = (rayon::current_num_threads() * 2).clamp(1, 64);
    let per = groups.len().div_ceil(parts).max(1);
    let chunks: Vec<(Vec<u8>, Vec<u8>, String)> = groups
        .par_chunks(per)
        .map(|chunk| {
            let mut posts = Vec::with_capacity(chunk.len() * 8);
            let mut names = fst::MapBuilder::memory();
            for group in chunk {
                let at = posts.len() as u64;
                put_varint(&mut posts, group.len() as u64);
                let mut prev = first_id;
                for e in group.iter() {
                    put_varint(&mut posts, u64::from(e.1 - prev));
                    prev = e.1;
                    put_varint(&mut posts, u64::from(e.2));
                    put_varint(&mut posts, u64::from(e.3));
                    posts.push(e.4);
                }
                let _ = names.insert(group[0].0, at);
            }
            (
                names.into_inner().unwrap_or_default(),
                posts,
                chunk[0][0].0.to_string(),
            )
        })
        .collect();
    drop(groups);
    drop(entries);
    timer.mark("names fst + postings");
    let mut paths = fst::MapBuilder::memory();
    let mut files: Vec<FileRec> = Vec::with_capacity(processed.len());
    for (i, (sf, p)) in processed.into_iter().enumerate() {
        let _ = paths.insert(path_key(sf.root, &sf.path), u64::from(first_id) + i as u64);
        files.push(p.rec);
    }
    let mut strs = Vec::new();
    let file_sec = encode_files(&files, &mut strs);
    let dir_sec = encode_dirs(&dir_records(layout, all_files), &mut strs);
    let mut nfst = Vec::new();
    let mut posts = Vec::new();
    let mut chnk = Vec::with_capacity(chunks.len() * super::format::CHUNK_REC);
    for (fst_bytes, post_bytes, first) in chunks {
        chnk.extend_from_slice(&(strs.len() as u32).to_le_bytes());
        chnk.extend_from_slice(&(first.len() as u32).to_le_bytes());
        strs.extend_from_slice(first.as_bytes());
        chnk.extend_from_slice(&(nfst.len() as u64).to_le_bytes());
        chnk.extend_from_slice(&(fst_bytes.len() as u64).to_le_bytes());
        chnk.extend_from_slice(&(posts.len() as u64).to_le_bytes());
        nfst.extend_from_slice(&fst_bytes);
        while nfst.len() % 8 != 0 {
            nfst.push(0);
        }
        posts.extend_from_slice(&post_bytes);
    }
    let mut w = Writer::default();
    w.section(Tag::STRS, strs);
    w.section(Tag::FILE, file_sec);
    w.section(Tag::DIRS, dir_sec);
    w.section(Tag::NFST, nfst);
    w.section(Tag::CHNK, chnk);
    w.section(Tag::POST, posts);
    w.section(Tag::PFST, paths.into_inner().unwrap_or_default());
    w.section(Tag::ARCH, arches.join("\n").into_bytes());
    if !tombstones.is_empty() {
        w.section(
            Tag::TOMB,
            tombstones.iter().flat_map(|t| t.to_le_bytes()).collect(),
        );
    }
    timer.mark("paths fst + dirs");
    let mut bytes = Vec::with_capacity(w.size() as usize);
    let _ = w.finish(&mut bytes);
    timer.mark("encode");
    Built {
        bytes,
        stats,
        failed,
    }
}

pub struct OverlayPlan {
    pub files: Vec<SourceFile>,
    pub tombstones: Vec<FileId>,
    pub all_files: Vec<SourceFile>,
}

pub fn plan_overlay(old: &super::Index, changes: &super::verify::Changes) -> Option<OverlayPlan> {
    let base_count = old.base_file_count();
    let gone: FxHashSet<&SourceFile> = changes
        .modified
        .iter()
        .chain(&changes.touched)
        .chain(&changes.deleted)
        .collect();
    let deleted: FxHashSet<&SourceFile> = changes.deleted.iter().collect();
    let mut tombstones: FxHashSet<FileId> = old.tombstones().collect();
    for sf in &gone {
        if let Some(id) = old.file_id(sf.root, &sf.path)
            && id < base_count
        {
            tombstones.insert(id);
        }
    }
    let mut overlay: FxHashSet<SourceFile> = old
        .files()
        .filter(|f| f.id >= base_count)
        .map(|f| SourceFile {
            root: f.root(),
            path: f.path.to_string(),
        })
        .filter(|sf| !deleted.contains(sf))
        .collect();
    overlay.extend(changes.modified.iter().cloned());
    overlay.extend(changes.touched.iter().cloned());
    overlay.extend(changes.added.iter().cloned());
    let limit = (base_count as usize / 33).max(2000);
    if overlay.len() > limit {
        return None;
    }
    let mut all: Vec<SourceFile> = old
        .files()
        .filter(|f| f.id < base_count && !tombstones.contains(&f.id))
        .map(|f| SourceFile {
            root: f.root(),
            path: f.path.to_string(),
        })
        .collect();
    all.extend(overlay.iter().cloned());
    all.sort_by_key(|a| path_key(a.root, &a.path));
    let mut files: Vec<SourceFile> = overlay.into_iter().collect();
    files.sort_by_key(|a| path_key(a.root, &a.path));
    let mut tombstones: Vec<FileId> = tombstones.into_iter().collect();
    tombstones.sort_unstable();
    Some(OverlayPlan {
        files,
        tombstones,
        all_files: all,
    })
}
