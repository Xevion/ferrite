//! The part of clang's JSON AST dump that classifies an import: what kind of declaration a name is, its
//! type and the header that declares it.

use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclKind {
    Function { variadic: bool },
    Data,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decl {
    pub kind: DeclKind,
    /// clang's spelling of the type, like `int (const char *, ...)` for a function.
    pub qual_type: String,
    pub file: String,
}

/// A top-level node of an `-ast-dump-filter` dump. Everything below it is skipped unread.
#[derive(Debug, Deserialize)]
struct Node {
    kind: Option<String>,
    name: Option<String>,
    loc: Option<Loc>,
    #[serde(rename = "type")]
    ty: Option<TypeInfo>,
    variadic: Option<bool>,
    #[serde(rename = "isImplicit")]
    implicit: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct TypeInfo {
    #[serde(rename = "qualType")]
    qual_type: Option<String>,
}

/// A location. One written inside a macro expansion splits into where the text is spelled and where it
/// expands, and a file is only named when it differs from the one before it.
#[derive(Debug, Deserialize)]
struct Loc {
    file: Option<String>,
    #[serde(rename = "spellingLoc")]
    spelling: Option<Box<Self>>,
    #[serde(rename = "expansionLoc")]
    expansion: Option<Box<Self>>,
}

impl Loc {
    /// The file the declaration is written in: where a macro expands, else where it is spelled.
    fn file(&self) -> Option<&str> {
        let expansion = self.expansion.as_deref().and_then(Self::file);
        expansion.or_else(|| self.spelling.as_deref().and_then(Self::file)).or(self.file.as_deref())
    }
}

/// The first real declaration of `name` in a dump, which is the one in the header that declares it.
///
/// `dump` holds one JSON document per declaration whose name contains the filter, so most of them are
/// other names and other kinds of declaration.
pub fn first_declaration(name: &str, dump: &[u8]) -> Result<Option<Decl>, serde_json::Error> {
    for node in serde_json::Deserializer::from_slice(dump).into_iter::<Node>() {
        let node = node?;
        if node.name.as_deref() != Some(name) || node.implicit == Some(true) {
            continue;
        }
        let kind = match node.kind.as_deref() {
            Some("FunctionDecl") => DeclKind::Function { variadic: node.variadic == Some(true) },
            Some("VarDecl") => DeclKind::Data,
            _ => continue,
        };
        let Some(file) = node.loc.as_ref().and_then(Loc::file) else { continue };
        let qual_type = node.ty.and_then(|ty| ty.qual_type).unwrap_or_default();
        return Ok(Some(Decl { kind, qual_type, file: file.to_owned() }));
    }
    Ok(None)
}

/// A C prototype for a function named `name` of clang type `qual_type`, for people to read.
pub fn prototype(name: &str, qual_type: &str) -> String {
    let body = qual_type.find(" __attribute__").map_or(qual_type, |at| &qual_type[..at]);
    let Some(open) = parameter_list(body) else { return format!("{qual_type} {name}") };
    let (returns, parameters) = body.split_at(open);
    let returns = returns.trim_end();
    let space = if returns.ends_with('*') { "" } else { " " };
    format!("{returns}{space}{name}{parameters}")
}

/// Where the trailing parenthesised group of a function type starts.
fn parameter_list(body: &str) -> Option<usize> {
    if !body.ends_with(')') {
        return None;
    }
    let mut depth = 0_usize;
    for (at, c) in body.char_indices().rev() {
        match c {
            ')' => depth += 1,
            '(' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            },
            _ => {},
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use assert2::{assert, check};
    use rstest::rstest;

    use super::*;

    const DUMP: &str = r#"
{"kind":"RecordDecl","name":"jiffies","loc":{"file":"linux/include/a.h","line":1}}
{"kind":"FunctionDecl","name":"jiffies_to_msecs","loc":{"file":"linux/include/linux/jiffies.h"},"type":{"qualType":"unsigned int (long)"}}
{"kind":"FunctionDecl","name":"jiffies","isImplicit":true,"type":{"qualType":"int ()"}}
{"kind":"VarDecl","name":"jiffies","loc":{"file":"linux/include/linux/jiffies.h","line":86},"type":{"qualType":"volatile unsigned long"},
 "inner":[{"kind":"Deep","loc":{"file":"elsewhere.h"}}]}
{"kind":"VarDecl","name":"jiffies","loc":{"file":"linux/include/linux/later.h"},"type":{"qualType":"int"}}
"#;

    #[test]
    fn the_first_real_declaration_of_the_exact_name_wins() {
        assert!(let Ok(Some(decl)) = first_declaration("jiffies", DUMP.as_bytes()));
        check!(decl.kind == DeclKind::Data);
        check!(decl.file == "linux/include/linux/jiffies.h");
        check!(decl.qual_type == "volatile unsigned long");
    }

    #[test]
    fn variadic_functions_say_so() {
        let dump = r#"{"kind":"FunctionDecl","name":"_printk","variadic":true,"loc":{"file":"p.h"},"type":{"qualType":"int (const char *, ...)"}}"#;
        assert!(let Ok(Some(decl)) = first_declaration("_printk", dump.as_bytes()));
        check!(decl.kind == DeclKind::Function { variadic: true });
        let plain =
            r#"{"kind":"FunctionDecl","name":"kfree","loc":{"file":"s.h"},"type":{"qualType":"void (const void *)"}}"#;
        assert!(let Ok(Some(decl)) = first_declaration("kfree", plain.as_bytes()));
        check!(decl.kind == DeclKind::Function { variadic: false });
    }

    #[test]
    fn a_macro_declaration_is_filed_under_where_it_expands() {
        let dump = r#"{"kind":"VarDecl","name":"x","loc":{"spellingLoc":{"file":"<scratch space>"},"expansionLoc":{"file":"linux/include/y.h"}}}"#;
        assert!(let Ok(Some(decl)) = first_declaration("x", dump.as_bytes()));
        check!(decl.file == "linux/include/y.h");
        let same_file =
            r#"{"kind":"VarDecl","name":"x","loc":{"spellingLoc":{"file":"z.h"},"expansionLoc":{"line":3}}}"#;
        assert!(let Ok(Some(decl)) = first_declaration("x", same_file.as_bytes()));
        check!(decl.file == "z.h");
    }

    #[test]
    fn a_name_that_is_not_declared_is_none_and_garbage_is_an_error() {
        check!(first_declaration("nope", DUMP.as_bytes()).ok() == Some(None));
        check!(first_declaration("x", b"not json").is_err());
        check!(first_declaration("x", b"").ok() == Some(None));
    }

    #[rstest]
    #[case::plain("int (const char *, ...)", "_printk", "int _printk(const char *, ...)")]
    #[case::pointer_return("void *(unsigned long)", "kmalloc", "void *kmalloc(unsigned long)")]
    #[case::attributes_after("void (void) __attribute__((noreturn))", "lx_bug", "void lx_bug(void)")]
    #[case::pointer_parameter("void (int (*)(void *), void *)", "f", "void f(int (*)(void *), void *)")]
    #[case::not_a_function("int[]", "x", "int[] x")]
    fn prototypes_put_the_name_before_the_parameters(#[case] qual_type: &str, #[case] name: &str, #[case] want: &str) {
        check!(prototype(name, qual_type) == want);
    }
}
