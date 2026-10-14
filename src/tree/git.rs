use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeadInfo {
    pub commit: String,
    pub reference: Option<String>,
    pub shallow: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusEntry {
    pub code: String,
    pub path: String,
}

#[derive(Debug, thiserror::Error)]
#[error("`git {args}` failed: {why}")]
pub struct GitError {
    pub args: String,
    pub why: String,
}

#[derive(Clone, Debug)]
pub struct GitDirs {
    pub git_dir: PathBuf,
    pub common_dir: PathBuf,
}

impl GitDirs {
    pub fn discover(root: &Path) -> Option<GitDirs> {
        let dotgit = root.join(".git");
        let meta = std::fs::metadata(&dotgit).ok()?;
        let git_dir = if meta.is_dir() {
            dotgit
        } else {
            let text = std::fs::read_to_string(&dotgit).ok()?;
            let rel = text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
            absolutize(root, rel)
        };
        let common_dir = std::fs::read_to_string(git_dir.join("commondir"))
            .map(|s| absolutize(&git_dir, s.trim()))
            .unwrap_or_else(|_| git_dir.clone());
        common_dir.join("objects").is_dir().then_some(GitDirs {
            git_dir,
            common_dir,
        })
    }

    pub fn read_head(&self) -> Result<HeadInfo, String> {
        let head = std::fs::read_to_string(self.git_dir.join("HEAD"))
            .map_err(|e| format!("cannot read HEAD: {e}"))?;
        let head = head.trim();
        let shallow = self.common_dir.join("shallow").exists();
        if is_object_id(head) {
            return Ok(HeadInfo {
                commit: head.to_ascii_lowercase(),
                reference: None,
                shallow,
            });
        }
        let first = head
            .strip_prefix("ref:")
            .map(str::trim)
            .ok_or_else(|| format!("unrecognised HEAD contents `{head}`"))?;
        let mut target = first.to_string();
        for _ in 0..8 {
            match self.lookup_ref(&target) {
                Some(v) if is_object_id(&v) => {
                    return Ok(HeadInfo {
                        commit: v.to_ascii_lowercase(),
                        reference: Some(first.to_string()),
                        shallow,
                    });
                }
                Some(v) => match v.strip_prefix("ref:") {
                    Some(next) => target = next.trim().to_string(),
                    None => return Err(format!("ref {target} holds `{v}`")),
                },
                None => return Err(format!("{first} does not point to a commit")),
            }
        }
        Err(format!("ref chain starting at {first} is too deep"))
    }

    fn lookup_ref(&self, name: &str) -> Option<String> {
        for dir in [&self.git_dir, &self.common_dir] {
            if let Ok(s) = std::fs::read_to_string(dir.join(name)) {
                return Some(s.trim().to_string());
            }
        }
        let packed = std::fs::read_to_string(self.common_dir.join("packed-refs")).ok()?;
        packed
            .lines()
            .filter(|l| !l.starts_with('#') && !l.starts_with('^'))
            .filter_map(|l| l.split_once(' '))
            .find(|(_, n)| n.trim() == name)
            .map(|(h, _)| h.to_string())
    }
}

fn absolutize(base: &Path, p: &str) -> PathBuf {
    let p = PathBuf::from(p);
    if p.is_absolute() { p } else { base.join(p) }
}

pub fn head_tree(root: &Path) -> Option<String> {
    let out = run(root, &["rev-parse", "--verify", "-q", "HEAD^{tree}"]).ok()?;
    let id = String::from_utf8_lossy(&out).trim().to_ascii_lowercase();
    is_object_id(&id).then_some(id)
}

pub fn tree_of(root: &Path, commit: &str) -> Option<String> {
    if !is_object_id(commit) {
        return None;
    }
    let out = run(
        root,
        &["rev-parse", "--verify", "-q", &format!("{commit}^{{tree}}")],
    )
    .ok()?;
    let id = String::from_utf8_lossy(&out).trim().to_ascii_lowercase();
    is_object_id(&id).then_some(id)
}

pub fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|c| c.is_ascii_hexdigit())
}

pub fn run(root: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let fail = |why: String| GitError {
        args: args.join(" "),
        why,
    };
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("--no-optional-locks")
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .map_err(|e| fail(e.to_string()))?;
    if !out.status.success() {
        return Err(fail(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    Ok(out.stdout)
}

pub fn check_ignore(root: &Path, paths: &[String]) -> Result<Vec<String>, GitError> {
    use std::io::Write as _;
    use std::process::Stdio;
    let fail = |why: String| GitError {
        args: "check-ignore -z --stdin".to_string(),
        why,
    };
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["--no-optional-locks", "check-ignore", "-z", "--stdin"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| fail(e.to_string()))?;
    let mut input = Vec::new();
    for p in paths {
        input.extend_from_slice(p.as_bytes());
        input.push(0);
    }
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&input).map_err(|e| fail(e.to_string()))?;
    }
    let out = child.wait_with_output().map_err(|e| fail(e.to_string()))?;
    Ok(split_nul(&out.stdout))
}

pub fn status(root: &Path) -> Result<Vec<StatusEntry>, GitError> {
    let raw = run(
        root,
        &[
            "status",
            "--porcelain=v2",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=all",
        ],
    )?;
    Ok(parse_status_v2(&raw))
}

fn split_nul(raw: &[u8]) -> Vec<String> {
    raw.split(|&b| b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

pub fn parse_status_v2(raw: &[u8]) -> Vec<StatusEntry> {
    let mut out = Vec::new();
    let mut records = raw.split(|&b| b == 0).filter(|r| !r.is_empty());
    while let Some(rec) = records.next() {
        let rec = String::from_utf8_lossy(rec);
        let (tag, rest) = rec.split_once(' ').unwrap_or((&rec, ""));
        let entry = match tag {
            "1" => field_tail(rest, 8).map(|(f, p)| (f[0].to_string(), p)),
            "2" => {
                let e = field_tail(rest, 9).map(|(f, p)| (format!("{} {}", f[0], f[7]), p));
                records.next();
                e
            }
            "u" => field_tail(rest, 10).map(|(f, p)| (format!("u {}", f[0]), p)),
            "?" => Some(("??".to_string(), rest.to_string())),
            _ => None,
        };
        if let Some((code, path)) = entry {
            out.push(StatusEntry { code, path });
        }
    }
    out
}

fn field_tail(rest: &str, n: usize) -> Option<(Vec<&str>, String)> {
    let parts: Vec<&str> = rest.splitn(n, ' ').collect();
    (parts.len() == n).then(|| (parts[..n - 1].to_vec(), parts[n - 1].to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_v2_parsing() {
        let raw = b"1 .M N... 100644 100644 100644 abc def mm/slub.c\0? new.h\0\
2 R. N... 100644 100644 100644 abc def R100 b.h\0a.h\0";
        let v = parse_status_v2(raw);
        assert_eq!(v.len(), 3);
        assert_eq!(
            v[0],
            StatusEntry {
                code: ".M".into(),
                path: "mm/slub.c".into()
            }
        );
        assert_eq!(v[1].path, "new.h");
        assert_eq!(v[2].code, "R. R100");
        assert_eq!(v[2].path, "b.h");
    }

    #[test]
    fn object_ids() {
        assert!(is_object_id(&"a".repeat(40)));
        assert!(is_object_id(&"0".repeat(64)));
        assert!(!is_object_id("ref: refs/heads/master"));
    }
}
