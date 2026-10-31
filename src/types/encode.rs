use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};

use crate::index::format::{Sections, Tag, Writer, rd_u16, rd_u32, rd_u64};

pub type Id = u128;

pub const MAGIC: &[u8; 8] = b"KMXTYPE\x01";
pub const VERSION: u32 = 1;

const STRS: Tag = Tag(*b"STRS");
const FILE: Tag = Tag(*b"FILE");
const DEFS: Tag = Tag(*b"DEFS");
const MEMB: Tag = Tag(*b"MEMB");
const ENUM: Tag = Tag(*b"ENUM");
const DERV: Tag = Tag(*b"DERV");
const BASE: Tag = Tag(*b"BASE");
const ENTS: Tag = Tag(*b"ENTS");
const NAMX: Tag = Tag(*b"NAMX");
const FUNC: Tag = Tag(*b"FUNC");
const PARM: Tag = Tag(*b"PARM");
const META: Tag = Tag(*b"META");

const DEF_REC: usize = 36;
const MEMB_REC: usize = 24;
const ENUM_REC: usize = 16;
const DERV_REC: usize = 16;
const BASE_REC: usize = 12;
const ENT_REC: usize = 20;
const NAMX_REC: usize = 12;
const FUNC_REC: usize = 16;

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum DefKind {
    Struct = 1,
    Union = 2,
    Enum = 3,
    Typedef = 4,
}

impl DefKind {
    fn from_u8(v: u8) -> Option<DefKind> {
        Some(match v {
            1 => DefKind::Struct,
            2 => DefKind::Union,
            3 => DefKind::Enum,
            4 => DefKind::Typedef,
            _ => return None,
        })
    }

    pub fn keyword(self) -> &'static str {
        match self {
            DefKind::Struct => "struct",
            DefKind::Union => "union",
            DefKind::Enum => "enum",
            DefKind::Typedef => "typedef",
        }
    }
}

pub struct Member {
    pub name: String,
    pub ty: Id,
    pub offset: Option<u64>,
    pub size: Option<u64>,
    pub bit_size: u64,
    pub bit_offset: Option<u64>,
}

pub struct Def {
    pub kind: DefKind,
    pub name: String,
    pub file: String,
    pub line: u32,
    pub col: u32,
    pub size: Option<u64>,
    pub members: Vec<Member>,
    pub enumerators: Vec<(String, i128)>,
    pub target: Id,
    pub signed_enum: bool,
    pub cus: u32,
    pub conflicts: u8,
}

pub const TYPE_CONFLICT: u8 = 1;
pub const LAYOUT_CONFLICT: u8 = 8;
pub const SIZE_CONFLICT: u8 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Base,
    Struct,
    Union,
    Enum,
    Typedef,
    Anon,
    Pointer,
    Const,
    Volatile,
    Restrict,
    Atomic,
    Array,
    Function,
}

pub struct Node {
    pub kind: NodeKind,
    pub name: String,
    pub target: Id,
    pub count: u64,
    pub enc: u8,
    pub anon: Option<Id>,
    pub params: Vec<Id>,
}

pub struct Entity {
    pub name: String,
    pub file: String,
    pub line: u32,
    pub ty: Id,
    pub function: bool,
}

pub struct Raw {
    pub defs: Vec<(Id, Def)>,
    pub nodes: FxHashMap<Id, Node>,
    pub entities: Vec<Entity>,
    pub address_size: u8,
    pub units: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Meta {
    pub address_size: u8,
    pub units: u64,
    pub failed_units: u64,
    pub products: Vec<String>,
    pub failed_products: Vec<(String, String)>,
    pub ignored_relocations: u64,
    pub conflicts: u64,
    pub elapsed_ms: u64,
}

const T_SHIFT: u32 = 28;
const T_MASK: u32 = (1 << T_SHIFT) - 1;
pub const T_VOID: u32 = 0;
const T_BASE: u32 = 1;
const T_STRUCT: u32 = 2;
const T_UNION: u32 = 3;
const T_ENUM: u32 = 4;
const T_TYPEDEF: u32 = 5;
const T_ANON: u32 = 6;
const T_DERIVED: u32 = 7;
const T_FUNCTION: u32 = 8;
pub const T_UNKNOWN: u32 = 15 << T_SHIFT;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DerivedKind {
    Pointer = 1,
    Const = 2,
    Volatile = 3,
    Restrict = 4,
    Atomic = 5,
    Array = 6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TRef<'a> {
    Void,
    Base {
        name: &'a str,
        size: u64,
        enc: u8,
    },
    Named(DefKind, &'a str),
    Anon(u32),
    Derived {
        kind: DerivedKind,
        target: u32,
        count: Option<u64>,
    },
    Function(FuncView),
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FuncView {
    pub ret: u32,
    first: u32,
    count: u32,
    pub variadic: bool,
    pub prototyped: bool,
}

struct Strings {
    blob: Vec<u8>,
    at: FxHashMap<String, u32>,
}

impl Strings {
    fn new() -> Strings {
        Strings {
            blob: vec![0],
            at: FxHashMap::default(),
        }
    }

    fn put(&mut self, s: &str) -> u32 {
        if s.is_empty() {
            return 0;
        }
        if let Some(&o) = self.at.get(s) {
            return o;
        }
        let o = self.blob.len() as u32;
        self.blob.extend_from_slice(s.as_bytes());
        self.blob.push(0);
        self.at.insert(s.to_string(), o);
        o
    }
}

#[derive(Default)]
struct Files {
    bytes: Vec<u8>,
    at: FxHashMap<String, u32>,
}

impl Files {
    fn id(&mut self, f: &str, strs: &mut Strings) -> u32 {
        if let Some(&i) = self.at.get(f) {
            return i;
        }
        let i = (self.bytes.len() / 4) as u32;
        self.bytes.extend_from_slice(&strs.put(f).to_le_bytes());
        self.at.insert(f.to_string(), i);
        i
    }
}

struct Refs<'r> {
    raw: &'r Raw,
    def_at: FxHashMap<Id, u32>,
    memo: FxHashMap<Id, u32>,
    base: Vec<u8>,
    base_at: FxHashMap<Id, u32>,
    derived: Vec<u8>,
    funcs: Vec<u8>,
    parms: Vec<u8>,
}

impl Refs<'_> {
    fn tref(&mut self, id: Id, strs: &mut Strings, depth: u32) -> u32 {
        if id == 0 {
            return T_VOID;
        }
        if let Some(&t) = self.memo.get(&id) {
            return t;
        }
        if depth > 64 {
            return T_UNKNOWN;
        }
        let Some(node) = self.raw.nodes.get(&id) else {
            return T_UNKNOWN;
        };
        let named = |tag: u32, strs: &mut Strings| {
            let o = strs.put(&node.name);
            if o > T_MASK {
                T_UNKNOWN
            } else {
                tag << T_SHIFT | o
            }
        };
        let t = match node.kind {
            NodeKind::Base => {
                let idx = match self.base_at.get(&id) {
                    Some(&i) => i,
                    None => {
                        let i = (self.base.len() / BASE_REC) as u32;
                        let name = strs.put(&node.name);
                        self.base.extend_from_slice(&name.to_le_bytes());
                        self.base
                            .extend_from_slice(&(node.count as u32).to_le_bytes());
                        self.base.extend_from_slice(&[node.enc, 0, 0, 0]);
                        self.base_at.insert(id, i);
                        i
                    }
                };
                T_BASE << T_SHIFT | idx
            }
            NodeKind::Struct => named(T_STRUCT, strs),
            NodeKind::Union => named(T_UNION, strs),
            NodeKind::Enum => named(T_ENUM, strs),
            NodeKind::Typedef => named(T_TYPEDEF, strs),
            NodeKind::Anon => match node.anon.and_then(|k| self.def_at.get(&k)) {
                Some(&i) => T_ANON << T_SHIFT | i,
                None => T_UNKNOWN,
            },
            NodeKind::Function => {
                let ret = self.tref(node.target, strs, depth + 1);
                let params: Vec<u32> = node
                    .params
                    .iter()
                    .map(|&p| self.tref(p, strs, depth + 1))
                    .collect();
                let i = (self.funcs.len() / FUNC_REC) as u32;
                let first = (self.parms.len() / 4) as u32;
                for p in &params {
                    self.parms.extend_from_slice(&p.to_le_bytes());
                }
                self.funcs.extend_from_slice(&ret.to_le_bytes());
                self.funcs.extend_from_slice(&first.to_le_bytes());
                self.funcs
                    .extend_from_slice(&(params.len() as u32).to_le_bytes());
                self.funcs
                    .extend_from_slice(&u32::from(node.enc).to_le_bytes());
                T_FUNCTION << T_SHIFT | i
            }
            kind => {
                let target = self.tref(node.target, strs, depth + 1);
                let k = match kind {
                    NodeKind::Pointer => DerivedKind::Pointer,
                    NodeKind::Const => DerivedKind::Const,
                    NodeKind::Volatile => DerivedKind::Volatile,
                    NodeKind::Restrict => DerivedKind::Restrict,
                    NodeKind::Atomic => DerivedKind::Atomic,
                    _ => DerivedKind::Array,
                };
                if target == T_UNKNOWN && k != DerivedKind::Pointer {
                    T_UNKNOWN
                } else {
                    let i = (self.derived.len() / DERV_REC) as u32;
                    self.derived.extend_from_slice(&[k as u8, 0, 0, 0]);
                    self.derived.extend_from_slice(&target.to_le_bytes());
                    self.derived.extend_from_slice(&node.count.to_le_bytes());
                    T_DERIVED << T_SHIFT | i
                }
            }
        };
        self.memo.insert(id, t);
        t
    }
}

pub fn encode(raw: &Raw, meta: &Meta) -> Vec<u8> {
    let mut strs = Strings::new();
    let mut files = Files::default();
    let mut refs = Refs {
        raw,
        def_at: raw
            .defs
            .iter()
            .enumerate()
            .map(|(i, (k, _))| (*k, i as u32))
            .collect(),
        memo: FxHashMap::default(),
        base: Vec::new(),
        base_at: FxHashMap::default(),
        derived: Vec::new(),
        funcs: Vec::new(),
        parms: Vec::new(),
    };
    let mut defs = Vec::with_capacity(raw.defs.len() * DEF_REC);
    let mut members = Vec::new();
    let mut enums = Vec::new();
    let mut names: Vec<(u32, u8, u32)> = Vec::new();
    for (i, (_, d)) in raw.defs.iter().enumerate() {
        let name = strs.put(&d.name);
        let file = files.id(&d.file, &mut strs);
        let size = d.size.and_then(|s| u32::try_from(s).ok());
        let mut flags = d.conflicts & (TYPE_CONFLICT | LAYOUT_CONFLICT | SIZE_CONFLICT);
        if size.is_some() {
            flags |= 2;
        }
        if d.signed_enum {
            flags |= 4;
        }
        let (a, b, under) = match d.kind {
            DefKind::Struct | DefKind::Union => {
                let first = (members.len() / MEMB_REC) as u32;
                for m in &d.members {
                    let ty = refs.tref(m.ty, &mut strs, 0);
                    members.extend_from_slice(&strs.put(&m.name).to_le_bytes());
                    members.extend_from_slice(&ty.to_le_bytes());
                    let clamp =
                        |v: Option<u64>| v.and_then(|x| u32::try_from(x).ok()).unwrap_or(NONE);
                    members.extend_from_slice(&clamp(m.offset).to_le_bytes());
                    members.extend_from_slice(&clamp(m.size).to_le_bytes());
                    let bit_offset = if m.bit_size > 0 {
                        clamp(m.bit_offset)
                    } else {
                        NONE
                    };
                    members.extend_from_slice(&bit_offset.to_le_bytes());
                    members.extend_from_slice(&(m.bit_size as u32).to_le_bytes());
                }
                (first, d.members.len() as u32, T_VOID)
            }
            DefKind::Enum => {
                let first = (enums.len() / ENUM_REC) as u32;
                for (n, v) in &d.enumerators {
                    let o = strs.put(n);
                    names.push((o, 5, (enums.len() / ENUM_REC) as u32));
                    enums.extend_from_slice(&o.to_le_bytes());
                    enums.extend_from_slice(&(i as u32).to_le_bytes());
                    enums.extend_from_slice(&(*v as u64).to_le_bytes());
                }
                let under = refs.tref(d.target, &mut strs, 0);
                (first, d.enumerators.len() as u32, under)
            }
            DefKind::Typedef => (refs.tref(d.target, &mut strs, 0), 0, T_VOID),
        };
        if name != 0 {
            names.push((name, d.kind as u8, i as u32));
        }
        defs.push(d.kind as u8);
        defs.push(flags);
        defs.extend_from_slice(&(d.col.min(u32::from(u16::MAX)) as u16).to_le_bytes());
        defs.extend_from_slice(&name.to_le_bytes());
        defs.extend_from_slice(&file.to_le_bytes());
        defs.extend_from_slice(&d.line.to_le_bytes());
        defs.extend_from_slice(&size.unwrap_or(NONE).to_le_bytes());
        defs.extend_from_slice(&a.to_le_bytes());
        defs.extend_from_slice(&b.to_le_bytes());
        defs.extend_from_slice(&d.cus.to_le_bytes());
        defs.extend_from_slice(&under.to_le_bytes());
    }
    let mut ents = Vec::with_capacity(raw.entities.len() * ENT_REC);
    for (i, e) in raw.entities.iter().enumerate() {
        let name = strs.put(&e.name);
        let file = files.id(&e.file, &mut strs);
        let ty = refs.tref(e.ty, &mut strs, 0);
        names.push((name, if e.function { 6 } else { 7 }, i as u32));
        ents.extend_from_slice(&name.to_le_bytes());
        ents.extend_from_slice(&file.to_le_bytes());
        ents.extend_from_slice(&e.line.to_le_bytes());
        ents.extend_from_slice(&ty.to_le_bytes());
        ents.extend_from_slice(&u32::from(e.function).to_le_bytes());
    }
    let blob = &strs.blob;
    let text = |o: u32| -> &[u8] {
        let s = &blob[o as usize..];
        &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())]
    };
    names.sort_by(|a, b| {
        text(a.0)
            .cmp(text(b.0))
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
    });
    let mut namx = Vec::with_capacity(names.len() * NAMX_REC);
    for (o, k, i) in &names {
        namx.extend_from_slice(&o.to_le_bytes());
        namx.extend_from_slice(&[*k, 0, 0, 0]);
        namx.extend_from_slice(&i.to_le_bytes());
    }
    let mut w = Writer::default();
    w.section(STRS, strs.blob.clone());
    w.section(FILE, files.bytes);
    w.section(DEFS, defs);
    w.section(MEMB, members);
    w.section(ENUM, enums);
    w.section(DERV, refs.derived);
    w.section(BASE, refs.base);
    w.section(ENTS, ents);
    w.section(NAMX, namx);
    w.section(FUNC, refs.funcs);
    w.section(PARM, refs.parms);
    w.section(META, serde_json::to_vec(meta).unwrap_or_default());
    let mut out = Vec::new();
    let _ = w.finish_as(MAGIC, VERSION, &mut out);
    out
}

pub struct Table {
    bytes: Bytes,
    strs: (usize, usize),
    files: (usize, usize),
    defs: (usize, usize),
    members: (usize, usize),
    enums: (usize, usize),
    derived: (usize, usize),
    base: (usize, usize),
    ents: (usize, usize),
    namx: (usize, usize),
    funcs: (usize, usize),
    parms: (usize, usize),
    pub meta: Meta,
}

enum Bytes {
    Map(memmap2::Mmap),
    Vec(Vec<u8>),
}

impl Bytes {
    fn get(&self) -> &[u8] {
        match self {
            Bytes::Map(m) => m,
            Bytes::Vec(v) => v,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DefView<'a> {
    pub index: u32,
    pub kind: DefKind,
    pub name: &'a str,
    pub file: &'a str,
    pub line: u32,
    pub col: u32,
    pub size: Option<u64>,
    pub conflicts: u8,
    pub signed_enum: bool,
    pub cus: u32,
    pub target: u32,
    pub under: u32,
    first: u32,
    count: u32,
}

impl DefView<'_> {
    pub fn layout_agrees(&self) -> bool {
        self.conflicts & (LAYOUT_CONFLICT | SIZE_CONFLICT) == 0
    }

    pub fn types_agree(&self) -> bool {
        self.conflicts == 0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct MemberView<'a> {
    pub name: &'a str,
    pub ty: u32,
    pub offset: Option<u64>,
    pub size: Option<u64>,
    pub bit_offset: Option<u64>,
    pub bit_size: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct EntityView<'a> {
    pub name: &'a str,
    pub file: &'a str,
    pub line: u32,
    pub ty: u32,
    pub function: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct EnumeratorView<'a> {
    pub name: &'a str,
    pub def: u32,
    pub value: i128,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum NameKind {
    Def(DefKind),
    Enumerator,
    Function,
    Variable,
}

impl Table {
    pub fn open(path: &std::path::Path) -> Option<Table> {
        let file = std::fs::File::open(path).ok()?;
        let map = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        Table::from_bytes(Bytes::Map(map))
    }

    pub fn from_vec(v: Vec<u8>) -> Option<Table> {
        Table::from_bytes(Bytes::Vec(v))
    }

    fn from_bytes(bytes: Bytes) -> Option<Table> {
        let data = bytes.get();
        let s = Sections::parse_as(data, MAGIC, VERSION)?;
        let base = data.as_ptr() as usize;
        let span = |t: Tag| {
            let b = s.get(t);
            (b.as_ptr() as usize - base, b.len())
        };
        let meta = serde_json::from_slice(s.get(META)).ok()?;
        let t = Table {
            strs: span(STRS),
            files: span(FILE),
            defs: span(DEFS),
            members: span(MEMB),
            enums: span(ENUM),
            derived: span(DERV),
            base: span(BASE),
            ents: span(ENTS),
            namx: span(NAMX),
            funcs: span(FUNC),
            parms: span(PARM),
            meta,
            bytes: Bytes::Vec(Vec::new()),
        };
        Some(Table { bytes, ..t })
    }

    fn sec(&self, s: (usize, usize)) -> &[u8] {
        &self.bytes.get()[s.0..s.0 + s.1]
    }

    fn str_at(&self, o: u32) -> &str {
        let s = self.sec(self.strs);
        let Some(tail) = s.get(o as usize..) else {
            return "";
        };
        let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
        std::str::from_utf8(&tail[..end]).unwrap_or("")
    }

    fn file_at(&self, i: u32) -> &str {
        let f = self.sec(self.files);
        let at = i as usize * 4;
        if at + 4 > f.len() {
            return "";
        }
        self.str_at(rd_u32(f, at))
    }

    pub fn def_count(&self) -> usize {
        self.defs.1 / DEF_REC
    }

    pub fn def(&self, i: u32) -> Option<DefView<'_>> {
        let d = self.sec(self.defs);
        let at = i as usize * DEF_REC;
        if at + DEF_REC > d.len() {
            return None;
        }
        let r = &d[at..at + DEF_REC];
        let flags = r[1];
        let size = rd_u32(r, 16);
        Some(DefView {
            index: i,
            kind: DefKind::from_u8(r[0])?,
            col: u32::from(rd_u16(r, 2)),
            name: self.str_at(rd_u32(r, 4)),
            file: self.file_at(rd_u32(r, 8)),
            line: rd_u32(r, 12),
            size: (flags & 2 != 0 && flags & SIZE_CONFLICT == 0).then_some(u64::from(size)),
            conflicts: flags & (TYPE_CONFLICT | LAYOUT_CONFLICT | SIZE_CONFLICT),
            signed_enum: flags & 4 != 0,
            first: rd_u32(r, 20),
            count: rd_u32(r, 24),
            target: rd_u32(r, 20),
            cus: rd_u32(r, 28),
            under: rd_u32(r, 32),
        })
    }

    pub fn members(&self, d: &DefView<'_>) -> impl Iterator<Item = MemberView<'_>> + '_ {
        let m = self.sec(self.members);
        let (first, count) = match d.kind {
            DefKind::Struct | DefKind::Union => (d.first as usize, d.count as usize),
            _ => (0, 0),
        };
        (first..first + count).filter_map(move |i| {
            let at = i * MEMB_REC;
            let r = m.get(at..at + MEMB_REC)?;
            let opt = |v: u32| (v != NONE).then_some(u64::from(v));
            Some(MemberView {
                name: self.str_at(rd_u32(r, 0)),
                ty: rd_u32(r, 4),
                offset: opt(rd_u32(r, 8)),
                size: opt(rd_u32(r, 12)),
                bit_offset: opt(rd_u32(r, 16)),
                bit_size: rd_u32(r, 20),
            })
        })
    }

    pub fn enumerators(&self, d: &DefView<'_>) -> impl Iterator<Item = EnumeratorView<'_>> + '_ {
        let (first, count) = match d.kind {
            DefKind::Enum => (d.first as usize, d.count as usize),
            _ => (0, 0),
        };
        (first..first + count).filter_map(move |i| self.enumerator(i as u32))
    }

    pub fn enumerator(&self, i: u32) -> Option<EnumeratorView<'_>> {
        let e = self.sec(self.enums);
        let at = i as usize * ENUM_REC;
        let r = e.get(at..at + ENUM_REC)?;
        let def = rd_u32(r, 4);
        let raw = rd_u64(r, 8);
        let signed = self.def(def).is_some_and(|d| d.signed_enum);
        Some(EnumeratorView {
            name: self.str_at(rd_u32(r, 0)),
            def,
            value: if signed {
                i128::from(raw as i64)
            } else {
                i128::from(raw)
            },
        })
    }

    pub fn entity(&self, i: u32) -> Option<EntityView<'_>> {
        let e = self.sec(self.ents);
        let at = i as usize * ENT_REC;
        let r = e.get(at..at + ENT_REC)?;
        Some(EntityView {
            name: self.str_at(rd_u32(r, 0)),
            file: self.file_at(rd_u32(r, 4)),
            line: rd_u32(r, 8),
            ty: rd_u32(r, 12),
            function: rd_u32(r, 16) != 0,
        })
    }

    pub fn tref(&self, t: u32) -> TRef<'_> {
        let idx = t & T_MASK;
        match t >> T_SHIFT {
            T_VOID if t == T_VOID => TRef::Void,
            T_BASE => {
                let b = self.sec(self.base);
                let at = idx as usize * BASE_REC;
                match b.get(at..at + BASE_REC) {
                    Some(r) => TRef::Base {
                        name: self.str_at(rd_u32(r, 0)),
                        size: u64::from(rd_u32(r, 4)),
                        enc: r[8],
                    },
                    None => TRef::Unknown,
                }
            }
            T_STRUCT => TRef::Named(DefKind::Struct, self.str_at(idx)),
            T_UNION => TRef::Named(DefKind::Union, self.str_at(idx)),
            T_ENUM => TRef::Named(DefKind::Enum, self.str_at(idx)),
            T_TYPEDEF => TRef::Named(DefKind::Typedef, self.str_at(idx)),
            T_ANON => TRef::Anon(idx),
            T_DERIVED => {
                let d = self.sec(self.derived);
                let at = idx as usize * DERV_REC;
                let Some(r) = d.get(at..at + DERV_REC) else {
                    return TRef::Unknown;
                };
                let kind = match r[0] {
                    1 => DerivedKind::Pointer,
                    2 => DerivedKind::Const,
                    3 => DerivedKind::Volatile,
                    4 => DerivedKind::Restrict,
                    5 => DerivedKind::Atomic,
                    6 => DerivedKind::Array,
                    _ => return TRef::Unknown,
                };
                let count = rd_u64(r, 8);
                TRef::Derived {
                    kind,
                    target: rd_u32(r, 4),
                    count: (count != u64::MAX).then_some(count),
                }
            }
            T_FUNCTION => {
                let f = self.sec(self.funcs);
                let at = idx as usize * FUNC_REC;
                match f.get(at..at + FUNC_REC) {
                    Some(r) => {
                        let flags = rd_u32(r, 12);
                        TRef::Function(FuncView {
                            ret: rd_u32(r, 0),
                            first: rd_u32(r, 4),
                            count: rd_u32(r, 8),
                            variadic: flags & 1 != 0,
                            prototyped: flags & 2 != 0,
                        })
                    }
                    None => TRef::Unknown,
                }
            }
            _ => TRef::Unknown,
        }
    }

    pub fn params(&self, f: &FuncView) -> impl Iterator<Item = u32> + '_ {
        let p = self.sec(self.parms);
        let (first, count) = (f.first as usize, f.count as usize);
        (first..first + count).filter_map(move |i| p.get(i * 4..i * 4 + 4).map(|b| rd_u32(b, 0)))
    }

    pub fn bases(&self) -> impl Iterator<Item = (&str, u64, u8)> + '_ {
        let b = self.sec(self.base);
        b.as_chunks::<BASE_REC>()
            .0
            .iter()
            .map(move |r| (self.str_at(rd_u32(r, 0)), u64::from(rd_u32(r, 4)), r[8]))
    }

    pub fn named(&self, name: &str) -> impl Iterator<Item = (NameKind, u32)> + '_ {
        let n = self.sec(self.namx);
        let count = n.len() / NAMX_REC;
        let key = |i: usize| self.str_at(rd_u32(n, i * NAMX_REC));
        let mut lo = 0;
        let mut hi = count;
        while lo < hi {
            let mid = (lo + hi) / 2;
            if key(mid).as_bytes() < name.as_bytes() {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let name = name.to_string();
        (lo..count)
            .take_while(move |&i| key(i) == name)
            .filter_map(move |i| {
                let at = i * NAMX_REC;
                let kind = match n[at + 4] {
                    k @ 1..=4 => NameKind::Def(DefKind::from_u8(k)?),
                    5 => NameKind::Enumerator,
                    6 => NameKind::Function,
                    7 => NameKind::Variable,
                    _ => return None,
                };
                Some((kind, rd_u32(n, at + 8)))
            })
    }
}
