use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Root {
    Src,
    Obj,
}

impl PartialOrd for Root {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Root {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (*self as u8).cmp(&(*other as u8))
    }
}

#[derive(Clone, Debug)]
pub struct Layout {
    pub srctree: PathBuf,
    pub objtree: PathBuf,
    pub srcarch: String,
}

impl Layout {
    pub fn in_tree(&self) -> bool {
        self.srctree == self.objtree
    }

    pub fn obj_root(&self) -> Root {
        if self.in_tree() { Root::Src } else { Root::Obj }
    }

    pub fn abs(&self, root: Root, rel: &str) -> PathBuf {
        match root {
            Root::Src => self.srctree.join(rel),
            Root::Obj => self.objtree.join(rel),
        }
    }
}
