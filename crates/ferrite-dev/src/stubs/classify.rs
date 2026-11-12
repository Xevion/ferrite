//! What each import is, from its declaration.

use std::collections::BTreeMap;

use super::ast::{Decl, DeclKind, prototype};
use super::include::Include;

/// Imports the compiler and the host's libc supply: the kernel's string functions do the same job, and
/// defining them here would replace the ones every other call in the process uses.
const HOST: [&str; 6] = ["bcmp", "memcmp", "memcpy", "memmove", "memset", "strlen"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Class {
    /// A non-variadic function: a Rust stub.
    Function,
    /// A variadic function: a C stub, which Rust cannot define.
    Variadic,
    /// A variable: zeroed storage in C, in the read-only region when the C type is `const` itself. An array
    /// declared without a length (`char[]`) is given one element.
    Data { read_only: bool, open_array: bool },
    /// A function only a cut file defines, the one named. A Rust stub like any function.
    Seam { cut: String },
    /// Supplied by the host, so never defined here.
    Host,
    /// No declaration found, so nothing is generated.
    Unknown,
}

impl Class {
    pub const LABELS: [&'static str; 6] = ["function", "variadic", "data", "seam", "host", "unknown"];

    pub const fn label(&self) -> &'static str {
        match self {
            Self::Function => Self::LABELS[0],
            Self::Variadic => Self::LABELS[1],
            Self::Data { .. } => Self::LABELS[2],
            Self::Seam { .. } => Self::LABELS[3],
            Self::Host => Self::LABELS[4],
            Self::Unknown => Self::LABELS[5],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Import {
    /// The symbol as the objects import it, after any rename.
    pub symbol: String,
    pub class: Class,
    /// For a function, its C prototype under the symbol's name.
    pub prototype: String,
    /// The header that declares it, when one does.
    pub header: Option<Include>,
}

/// What classifying needs to know about the build.
#[derive(Debug)]
pub struct Context<'a> {
    pub roots: &'a [String],
    /// Renamed imports: the shim's name mapped to the libc name the headers declare.
    pub renames: &'a BTreeMap<String, String>,
    /// The symbols the cut Linux sources define, each with the source that defines it.
    pub seams: &'a BTreeMap<String, String>,
}

impl Context<'_> {
    /// The name the headers declare for `symbol`.
    pub fn declared_name<'s>(&'s self, symbol: &'s str) -> &'s str {
        self.renames.get(symbol).map_or(symbol, String::as_str)
    }
}

/// Whether a variable of the clang type `qual_type` is itself `const`: an array by its element type, a
/// pointer by a `const` after its last `*`, anything else by a `const` anywhere.
fn is_read_only(qual_type: &str) -> bool {
    let element = qual_type.split('[').next().unwrap_or(qual_type).trim();
    match element.rsplit_once('*') {
        Some((_, after)) => after.split_whitespace().any(|word| word == "const"),
        None => element.split_whitespace().any(|word| word == "const"),
    }
}

pub fn classify(symbol: &str, decl: Option<&Decl>, ctx: &Context<'_>) -> Import {
    let bare = |class| Import { symbol: symbol.to_owned(), class, prototype: String::new(), header: None };
    if HOST.contains(&symbol) {
        return bare(Class::Host);
    }
    let Some(decl) = decl else { return bare(Class::Unknown) };
    let header = Some(Include::resolve(&decl.file, ctx.roots));
    let (class, prototype) = match decl.kind {
        DeclKind::Data => {
            let (read_only, open_array) = (is_read_only(&decl.qual_type), decl.qual_type.trim_end().ends_with("[]"));
            (Class::Data { read_only, open_array }, String::new())
        },
        DeclKind::Function { variadic: true } => (Class::Variadic, prototype(symbol, &decl.qual_type)),
        DeclKind::Function { variadic: false } => {
            let class = ctx.seams.get(symbol).map_or(Class::Function, |cut| Class::Seam { cut: cut.clone() });
            (class, prototype(symbol, &decl.qual_type))
        },
    };
    Import { symbol: symbol.to_owned(), class, prototype, header }
}

#[cfg(test)]
mod tests {
    use assert2::check;

    use super::*;

    fn decl(kind: DeclKind, qual_type: &str, file: &str) -> Decl {
        Decl { kind, qual_type: qual_type.to_owned(), file: file.to_owned() }
    }

    fn with_context<R>(run: impl FnOnce(&Context<'_>) -> R) -> R {
        let roots = vec!["linux/include".to_owned()];
        let renames = BTreeMap::from([("lx_snprintf".to_owned(), "snprintf".to_owned())]);
        let seams = BTreeMap::from([("nl80211_notify".to_owned(), "net/wireless/nl80211.c".to_owned())]);
        run(&Context { roots: &roots, renames: &renames, seams: &seams })
    }

    #[test]
    fn each_declaration_kind_lands_in_its_class() {
        with_context(|ctx| {
            let function =
                decl(DeclKind::Function { variadic: false }, "void (const void *)", "linux/include/linux/slab.h");
            let got = classify("kfree", Some(&function), ctx);
            check!(got.class == Class::Function);
            check!(got.prototype == "void kfree(const void *)");
            check!(got.header.map(|h| h.name) == Some("linux/slab.h".to_owned()));

            let variadic =
                decl(DeclKind::Function { variadic: true }, "int (const char *, ...)", "linux/include/linux/printk.h");
            check!(classify("_printk", Some(&variadic), ctx).class == Class::Variadic);

            let data = decl(DeclKind::Data, "volatile unsigned long", "linux/include/linux/jiffies.h");
            let got = classify("jiffies", Some(&data), ctx);
            check!(got.class == Class::Data { read_only: false, open_array: false });
            check!(got.prototype.is_empty());
        });
    }

    #[rstest::rstest]
    #[case::plain("volatile unsigned long", false)]
    #[case::const_struct("const struct kernel_param_ops", true)]
    #[case::const_array("const unsigned long[65][1]", true)]
    #[case::array_of_const_pointers("const char *const[32]", true)]
    #[case::array_of_pointers_to_const("const char *[32]", false)]
    #[case::pointer_to_const("const struct x *", false)]
    fn read_only_follows_the_object_not_what_it_points_at(#[case] qual_type: &str, #[case] read_only: bool) {
        check!(is_read_only(qual_type) == read_only);
    }

    #[test]
    fn a_function_only_a_cut_source_defines_is_a_seam() {
        with_context(|ctx| {
            let seam = decl(DeclKind::Function { variadic: false }, "void (int)", "linux/net/wireless/nl80211.h");
            check!(
                classify("nl80211_notify", Some(&seam), ctx).class
                    == Class::Seam { cut: "net/wireless/nl80211.c".to_owned() }
            );
            let other = decl(DeclKind::Function { variadic: false }, "void (int)", "linux/net/wireless/nl80211.h");
            check!(classify("cfg80211_x", Some(&other), ctx).class == Class::Function);
        });
    }

    #[test]
    fn host_names_and_undeclared_names_generate_nothing() {
        with_context(|ctx| {
            let declared = decl(
                DeclKind::Function { variadic: false },
                "void *(void *, int, unsigned long)",
                "linux/include/linux/string.h",
            );
            check!(classify("memset", Some(&declared), ctx).class == Class::Host);
            check!(classify("__x86_return_thunk", None, ctx).class == Class::Unknown);
        });
    }

    #[test]
    fn a_renamed_import_is_declared_under_its_libc_name() {
        with_context(|ctx| {
            check!(ctx.declared_name("lx_snprintf") == "snprintf");
            check!(ctx.declared_name("kfree") == "kfree");
        });
    }
}
