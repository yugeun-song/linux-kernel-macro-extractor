use rustc_hash::FxHashMap;

use crate::intern::Sym;

pub type HsId = u32;

pub struct HideSets {
    sets: Vec<Box<[Sym]>>,
    ids: FxHashMap<Box<[Sym]>, HsId>,
    add_cache: FxHashMap<(HsId, Sym), HsId>,
    union_cache: FxHashMap<(HsId, HsId), HsId>,
    inter_cache: FxHashMap<(HsId, HsId), HsId>,
}

impl Default for HideSets {
    fn default() -> Self {
        Self::new()
    }
}

impl HideSets {
    pub fn new() -> Self {
        let mut hs = HideSets {
            sets: Vec::new(),
            ids: FxHashMap::default(),
            add_cache: FxHashMap::default(),
            union_cache: FxHashMap::default(),
            inter_cache: FxHashMap::default(),
        };
        hs.intern(Vec::new());
        hs
    }

    fn intern(&mut self, v: Vec<Sym>) -> HsId {
        let boxed: Box<[Sym]> = v.into_boxed_slice();
        if let Some(&id) = self.ids.get(&boxed) {
            return id;
        }
        let id = self.sets.len() as HsId;
        self.sets.push(boxed.clone());
        self.ids.insert(boxed, id);
        id
    }

    pub fn contains(&self, hs: HsId, s: Sym) -> bool {
        self.sets[hs as usize].binary_search(&s).is_ok()
    }

    pub fn add(&mut self, hs: HsId, s: Sym) -> HsId {
        if let Some(&id) = self.add_cache.get(&(hs, s)) {
            return id;
        }
        let set = &self.sets[hs as usize];
        let id = match set.binary_search(&s) {
            Ok(_) => hs,
            Err(pos) => {
                let mut v = set.to_vec();
                v.insert(pos, s);
                self.intern(v)
            }
        };
        self.add_cache.insert((hs, s), id);
        id
    }

    pub fn union(&mut self, a: HsId, b: HsId) -> HsId {
        if a == b || b == 0 {
            return a;
        }
        if a == 0 {
            return b;
        }
        let key = (a.min(b), a.max(b));
        if let Some(&id) = self.union_cache.get(&key) {
            return id;
        }
        let (x, y) = (&self.sets[a as usize], &self.sets[b as usize]);
        let mut v = Vec::with_capacity(x.len() + y.len());
        let (mut i, mut j) = (0, 0);
        while i < x.len() && j < y.len() {
            match x[i].cmp(&y[j]) {
                std::cmp::Ordering::Less => {
                    v.push(x[i]);
                    i += 1;
                }
                std::cmp::Ordering::Greater => {
                    v.push(y[j]);
                    j += 1;
                }
                std::cmp::Ordering::Equal => {
                    v.push(x[i]);
                    i += 1;
                    j += 1;
                }
            }
        }
        v.extend_from_slice(&x[i..]);
        v.extend_from_slice(&y[j..]);
        let id = self.intern(v);
        self.union_cache.insert(key, id);
        id
    }

    pub fn intersect(&mut self, a: HsId, b: HsId) -> HsId {
        if a == b {
            return a;
        }
        if a == 0 || b == 0 {
            return 0;
        }
        let key = (a.min(b), a.max(b));
        if let Some(&id) = self.inter_cache.get(&key) {
            return id;
        }
        let y = &self.sets[b as usize];
        let v: Vec<Sym> = self.sets[a as usize]
            .iter()
            .copied()
            .filter(|s| y.binary_search(s).is_ok())
            .collect();
        let id = self.intern(v);
        self.inter_cache.insert(key, id);
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_algebra() {
        let mut h = HideSets::new();
        let a = h.add(0, Sym(5));
        let a2 = h.add(a, Sym(3));
        let b = h.add(0, Sym(3));
        assert!(h.contains(a2, Sym(5)) && h.contains(a2, Sym(3)));
        assert_eq!(h.intersect(a2, b), b);
        assert_eq!(h.union(a, b), a2);
        assert_eq!(h.intersect(a, b), 0);
        assert_eq!(h.add(a2, Sym(5)), a2);
    }
}
