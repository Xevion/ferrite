//! Reading `llvm-nm` output: which symbols an archive defines and which it leaves undefined.

use std::collections::BTreeSet;

/// Archive members built from a generated glue file start with this. They are what the generator itself
/// writes, so they must not count as definitions when it works out what is still missing.
pub const GENERATED_GLUE_PREFIX: &str = "generated_";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Symbols {
    pub defined: BTreeSet<String>,
    pub undefined: BTreeSet<String>,
}

/// Whether an archive member (`<hash>-<file>.o`, as `cc` names them) comes from a generated glue file.
pub fn is_generated_member(member: &str) -> bool {
    member.split_once('-').map_or(member, |(_, file)| file).starts_with(GENERATED_GLUE_PREFIX)
}

/// The symbols in `llvm-nm` output, leaving out the members `skip` names.
///
/// Weak undefined references need no definition and local symbols cannot be imported, so neither counts.
pub fn parse(text: &str, skip: impl Fn(&str) -> bool) -> Symbols {
    let mut symbols = Symbols::default();
    let mut skipping = false;
    for line in text.lines() {
        if let Some(member) = line.strip_suffix(':').filter(|m| !m.is_empty() && !m.starts_with(' ')) {
            skipping = skip(member);
            continue;
        }
        if skipping {
            continue;
        }
        match line.split_whitespace().collect::<Vec<_>>()[..] {
            ["U", name] => {
                symbols.undefined.insert(name.to_owned());
            },
            [_, kind, name] if kind.len() == 1 && kind.bytes().all(|b| b.is_ascii_uppercase() && b != b'U') => {
                symbols.defined.insert(name.to_owned());
            },
            _ => {},
        }
    }
    symbols
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    const ARCHIVE: &str = "\
aaaa-main.o:
                 U mutex_lock
                 U helper
                 w weak_thing
0000000000000000 T helper
0000000000000010 t local_only
0000000000000000 D __lx_initcall_4_init

bbbb-generated_data.o:
                 U lx_stub_hit
0000000000000000 B jiffies

cccc-core.o:
                 U kfree
";

    #[test]
    fn undefined_minus_defined_is_what_the_archive_imports() {
        let symbols = parse(ARCHIVE, is_generated_member);
        check!(symbols.undefined.iter().map(String::as_str).collect::<Vec<_>>() == ["helper", "kfree", "mutex_lock"]);
        check!(symbols.defined.iter().map(String::as_str).collect::<Vec<_>>() == ["__lx_initcall_4_init", "helper"]);
    }

    #[test]
    fn generated_members_count_as_neither_defined_nor_undefined() {
        let all = parse(ARCHIVE, |_| false);
        check!(all.defined.contains("jiffies"));
        check!(all.undefined.contains("lx_stub_hit"));
        let kept = parse(ARCHIVE, is_generated_member);
        check!(!kept.defined.contains("jiffies"));
        check!(!kept.undefined.contains("lx_stub_hit"));
    }

    #[test]
    fn only_the_members_named_generated_are_skipped() {
        check!(is_generated_member("fc395df20a5df568-generated_variadic.o"));
        check!(!is_generated_member("fc395df20a5df568-main.o"));
        check!(!is_generated_member("fc395df20a5df568-regenerated_x.o"));
    }
}
