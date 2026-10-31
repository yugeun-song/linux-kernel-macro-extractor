use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use gimli::{AttributeValue, EndianSlice, Reader, RunTimeEndian, UnitOffset, constants as dw};
use object::{Object, ObjectSection, ObjectSymbol, RelocationKind, RelocationTarget};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use xxhash_rust::xxh3::Xxh3;

use super::encode::{self, Def, DefKind, Member, Node, NodeKind, Raw};

const SHARDS: usize = 64;

pub type Id = u128;

const VOID: Id = 0;
const UNKNOWN: Id = 1;

#[derive(Default)]
pub struct ProductReport {
    pub path: PathBuf,
    pub units: u32,
    pub failed_units: u32,
    pub ignored_relocations: u32,
    pub error: Option<String>,
}

pub struct Extracted {
    pub raw: Raw,
    pub products: Vec<ProductReport>,
}

struct Sec<'d> {
    data: Cow<'d, [u8]>,
    ignored: u32,
}

const SECTIONS: [(gimli::SectionId, &str); 7] = [
    (gimli::SectionId::DebugAbbrev, ".debug_abbrev"),
    (gimli::SectionId::DebugInfo, ".debug_info"),
    (gimli::SectionId::DebugStr, ".debug_str"),
    (gimli::SectionId::DebugStrOffsets, ".debug_str_offsets"),
    (gimli::SectionId::DebugLine, ".debug_line"),
    (gimli::SectionId::DebugLineStr, ".debug_line_str"),
    (gimli::SectionId::DebugAddr, ".debug_addr"),
];

fn load_section<'d>(file: &object::File<'d>, name: &str) -> Result<Option<Sec<'d>>, String> {
    let Some(section) = file.section_by_name(name) else {
        return Ok(None);
    };
    let data = section
        .uncompressed_data()
        .map_err(|e| format!("{name}: {e}"))?;
    let mut ignored = 0u32;
    if file.kind() != object::ObjectKind::Relocatable {
        return Ok(Some(Sec { data, ignored }));
    }
    let mut bytes = data.into_owned();
    let little = file.is_little_endian();
    for (offset, rel) in section.relocations() {
        let at = offset as usize;
        let size = rel.size();
        let target = match rel.target() {
            RelocationTarget::Symbol(i) => file.symbol_by_index(i).ok().map(|s| s.address()),
            RelocationTarget::Section(i) => file.section_by_index(i).ok().map(|s| {
                if s.kind() == object::SectionKind::Debug {
                    0
                } else {
                    s.address()
                }
            }),
            _ => None,
        };
        let supported = rel.kind() == RelocationKind::Absolute
            && rel.encoding() == object::RelocationEncoding::Generic
            && (size == 32 || size == 64);
        let width = usize::from(size / 8);
        let (Some(s), true) = (target, supported) else {
            if rel.kind() != RelocationKind::None {
                ignored += 1;
            }
            continue;
        };
        let Some(field) = bytes.get_mut(at..at + width) else {
            ignored += 1;
            continue;
        };
        let implicit = if rel.has_implicit_addend() {
            let mut buf = [0u8; 8];
            if little {
                buf[..width].copy_from_slice(field);
                u64::from_le_bytes(buf)
            } else {
                buf[8 - width..].copy_from_slice(field);
                u64::from_be_bytes(buf)
            }
        } else {
            0
        };
        let value = s.wrapping_add(rel.addend() as u64).wrapping_add(implicit);
        let encoded = if little {
            value.to_le_bytes()
        } else {
            value.to_be_bytes()
        };
        if little {
            field.copy_from_slice(&encoded[..width]);
        } else {
            field.copy_from_slice(&encoded[8 - width..]);
        }
    }
    Ok(Some(Sec {
        data: Cow::Owned(bytes),
        ignored,
    }))
}

type R<'d> = EndianSlice<'d, RunTimeEndian>;

struct Hasher(Xxh3);

impl Hasher {
    fn new(tag: &[u8]) -> Hasher {
        let mut h = Xxh3::new();
        h.update(tag);
        Hasher(h)
    }

    fn bytes(mut self, b: &[u8]) -> Self {
        self.0.update(&(b.len() as u64).to_le_bytes());
        self.0.update(b);
        self
    }

    fn u64(mut self, v: u64) -> Self {
        self.0.update(&v.to_le_bytes());
        self
    }

    fn id(mut self, v: Id) -> Self {
        self.0.update(&v.to_le_bytes());
        self
    }

    fn finish(self) -> Id {
        let v = self.0.digest128();
        if v <= UNKNOWN { v + 2 } else { v }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum K {
    Base,
    Struct,
    Union,
    Enum,
    Typedef,
    Ptr,
    Const,
    Volatile,
    Restrict,
    Atomic,
    Array,
    Func,
    Other,
}

struct LMember<'d> {
    name: Option<&'d [u8]>,
    target: Option<usize>,
    offset: Option<u64>,
    bit_size: Option<u64>,
    bit_offset: Option<u64>,
}

struct LType<'d> {
    k: K,
    name: Option<&'d [u8]>,
    size: Option<u64>,
    file: Option<u64>,
    line: u64,
    col: u64,
    target: Option<usize>,
    enc: u8,
    decl: bool,
    members: Vec<LMember<'d>>,
    enums: Vec<(&'d [u8], u64, u8)>,
    dims: Vec<Option<u64>>,
    signed_enum: bool,
    params: Vec<usize>,
    variadic: bool,
    prototyped: bool,
}

struct LEntity<'d> {
    name: &'d [u8],
    file: Option<u64>,
    line: u64,
    target: Option<usize>,
    function: bool,
}

const NO_TARGET: usize = usize::MAX;

fn kind_of(tag: gimli::DwTag) -> Option<K> {
    Some(match tag {
        dw::DW_TAG_base_type => K::Base,
        dw::DW_TAG_structure_type | dw::DW_TAG_class_type => K::Struct,
        dw::DW_TAG_union_type => K::Union,
        dw::DW_TAG_enumeration_type => K::Enum,
        dw::DW_TAG_typedef => K::Typedef,
        dw::DW_TAG_pointer_type => K::Ptr,
        dw::DW_TAG_const_type => K::Const,
        dw::DW_TAG_volatile_type => K::Volatile,
        dw::DW_TAG_restrict_type => K::Restrict,
        dw::DW_TAG_atomic_type => K::Atomic,
        dw::DW_TAG_array_type => K::Array,
        dw::DW_TAG_subroutine_type => K::Func,
        dw::DW_TAG_unspecified_type => K::Other,
        _ => return None,
    })
}

struct Ctx<'a, 'd> {
    dwarf: &'a gimli::Dwarf<R<'d>>,
    unit: &'a gimli::Unit<R<'d>>,
}

impl<'d> Ctx<'_, 'd> {
    fn string(&self, v: AttributeValue<R<'d>>) -> Option<&'d [u8]> {
        Some(self.dwarf.attr_string(self.unit, v).ok()?.slice())
    }
}

fn udata<Rd: Reader>(v: &AttributeValue<Rd>) -> Option<u64> {
    v.udata_value()
}

const SDATA: u8 = 0xff;

fn enum_value<Rd: Reader>(v: &AttributeValue<Rd>) -> Option<(u64, u8)> {
    Some(match *v {
        AttributeValue::Sdata(x) => (x as u64, SDATA),
        AttributeValue::Udata(x) => (x, 0),
        AttributeValue::Data1(x) => (u64::from(x), 1),
        AttributeValue::Data2(x) => (u64::from(x), 2),
        AttributeValue::Data4(x) => (u64::from(x), 4),
        AttributeValue::Data8(x) => (x, 8),
        _ => return None,
    })
}

fn decode_enum(raw: u64, width: u8, signed: bool) -> i128 {
    match width {
        SDATA => i128::from(raw as i64),
        1 | 2 | 4 | 8 if signed => {
            let shift = 64 - u32::from(width) * 8;
            i128::from(((raw << shift) as i64) >> shift)
        }
        _ => i128::from(raw),
    }
}

fn member_offset<Rd: Reader>(v: &AttributeValue<Rd>, encoding: gimli::Encoding) -> Option<u64> {
    if let Some(x) = v.udata_value() {
        return Some(x);
    }
    let AttributeValue::Exprloc(expr) = v else {
        return None;
    };
    let mut ops = expr.clone().operations(encoding);
    match ops.next().ok()? {
        Some(gimli::Operation::PlusConstant { value }) if ops.next().ok()?.is_none() => Some(value),
        _ => None,
    }
}

struct UnitOut<'d> {
    types: Vec<LType<'d>>,
    entities: Vec<LEntity<'d>>,
    files: Vec<Option<String>>,
    address_size: u8,
}

fn walk<'d>(
    dwarf: &gimli::Dwarf<R<'d>>,
    header: gimli::UnitHeader<R<'d>>,
) -> gimli::Result<UnitOut<'d>> {
    let unit = dwarf.unit(header)?;
    let cx = Ctx { dwarf, unit: &unit };
    let encoding = unit.encoding();
    let files = unit_files(dwarf, &unit);
    let mut types: Vec<LType<'d>> = Vec::new();
    let mut entities: Vec<LEntity<'d>> = Vec::new();
    let mut at: FxHashMap<usize, usize> = FxHashMap::default();
    let mut pending: Vec<(usize, Ref)> = Vec::new();
    let mut parents: Vec<(isize, usize)> = Vec::new();
    let mut cursor = unit.entries();
    if cursor.next_dfs()?.is_none() {
        return Ok(UnitOut {
            types,
            entities,
            files,
            address_size: encoding.address_size,
        });
    }
    let mut next = cursor.next_dfs()?.is_some();
    while next {
        let Some(entry) = cursor.current() else {
            break;
        };
        let depth = entry.depth();
        while parents.last().is_some_and(|(d, _)| *d >= depth) {
            parents.pop();
        }
        let parent = parents.last().map(|&(_, p)| p);
        let tag = entry.tag();
        let offset = entry.offset().0;
        let target = match entry.attr_value(dw::DW_AT_type) {
            Some(AttributeValue::UnitRef(UnitOffset(o))) => Some(Ref::Local(o)),
            Some(_) => Some(Ref::Foreign),
            None => None,
        };
        let name = entry.attr_value(dw::DW_AT_name).and_then(|v| cx.string(v));
        let file = match entry.attr_value(dw::DW_AT_decl_file) {
            Some(AttributeValue::FileIndex(i)) => Some(i),
            Some(v) => udata(&v),
            None => None,
        };
        let line = entry
            .attr_value(dw::DW_AT_decl_line)
            .and_then(|v| udata(&v))
            .unwrap_or(0);
        let mut skip = entry.has_children();
        if let Some(k) = kind_of(tag) {
            let col = entry
                .attr_value(dw::DW_AT_decl_column)
                .and_then(|v| udata(&v))
                .unwrap_or(0);
            let size = entry
                .attr_value(dw::DW_AT_byte_size)
                .and_then(|v| udata(&v));
            let enc = match entry.attr_value(dw::DW_AT_encoding) {
                Some(AttributeValue::Encoding(e)) => e.0,
                _ => 0,
            };
            let decl = matches!(
                entry.attr_value(dw::DW_AT_declaration),
                Some(AttributeValue::Flag(true))
            );
            let prototyped = matches!(
                entry.attr_value(dw::DW_AT_prototyped),
                Some(AttributeValue::Flag(true))
            );
            let idx = types.len();
            at.insert(offset, idx);
            if let Some(t) = target {
                pending.push((idx, t));
            }
            types.push(LType {
                k,
                name,
                size,
                file,
                line,
                col,
                target: None,
                enc,
                decl,
                members: Vec::new(),
                enums: Vec::new(),
                dims: Vec::new(),
                signed_enum: false,
                params: Vec::new(),
                variadic: false,
                prototyped,
            });
            if matches!(k, K::Struct | K::Union | K::Enum | K::Array | K::Func) {
                parents.push((depth, idx));
                skip = false;
            }
        } else if let Some(p) = parent {
            match (tag, types[p].k) {
                (dw::DW_TAG_member, K::Struct | K::Union) => {
                    let offset = match entry.attr_value(dw::DW_AT_data_member_location) {
                        Some(v) => member_offset(&v, encoding),
                        None if types[p].k == K::Union => Some(0),
                        None => None,
                    };
                    let bit_size = entry.attr_value(dw::DW_AT_bit_size).and_then(|v| udata(&v));
                    let bit_offset = entry
                        .attr_value(dw::DW_AT_data_bit_offset)
                        .and_then(|v| udata(&v));
                    let m = types[p].members.len();
                    types[p].members.push(LMember {
                        name,
                        target: None,
                        offset,
                        bit_size,
                        bit_offset,
                    });
                    if let Some(t) = target {
                        pending.push((p, Ref::Member(m, Box::new(t))));
                    }
                }
                (dw::DW_TAG_enumerator, K::Enum) => {
                    if let (Some(n), Some(v)) = (name, entry.attr_value(dw::DW_AT_const_value))
                        && let Some((raw, width)) = enum_value(&v)
                    {
                        types[p].enums.push((n, raw, width));
                    }
                }
                (dw::DW_TAG_formal_parameter, K::Func) => {
                    let n = types[p].params.len();
                    types[p].params.push(NO_TARGET);
                    if let Some(t) = target {
                        pending.push((p, Ref::Param(n, Box::new(t))));
                    }
                }
                (dw::DW_TAG_unspecified_parameters, K::Func) => types[p].variadic = true,
                (dw::DW_TAG_subrange_type, K::Array) => {
                    let count = entry
                        .attr_value(dw::DW_AT_count)
                        .and_then(|v| udata(&v))
                        .or_else(|| {
                            let upper = entry.attr_value(dw::DW_AT_upper_bound)?;
                            let lower = entry
                                .attr_value(dw::DW_AT_lower_bound)
                                .and_then(|v| udata(&v))
                                .unwrap_or(0);
                            udata(&upper)?.checked_add(1)?.checked_sub(lower)
                        });
                    types[p].dims.push(count);
                }
                _ => {}
            }
        } else if depth == 1
            && matches!(tag, dw::DW_TAG_subprogram | dw::DW_TAG_variable)
            && let Some(name) = name
        {
            let idx = entities.len();
            entities.push(LEntity {
                name,
                file,
                line,
                target: None,
                function: tag == dw::DW_TAG_subprogram,
            });
            match target {
                Some(t) => pending.push((idx, Ref::Entity(Box::new(t)))),
                None if tag == dw::DW_TAG_subprogram => {}
                None => entities[idx].target = Some(NO_TARGET),
            }
        }
        next = if skip {
            cursor.next_sibling()?.is_some()
        } else {
            cursor.next_dfs()?.is_some()
        };
    }
    let resolve = |r: &Ref| match r {
        Ref::Local(o) => Some(at.get(o).copied().unwrap_or(NO_TARGET)),
        _ => Some(NO_TARGET),
    };
    for (idx, r) in pending {
        match r {
            Ref::Member(m, t) => types[idx].members[m].target = resolve(&t),
            Ref::Param(n, t) => types[idx].params[n] = resolve(&t).unwrap_or(NO_TARGET),
            Ref::Entity(t) => entities[idx].target = resolve(&t),
            other => types[idx].target = resolve(&other),
        }
    }
    for i in 0..types.len() {
        if types[i].k == K::Enum
            && let Some(t) = types[i].target.filter(|&t| t != NO_TARGET)
        {
            types[i].signed_enum =
                matches!(types.get(t).map(|b| (b.k, b.enc)), Some((K::Base, 5 | 6)));
        }
    }
    Ok(UnitOut {
        types,
        entities,
        files,
        address_size: encoding.address_size,
    })
}

enum Ref {
    Local(usize),
    Foreign,
    Member(usize, Box<Ref>),
    Param(usize, Box<Ref>),
    Entity(Box<Ref>),
}

fn unit_files(dwarf: &gimli::Dwarf<R<'_>>, unit: &gimli::Unit<R<'_>>) -> Vec<Option<String>> {
    let Some(program) = &unit.line_program else {
        return Vec::new();
    };
    let header = program.header();
    let comp_dir = unit
        .comp_dir
        .as_ref()
        .map(|d| PathBuf::from(String::from_utf8_lossy(d.slice()).into_owned()));
    let text = |v: AttributeValue<R<'_>>| -> Option<String> {
        let r = dwarf.attr_string(unit, v).ok()?;
        Some(String::from_utf8_lossy(r.slice()).into_owned())
    };
    let count = header.file_names().len() as u64;
    let first = if header.version() >= 5 { 0 } else { 1 };
    let mut out = vec![None; (count + first) as usize];
    for i in first..count + first {
        let Some(f) = header.file(i) else { continue };
        let Some(name) = text(f.path_name()) else {
            continue;
        };
        let mut path = PathBuf::new();
        if !name.starts_with('/') {
            if let Some(d) = f.directory(header).and_then(text) {
                if !d.starts_with('/')
                    && let Some(c) = &comp_dir
                {
                    path.push(c);
                }
                path.push(d);
            } else if let Some(c) = &comp_dir {
                path.push(c);
            }
        }
        path.push(&name);
        let path = crate::pp::kbuild::normalize(&path);
        let stored = match comp_dir.as_ref().and_then(|c| path.strip_prefix(c).ok()) {
            Some(rel) => rel.to_string_lossy().into_owned(),
            None => path.to_string_lossy().into_owned(),
        };
        out[i as usize] = Some(stored);
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Content {
    size: Id,
    layout: Id,
    types: Id,
}

struct DefEntry {
    content: Content,
    cus: u32,
    conflicts: u8,
    def: Def,
}

fn compare(e: &mut DefEntry, c: Content, size: Option<u64>) {
    e.cus += 1;
    if e.def.kind == DefKind::Typedef {
        match (e.def.size, size) {
            (None, s) => e.def.size = s,
            (Some(a), Some(b)) if a != b => {
                e.conflicts |= encode::SIZE_CONFLICT | encode::LAYOUT_CONFLICT;
            }
            _ => {}
        }
    }
    if e.content.size != c.size {
        e.conflicts |= encode::SIZE_CONFLICT;
    }
    if e.content.layout != c.layout {
        e.conflicts |= encode::LAYOUT_CONFLICT;
    }
    if e.content.types != c.types {
        e.conflicts |= encode::TYPE_CONFLICT;
    }
}

struct EntityEntry {
    target: Id,
    conflict: bool,
    name: String,
    file: String,
    line: u32,
    function: bool,
}

struct Shards<V> {
    shards: Vec<Mutex<FxHashMap<Id, V>>>,
}

impl<V> Shards<V> {
    fn new() -> Shards<V> {
        Shards {
            shards: (0..SHARDS)
                .map(|_| Mutex::new(FxHashMap::default()))
                .collect(),
        }
    }

    fn shard(&self, k: Id) -> std::sync::MutexGuard<'_, FxHashMap<Id, V>> {
        let i = (k as usize) % SHARDS;
        match self.shards[i].lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }
    }

    fn into_values(self) -> Vec<(Id, V)> {
        self.shards
            .into_iter()
            .flat_map(|m| match m.into_inner() {
                Ok(m) => m,
                Err(p) => p.into_inner(),
            })
            .collect()
    }
}

struct Global {
    defs: Shards<DefEntry>,
    nodes: Shards<Node>,
    entities: Shards<EntityEntry>,
    address_size: AtomicU32,
    units: AtomicU64,
}

struct Unit<'u, 'd> {
    out: &'u UnitOut<'d>,
    ids: Vec<Option<Id>>,
    sizes: Vec<Option<Option<u64>>>,
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

impl Unit<'_, '_> {
    fn path(&self, t: &LType<'_>) -> Option<&str> {
        let f = t.file?;
        self.out.files.get(f as usize)?.as_deref()
    }

    fn id(&mut self, i: usize) -> Id {
        if i == NO_TARGET || i >= self.out.types.len() {
            return UNKNOWN;
        }
        if let Some(Some(id)) = self.ids.get(i) {
            return *id;
        }
        self.ids[i] = Some(UNKNOWN);
        let t = &self.out.types[i];
        let target = t.target;
        let id = match t.k {
            K::Base => Hasher::new(b"B")
                .bytes(t.name.unwrap_or_default())
                .u64(t.size.unwrap_or(0))
                .u64(u64::from(t.enc))
                .finish(),
            K::Struct | K::Union | K::Enum => {
                let tag: &[u8] = match t.k {
                    K::Struct => b"S",
                    K::Union => b"U",
                    _ => b"E",
                };
                match (&t.name, self.path(t)) {
                    (Some(n), _) => Hasher::new(tag).bytes(n).finish(),
                    (None, Some(p)) => {
                        let h = Hasher::new(b"anon")
                            .bytes(tag)
                            .bytes(p.as_bytes())
                            .u64(t.line)
                            .u64(t.col);
                        h.id(self.content(i).types).finish()
                    }
                    (None, None) => UNKNOWN,
                }
            }
            K::Typedef => match &t.name {
                Some(n) => Hasher::new(b"T").bytes(n).finish(),
                None => UNKNOWN,
            },
            K::Ptr | K::Const | K::Volatile | K::Restrict | K::Atomic => {
                let tag: &[u8] = match t.k {
                    K::Ptr => b"P",
                    K::Const => b"C",
                    K::Volatile => b"V",
                    K::Restrict => b"R",
                    _ => b"A",
                };
                let inner = match target {
                    None => VOID,
                    Some(x) => self.id(x),
                };
                if inner == UNKNOWN && t.k != K::Ptr {
                    UNKNOWN
                } else {
                    Hasher::new(tag).id(inner).finish()
                }
            }
            K::Array => {
                let elem = target.map_or(UNKNOWN, |x| self.id(x));
                if elem == UNKNOWN {
                    UNKNOWN
                } else {
                    let t = &self.out.types[i];
                    let mut h = Hasher::new(b"[").id(elem).u64(t.dims.len() as u64);
                    for d in &t.dims {
                        h = h.u64(d.unwrap_or(u64::MAX));
                    }
                    h.finish()
                }
            }
            K::Func => {
                let ret = target.map_or(VOID, |x| self.id(x));
                let t = &self.out.types[i];
                let params = t.params.clone();
                let flags = u64::from(t.variadic) | u64::from(t.prototyped) << 1;
                let mut h = Hasher::new(b"F")
                    .id(ret)
                    .u64(flags)
                    .u64(params.len() as u64);
                for x in params {
                    h = h.id(self.id(x));
                }
                h.finish()
            }
            K::Other => UNKNOWN,
        };
        self.ids[i] = Some(id);
        id
    }

    fn own_size(&mut self, i: usize) -> Option<u64> {
        let t = &self.out.types[i];
        match t.k {
            K::Typedef => t.target.and_then(|x| self.size(x)),
            _ => t.size,
        }
    }

    fn content(&mut self, i: usize) -> Content {
        let out = self.out;
        let t = &out.types[i];
        let size = if t.k == K::Typedef {
            None
        } else {
            self.own_size(i)
        };
        let mut layout = Hasher::new(b"L").u64(size.unwrap_or(u64::MAX));
        let mut types = Hasher::new(b"Y");
        for m in &t.members {
            let mid = m.target.map_or(UNKNOWN, |x| self.id(x));
            let msize = m.target.and_then(|x| self.size(x));
            layout = layout
                .bytes(m.name.unwrap_or(b"\0"))
                .u64(m.offset.unwrap_or(u64::MAX))
                .u64(msize.unwrap_or(u64::MAX))
                .u64(m.bit_size.unwrap_or(0))
                .u64(m.bit_offset.unwrap_or(u64::MAX));
            types = types.id(mid);
        }
        for &(name, raw, width) in &t.enums {
            let v = decode_enum(raw, width, t.signed_enum);
            layout = layout.bytes(name).u64(v as u64).u64((v >> 64) as u64);
        }
        let layout = layout.u64(u64::from(t.signed_enum)).finish();
        let target = match t.k {
            K::Typedef | K::Enum => t.target.map_or(VOID, |x| self.id(x)),
            _ => VOID,
        };
        Content {
            size: Hasher::new(b"Z").u64(size.unwrap_or(u64::MAX)).finish(),
            layout,
            types: types.id(target).id(layout).finish(),
        }
    }

    fn size(&mut self, i: usize) -> Option<u64> {
        if i == NO_TARGET || i >= self.out.types.len() {
            return None;
        }
        if let Some(Some(s)) = self.sizes.get(i) {
            return *s;
        }
        self.sizes[i] = Some(None);
        let t = &self.out.types[i];
        let s = match t.k {
            K::Base | K::Struct | K::Union | K::Enum if t.decl => None,
            K::Base | K::Struct | K::Union | K::Enum => t.size,
            K::Ptr => t.size.or(Some(u64::from(self.out.address_size))),
            K::Typedef | K::Const | K::Volatile | K::Restrict => {
                t.target.and_then(|x| self.size(x))
            }
            K::Atomic => t.size.or_else(|| t.target.and_then(|x| self.size(x))),
            K::Array => {
                let dims = t.dims.clone();
                let elem = t.target.and_then(|x| self.size(x));
                match elem {
                    Some(e) if !dims.is_empty() && dims.iter().all(Option::is_some) => dims
                        .iter()
                        .try_fold(e, |acc, d| acc.checked_mul(d.unwrap_or(0))),
                    _ => None,
                }
            }
            K::Func | K::Other => None,
        };
        self.sizes[i] = Some(s);
        s
    }
}

fn intern_node(u: &mut Unit<'_, '_>, i: usize, g: &Global, seen: &mut FxHashSet<usize>) {
    if i == NO_TARGET || i >= u.out.types.len() || !seen.insert(i) {
        return;
    }
    let id = u.id(i);
    if id == UNKNOWN {
        return;
    }
    {
        let shard = g.nodes.shard(id);
        if shard.contains_key(&id) {
            return;
        }
    }
    let t = &u.out.types[i];
    let target = t.target;
    let node = match t.k {
        K::Base => Node {
            kind: NodeKind::Base,
            name: t.name.map(lossy).unwrap_or_default(),
            target: VOID,
            count: t.size.unwrap_or(0),
            enc: t.enc,
            anon: None,
            params: Vec::new(),
        },
        K::Struct | K::Union | K::Enum if t.name.is_some() => Node {
            kind: match t.k {
                K::Struct => NodeKind::Struct,
                K::Union => NodeKind::Union,
                _ => NodeKind::Enum,
            },
            name: t.name.map(lossy).unwrap_or_default(),
            target: VOID,
            count: 0,
            enc: 0,
            anon: None,
            params: Vec::new(),
        },
        K::Struct | K::Union | K::Enum => Node {
            kind: NodeKind::Anon,
            name: String::new(),
            target: VOID,
            count: 0,
            enc: 0,
            anon: Some(id),
            params: Vec::new(),
        },
        K::Typedef => Node {
            kind: NodeKind::Typedef,
            name: t.name.map(lossy).unwrap_or_default(),
            target: VOID,
            count: 0,
            enc: 0,
            anon: None,
            params: Vec::new(),
        },
        K::Ptr | K::Const | K::Volatile | K::Restrict | K::Atomic => {
            let inner = target.map_or(VOID, |x| u.id(x));
            if let Some(x) = target {
                intern_node(u, x, g, seen);
            }
            Node {
                kind: match t.k {
                    K::Ptr => NodeKind::Pointer,
                    K::Const => NodeKind::Const,
                    K::Volatile => NodeKind::Volatile,
                    K::Restrict => NodeKind::Restrict,
                    _ => NodeKind::Atomic,
                },
                name: String::new(),
                target: inner,
                count: 0,
                enc: 0,
                anon: None,
                params: Vec::new(),
            }
        }
        K::Array => {
            let dims = t.dims.clone();
            let elem = target.map_or(UNKNOWN, |x| u.id(x));
            if let Some(x) = target {
                intern_node(u, x, g, seen);
            }
            let mut inner = elem;
            for (k, d) in dims.iter().enumerate().skip(1).rev() {
                let mut h = Hasher::new(b"[").id(elem).u64((dims.len() - k) as u64);
                for dd in &dims[k..] {
                    h = h.u64(dd.unwrap_or(u64::MAX));
                }
                let id = h.finish();
                g.nodes.shard(id).entry(id).or_insert(Node {
                    kind: NodeKind::Array,
                    name: String::new(),
                    target: inner,
                    count: d.unwrap_or(u64::MAX),
                    enc: 0,
                    anon: None,
                    params: Vec::new(),
                });
                inner = id;
            }
            Node {
                kind: NodeKind::Array,
                name: String::new(),
                target: inner,
                count: dims.first().copied().flatten().unwrap_or(u64::MAX),
                enc: 0,
                anon: None,
                params: Vec::new(),
            }
        }
        K::Func => {
            let params = t.params.clone();
            let flags = u8::from(t.variadic) | u8::from(t.prototyped) << 1;
            let ret = target.map_or(VOID, |x| u.id(x));
            if let Some(x) = target {
                intern_node(u, x, g, seen);
            }
            let mut ids = Vec::with_capacity(params.len());
            for x in params {
                intern_node(u, x, g, seen);
                ids.push(u.id(x));
            }
            Node {
                kind: NodeKind::Function,
                name: String::new(),
                target: ret,
                count: 0,
                enc: flags,
                anon: None,
                params: ids,
            }
        }
        K::Other => return,
    };
    g.nodes.shard(id).entry(id).or_insert(node);
}

fn def_key(u: &mut Unit<'_, '_>, i: usize) -> Id {
    let t = &u.out.types[i];
    if t.name.is_none() && t.k != K::Typedef {
        return u.id(i);
    }
    let tag: &[u8] = match t.k {
        K::Struct => b"s",
        K::Union => b"u",
        K::Enum => b"e",
        _ => b"t",
    };
    Hasher::new(b"D")
        .bytes(tag)
        .bytes(t.name.unwrap_or_default())
        .bytes(u.path(t).unwrap_or_default().as_bytes())
        .u64(t.line)
        .u64(t.col)
        .finish()
}

fn merge(out: &UnitOut<'_>, g: &Global) {
    let n = out.types.len();
    let mut u = Unit {
        out,
        ids: vec![None; n],
        sizes: vec![None; n],
    };
    let mut seen = FxHashSet::default();
    for i in 0..n {
        let t = &out.types[i];
        let definition = match t.k {
            K::Struct | K::Union | K::Enum => !t.decl && t.size.is_some(),
            K::Typedef => t.name.is_some(),
            _ => false,
        };
        if !definition || u.path(t).is_none() {
            continue;
        }
        let key = def_key(&mut u, i);
        if key == UNKNOWN {
            continue;
        }
        let content = u.content(i);
        {
            let mut shard = g.defs.shard(key);
            if let Some(e) = shard.get_mut(&key) {
                compare(e, content, u.own_size(i));
                continue;
            }
        }
        for m in &t.members {
            if let Some(x) = m.target {
                intern_node(&mut u, x, g, &mut seen);
            }
        }
        if let Some(x) = t.target {
            intern_node(&mut u, x, g, &mut seen);
        }
        let member_info: Vec<(Id, Option<u64>)> = t
            .members
            .iter()
            .map(|m| {
                (
                    m.target.map_or(UNKNOWN, |x| u.id(x)),
                    m.target.and_then(|x| u.size(x)),
                )
            })
            .collect();
        let values: Vec<i128> = t
            .enums
            .iter()
            .map(|&(_, raw, width)| decode_enum(raw, width, t.signed_enum))
            .collect();
        let target = match t.k {
            K::Typedef | K::Enum => t.target.map_or(VOID, |x| u.id(x)),
            _ => VOID,
        };
        let own_size = u.own_size(i);
        let t = &out.types[i];
        let def = Def {
            kind: match t.k {
                K::Struct => DefKind::Struct,
                K::Union => DefKind::Union,
                K::Enum => DefKind::Enum,
                _ => DefKind::Typedef,
            },
            name: t.name.map(lossy).unwrap_or_default(),
            file: u.path(t).unwrap_or_default().to_string(),
            line: t.line as u32,
            col: t.col as u32,
            size: own_size,
            members: t
                .members
                .iter()
                .zip(&member_info)
                .map(|(m, &(mid, msize))| Member {
                    name: m.name.map(lossy).unwrap_or_default(),
                    ty: mid,
                    offset: m.offset,
                    size: msize,
                    bit_size: m.bit_size.unwrap_or(0),
                    bit_offset: m.bit_offset,
                })
                .collect(),
            enumerators: t
                .enums
                .iter()
                .zip(&values)
                .map(|((n, ..), v)| (lossy(n), *v))
                .collect(),
            target,
            signed_enum: t.signed_enum,
            cus: 1,
            conflicts: 0,
        };
        let mut shard = g.defs.shard(key);
        match shard.get_mut(&key) {
            Some(e) => compare(e, content, own_size),
            None => {
                shard.insert(
                    key,
                    DefEntry {
                        content,
                        cus: 1,
                        conflicts: 0,
                        def,
                    },
                );
            }
        }
    }
    for e in &out.entities {
        let Some(file) = e.file.and_then(|f| out.files.get(f as usize)?.clone()) else {
            continue;
        };
        let target = match e.target {
            None => VOID,
            Some(NO_TARGET) => UNKNOWN,
            Some(x) => {
                intern_node(&mut u, x, g, &mut seen);
                u.id(x)
            }
        };
        let key = Hasher::new(if e.function { b"f" } else { b"v" })
            .bytes(e.name)
            .bytes(file.as_bytes())
            .u64(e.line)
            .finish();
        let mut shard = g.entities.shard(key);
        match shard.get_mut(&key) {
            Some(x) if x.target == target => {}
            Some(x) => x.conflict = true,
            None => {
                shard.insert(
                    key,
                    EntityEntry {
                        target,
                        conflict: false,
                        name: lossy(e.name),
                        file,
                        line: e.line as u32,
                        function: e.function,
                    },
                );
            }
        }
    }
}

fn process_product(path: &Path, g: &Global) -> ProductReport {
    let mut report = ProductReport {
        path: path.to_path_buf(),
        ..ProductReport::default()
    };
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            report.error = Some(e.to_string());
            return report;
        }
    };
    let map = match unsafe { memmap2::Mmap::map(&file) } {
        Ok(m) => m,
        Err(e) => {
            report.error = Some(e.to_string());
            return report;
        }
    };
    let obj = match object::File::parse(&*map) {
        Ok(o) => o,
        Err(e) => {
            report.error = Some(e.to_string());
            return report;
        }
    };
    let endian = if obj.is_little_endian() {
        RunTimeEndian::Little
    } else {
        RunTimeEndian::Big
    };
    let mut secs: Vec<Option<Sec<'_>>> = Vec::with_capacity(SECTIONS.len());
    for (_, name) in SECTIONS {
        match load_section(&obj, name) {
            Ok(s) => secs.push(s),
            Err(e) => {
                report.error = Some(e);
                return report;
            }
        }
    }
    if secs[1].is_none() {
        report.error = Some("no .debug_info".to_string());
        return report;
    }
    report.ignored_relocations = secs.iter().flatten().map(|s| s.ignored).sum();
    let dwarf = gimli::Dwarf::load(|id| -> Result<R<'_>, gimli::Error> {
        let slot = SECTIONS.iter().position(|(s, _)| *s == id);
        Ok(match slot.and_then(|i| secs[i].as_ref()) {
            Some(s) => EndianSlice::new(&s.data, endian),
            None => EndianSlice::new(&[], endian),
        })
    });
    let dwarf = match dwarf {
        Ok(d) => d,
        Err(e) => {
            report.error = Some(e.to_string());
            return report;
        }
    };
    let mut headers = Vec::new();
    let mut iter = dwarf.units();
    loop {
        match iter.next() {
            Ok(Some(h)) => headers.push(h),
            Ok(None) => break,
            Err(e) => {
                report.error = Some(format!("unit headers: {e}"));
                break;
            }
        }
    }
    report.units = headers.len() as u32;
    let failed = AtomicU32::new(0);
    headers.into_par_iter().for_each(|h| match walk(&dwarf, h) {
        Ok(out) => {
            g.address_size
                .fetch_max(u32::from(out.address_size), Ordering::Relaxed);
            g.units.fetch_add(1, Ordering::Relaxed);
            merge(&out, g);
        }
        Err(_) => {
            failed.fetch_add(1, Ordering::Relaxed);
        }
    });
    report.failed_units = failed.into_inner();
    report
}

pub fn extract(products: &[PathBuf]) -> Extracted {
    let g = Global {
        defs: Shards::new(),
        nodes: Shards::new(),
        entities: Shards::new(),
        address_size: AtomicU32::new(0),
        units: AtomicU64::new(0),
    };
    let products: Vec<ProductReport> = products
        .par_iter()
        .map(|p| process_product(p, &g))
        .collect();
    let mut defs: Vec<(Id, Def)> = g
        .defs
        .into_values()
        .into_iter()
        .map(|(k, mut e)| {
            e.def.cus = e.cus;
            e.def.conflicts = e.conflicts;
            (k, e.def)
        })
        .collect();
    defs.sort_by(|a, b| {
        (a.1.kind as u8, &a.1.name, &a.1.file, a.1.line, a.1.col, a.0).cmp(&(
            b.1.kind as u8,
            &b.1.name,
            &b.1.file,
            b.1.line,
            b.1.col,
            b.0,
        ))
    });
    let nodes: FxHashMap<Id, Node> = g.nodes.into_values().into_iter().collect();
    let mut entities: Vec<encode::Entity> = g
        .entities
        .into_values()
        .into_iter()
        .map(|(_, e)| encode::Entity {
            name: e.name,
            file: e.file,
            line: e.line,
            ty: if e.conflict { UNKNOWN } else { e.target },
            function: e.function,
        })
        .collect();
    entities.sort_by(|a, b| {
        (a.function, &a.name, &a.file, a.line).cmp(&(b.function, &b.name, &b.file, b.line))
    });
    Extracted {
        raw: Raw {
            defs,
            nodes,
            entities,
            address_size: g.address_size.into_inner() as u8,
            units: g.units.into_inner(),
        },
        products,
    }
}
