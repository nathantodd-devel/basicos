//! Path normalization, shared by anything that has a current directory.
//!
//! Paths here are the *drive-relative* part only (what
//! `kernel::fs::resolve_path` returns after peeling off `A:`/`B:`/`C:`):
//! `/`-separated, no leading slash in the result, `""` meaning the root.

use alloc::string::String;
use alloc::vec::Vec;

/// Resolve `path` against `cwd`, collapsing `.`, `..` and repeated
/// slashes. A `path` starting with `/` ignores `cwd`. `..` at the root
/// stays at the root (like POSIX). The result has no leading or trailing
/// slash; the root is `""`.
pub fn normalize(cwd: &str, path: &str) -> String {
    let base = if path.starts_with('/') { "" } else { cwd };
    let mut parts: Vec<&str> = Vec::new();
    for comp in base.split('/').chain(path.split('/')) {
        match comp {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    parts.join("/")
}

/// Split a normalized path into `(parent, name)`; the root has no name.
pub fn split(path: &str) -> (&str, &str) {
    match path.rfind('/') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => ("", path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_and_absolute() {
        assert_eq!(normalize("a/b", "c"), "a/b/c");
        assert_eq!(normalize("a/b", "/c"), "c");
        assert_eq!(normalize("", "x/y"), "x/y");
        assert_eq!(normalize("a", ""), "a");
        assert_eq!(normalize("a", "/"), "");
    }

    #[test]
    fn dots_and_slashes() {
        assert_eq!(normalize("a/b", "../c"), "a/c");
        assert_eq!(normalize("a/b", "./c/./d"), "a/b/c/d");
        assert_eq!(normalize("a", "b//c///d/"), "a/b/c/d");
        assert_eq!(normalize("a/b", "../../.."), "");
        assert_eq!(normalize("", ".."), "");
        assert_eq!(normalize("a", "/../.."), "");
    }

    #[test]
    fn split_paths() {
        assert_eq!(split("a/b/c"), ("a/b", "c"));
        assert_eq!(split("a"), ("", "a"));
        assert_eq!(split(""), ("", ""));
    }
}
