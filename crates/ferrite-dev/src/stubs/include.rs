//! Header names: how a path clang reports is written in an `#include`, and the order a union of
//! headers has to be included in.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

/// A header as an `#include` names it: `<linux/jiffies.h>` below an include root, else `"path"`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Include {
    pub quoted: bool,
    pub name: String,
}

impl Include {
    /// The name of the header at `path` relative to the longest of `roots` that contains it.
    pub fn resolve(path: &str, roots: &[String]) -> Self {
        let path = clean(path);
        let root = roots.iter().filter(|root| path.starts_with(&format!("{root}/"))).max_by_key(|root| root.len());
        match root {
            Some(root) => Self { quoted: false, name: path[root.len() + 1..].to_owned() },
            None => Self { quoted: true, name: path },
        }
    }

    pub fn directive(&self) -> String {
        if self.quoted { format!("#include \"{}\"", self.name) } else { format!("#include <{}>", self.name) }
    }
}

/// `path` without `.` and `..` components, resolved textually.
pub fn clean(path: &str) -> String {
    let mut out = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => out.push(part),
            Component::ParentDir => {
                out.pop();
            },
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {},
        }
    }
    out.to_string_lossy().into_owned()
}

/// The include roots among compile flags, in search order.
pub fn include_roots(flags: &[String]) -> Vec<String> {
    flags.iter().filter_map(|flag| flag.strip_prefix("-I")).map(clean).collect()
}

/// Private headers that only parse after the `.c` that includes them has defined what they call.
const NOT_SELF_CONTAINED: [&str; 1] = ["kernel/locking/ww_mutex.h"];

/// The headers a source includes itself, from `clang -E -H` output, in the order it includes them. A `.c`
/// file included by a unity-build source is code, not a header, and is left out with `NOT_SELF_CONTAINED`.
pub fn direct_includes(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix(". "))
        .map(str::trim)
        .filter(|path| !Path::new(path).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("c")))
        .filter(|path| !NOT_SELF_CONTAINED.iter().any(|private| path.ends_with(private)))
        .collect()
}

/// Every header once, in the order a first reader of `sources` meets it. Each header follows whatever the
/// source that brought it in included before it, so the result parses as one translation unit.
pub fn union_in_order<'a>(sources: impl IntoIterator<Item = Vec<&'a str>>, roots: &[String]) -> Vec<Include> {
    let mut seen = BTreeSet::new();
    let mut order = Vec::new();
    for header in sources.into_iter().flatten() {
        let include = Include::resolve(header, roots);
        if seen.insert(include.clone()) {
            order.push(include);
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use assert2::check;
    use rstest::rstest;

    use super::*;

    fn roots() -> Vec<String> {
        include_roots(&[
            "-nostdinc".into(),
            "-Ishadow/include".into(),
            "-Ilinux/include".into(),
            "-Ilinux/include/uapi".into(),
        ])
    }

    #[rstest]
    #[case::kernel("linux/include/linux/jiffies.h", false, "linux/jiffies.h")]
    #[case::shadow_and_real_agree("shadow/include/linux/init.h", false, "linux/init.h")]
    #[case::longest_root_wins("linux/include/uapi/linux/if_ether.h", false, "linux/if_ether.h")]
    #[case::dotted("linux/net/mac80211/../../include/net/mac80211.h", false, "net/mac80211.h")]
    #[case::local("linux/net/wireless/nl80211.h", true, "linux/net/wireless/nl80211.h")]
    fn paths_become_include_names(#[case] path: &str, #[case] quoted: bool, #[case] name: &str) {
        check!(Include::resolve(path, &roots()) == Include { quoted, name: name.to_owned() });
    }

    #[test]
    fn directives_use_angle_brackets_below_a_root() {
        check!(Include::resolve("linux/include/linux/a.h", &roots()).directive() == "#include <linux/a.h>");
        check!(Include::resolve("linux/net/b.h", &roots()).directive() == "#include \"linux/net/b.h\"");
    }

    #[test]
    fn only_depth_one_lines_of_the_include_tree_are_direct() {
        let stderr = ". linux/include/a.h\n.. linux/include/nested.h\n. linux/include/b.h\nwarning: x\n";
        check!(direct_includes(stderr) == ["linux/include/a.h", "linux/include/b.h"]);
    }

    #[test]
    fn the_union_keeps_first_sight_order_and_merges_aliases() {
        let first = vec!["linux/include/linux/a.h", "shadow/include/linux/b.h"];
        let second = vec!["linux/include/linux/b.h", "linux/include/linux/c.h", "linux/include/linux/a.h"];
        let union = union_in_order([first, second], &roots());
        check!(union.iter().map(|i| i.name.as_str()).collect::<Vec<_>>() == ["linux/a.h", "linux/b.h", "linux/c.h"]);
    }
}
