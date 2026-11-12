//! Which C files Kbuild builds in a directory, and the flags it gives each.
//!
//! The directory's own Makefile is evaluated by `make` with the generated `.config` loaded, so
//! conditionals, composite objects (`foo-y := a.o b.o`) and `ccflags-y` mean what they mean to Kbuild.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use snafu::{ResultExt, ensure};

use super::kernel::Tree;
use super::{IoSnafu, NotCSnafu, VendorError, exec};

/// Expands `obj-y` into the `.o` files it builds (their composite's name travelling with each) and the
/// subdirectories it descends into, and prints them with the directory's flags.
const WRAPPER: &str = "\
include $(KCONFIG)
src := $(SRC)
obj := $(OBJDIR)
ifneq ($(wildcard $(TREE)/$(OBJDIR)/Kbuild),)
include $(TREE)/$(OBJDIR)/Kbuild
else
include $(TREE)/$(OBJDIR)/Makefile
endif
resolve = $(foreach o,$(1),$(if $(filter %/,$(o)),,$(if $($(o:.o=)-y)$($(o:.o=)-objs),$(call resolve,$($(o:.o=)-y) $($(o:.o=)-objs)),$(o))))
$(foreach t,$(filter-out %/,$(obj-y)),$(foreach l,$(call resolve,$(t)),$(info LEAF|$(t:.o=)|$(l)|$(ccflags-y) $(CFLAGS_$(l)))))
$(foreach d,$(filter %/,$(obj-y)),$(info DIR|$(d)))
dump:
\t@:
";

/// One C file Kbuild builds, with the arguments Kbuild adds to its compile line beyond the global ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KbuildFile {
    /// Relative to the Linux root.
    pub source: String,
    pub flags: Vec<String>,
}

/// A parsed line of the wrapper's output.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Line<'a> {
    Leaf { module: &'a str, object: &'a str, ccflags: Vec<&'a str> },
    Dir(&'a str),
}

fn parse_dump(output: &str) -> Vec<Line<'_>> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('|');
            match (fields.next()?, fields.next(), fields.next(), fields.next()) {
                ("LEAF", Some(module), Some(object), Some(flags)) => {
                    Some(Line::Leaf { module, object, ccflags: flags.split_whitespace().collect() })
                },
                ("DIR", Some(dir), None, None) => Some(Line::Dir(dir)),
                _ => None,
            }
        })
        .collect()
}

/// Kbuild's name mangling: `-` and `,` become `_`.
fn name_fix(name: &str) -> String {
    name.replace(['-', ','], "_")
}

/// The per-object defines Kbuild passes, as separate arguments.
pub fn kbuild_defines(dir: &str, module: &str, stem: &str) -> Vec<String> {
    vec![
        format!("-DKBUILD_BASENAME=\"{}\"", name_fix(stem)),
        format!("-DKBUILD_MODNAME=\"{}\"", name_fix(module)),
        format!("-D__KBUILD_MODNAME=kmod_{}", name_fix(module)),
        format!("-DKBUILD_MODFILE=\"{dir}/{module}\""),
    ]
}

/// Every C file the Makefile in each of `dirs` (and the directories it descends into) builds in.
pub fn evaluate(tree: &Tree, work: &Path, dirs: &[String]) -> Result<Vec<KbuildFile>, VendorError> {
    let wrapper = work.join("kbuild-wrapper.mk");
    fs::write(&wrapper, WRAPPER).context(IoSnafu { what: "write", path: &wrapper })?;
    let mut files: Vec<KbuildFile> = Vec::new();
    let mut seen = BTreeSet::new();
    let mut pending: Vec<String> = dirs.iter().rev().cloned().collect();
    while let Some(dir) = pending.pop() {
        let mut make = Command::new("make");
        make.args(["-s", "--no-print-directory", "-f"]).arg(&wrapper).arg("dump");
        make.arg(format!("KCONFIG={}", tree.dotconfig().display()));
        make.arg(format!("TREE={}", tree.src.display()));
        make.arg(format!("OBJDIR={dir}"));
        make.arg(format!("SRC=linux/{dir}"));
        let output = exec(&mut make, &format!("evaluate the Kbuild file of {dir}"))?;
        let mut subdirs = Vec::new();
        for line in parse_dump(&output) {
            match line {
                Line::Dir(sub) => subdirs.push(format!("{dir}/{}", sub.trim_end_matches('/'))),
                Line::Leaf { module, object, ccflags } => {
                    let stem = object.trim_end_matches(".o");
                    let source = format!("{dir}/{stem}.c");
                    ensure!(tree.src.join(&source).is_file(), NotCSnafu { dir: &dir, object });
                    if seen.insert(source.clone()) {
                        let mut flags = kbuild_defines(&dir, module, stem);
                        flags.extend(ccflags.into_iter().map(str::to_owned));
                        files.push(KbuildFile { source, flags });
                    }
                },
            }
        }
        pending.extend(subdirs.into_iter().rev());
    }
    Ok(files)
}

/// How `source.list` differs from what Kbuild builds in the Kbuild directories.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Drift {
    /// Built by Kbuild, neither listed nor cut.
    pub missing: Vec<String>,
    /// Listed under a Kbuild directory, but not built by Kbuild with this config.
    pub extra: Vec<String>,
}

impl Drift {
    pub const fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty()
    }
}

pub fn drift(kbuild: &[KbuildFile], cut: &[String], listed: &[String], dirs: &[String]) -> Drift {
    let built: BTreeSet<&str> = kbuild.iter().map(|f| f.source.as_str()).collect();
    let listed_set: BTreeSet<&str> = listed.iter().map(String::as_str).collect();
    let under_kbuild =
        |file: &str| dirs.iter().any(|dir| file.strip_prefix(dir.as_str()).is_some_and(|r| r.starts_with('/')));
    Drift {
        missing: kbuild
            .iter()
            .map(|f| f.source.as_str())
            .filter(|f| !listed_set.contains(f) && !cut.iter().any(|c| c == f))
            .map(str::to_owned)
            .collect(),
        extra: listed.iter().filter(|f| under_kbuild(f) && !built.contains(f.as_str())).cloned().collect(),
    }
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};

    use super::*;

    #[test]
    fn the_wrappers_output_is_parsed_into_leaves_and_directories() {
        let output = "LEAF|mac80211|main.o|-DDEBUG -Inet/x\nLEAF|rtw89_core|core.o| \nDIR|tests/\nnoise\n";
        let lines = parse_dump(output);
        assert!(
            lines
                == [
                    Line::Leaf { module: "mac80211", object: "main.o", ccflags: vec!["-DDEBUG", "-Inet/x"] },
                    Line::Leaf { module: "rtw89_core", object: "core.o", ccflags: vec![] },
                    Line::Dir("tests/"),
                ]
        );
    }

    #[test]
    fn kbuild_names_have_dashes_turned_into_underscores_and_strings_quoted() {
        check!(
            kbuild_defines("net/mac80211", "mac80211", "agg-tx")
                == [
                    "-DKBUILD_BASENAME=\"agg_tx\"",
                    "-DKBUILD_MODNAME=\"mac80211\"",
                    "-D__KBUILD_MODNAME=kmod_mac80211",
                    "-DKBUILD_MODFILE=\"net/mac80211/mac80211\"",
                ]
        );
    }

    #[test]
    fn drift_names_files_on_either_side_and_honours_cuts() {
        let built = |s: &str| KbuildFile { source: s.to_owned(), flags: vec![] };
        let kbuild = [built("net/a/x.c"), built("net/a/nl.c"), built("net/a/y.c")];
        let dirs = ["net/a".to_owned()];
        let listed = ["net/a/x.c", "net/a/gone.c", "lib/extra.c"].map(str::to_owned);
        let got = drift(&kbuild, &["net/a/nl.c".to_owned()], &listed, &dirs);
        check!(got.missing == ["net/a/y.c"]);
        check!(got.extra == ["net/a/gone.c"]);
        check!(drift(&kbuild, &[], &kbuild.iter().map(|f| f.source.clone()).collect::<Vec<_>>(), &dirs).is_empty());
    }
}
