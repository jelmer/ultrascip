//! C/C++ source scanner for JNI exports.
//!
//! Finds the two ways a native library exposes implementations of Java
//! `native` methods:
//!
//! * Exported functions following the JNI name mangling scheme
//!   (`JNIEXPORT jint JNICALL Java_com_example_Foo_bar(...)`). The Java
//!   class and method are recovered by demangling the identifier; an
//!   overload signature suffix (`__I`) is stripped.
//! * `JNINativeMethod` table initializers registered via `RegisterNatives`,
//!   each entry mapping a Java method name to a C function. The Java class
//!   is taken from a `FindClass` string literal in the same file, when
//!   exactly one distinct class is looked up there; otherwise the entries
//!   are dropped (best effort).
//!
//! `JNIEXPORT`/`JNICALL` are macros the parser has no definition for, so
//! exported functions are matched by identifier shape rather than by a
//! clean `function_definition` structure; tree-sitter's error recovery
//! still yields the `Java_*` declarator as an identifier node.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone)]
pub struct JniExport {
    pub relative_path: PathBuf,
    /// Class binary-name components: package parts, then the class itself
    /// (which may contain `$` for nested classes), e.g.
    /// `["com", "example", "Foo"]`.
    pub class: Vec<String>,
    /// Java-facing method name.
    pub java_name: String,
    /// C identifier implementing the method.
    pub c_name: String,
    /// Zero-based `(line, start_col, end_col)` -- the range of the mangled
    /// identifier, or of the name string literal for table entries.
    pub name_span: (u32, u32, u32),
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub exports: Vec<JniExport>,
}

/// A demangled JNI export name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JniName {
    pub class: Vec<String>,
    pub method: String,
}

/// Demangle a `Java_*` exported function name into its class components and
/// method name. Returns None for identifiers that don't follow the scheme.
///
/// The JNI escapes are `_1` for `_`, `_2` for `;`, `_3` for `[` and
/// `_0xxxx` for an arbitrary unicode character; `__` separates an
/// overloaded method's name from its mangled signature, which is discarded.
pub fn demangle_jni(name: &str) -> Option<JniName> {
    let rest = name.strip_prefix("Java_")?;
    let mut components = vec![String::new()];
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '_' {
            components.last_mut().unwrap().push(c);
            continue;
        }
        match chars.peek() {
            Some('1') => {
                chars.next();
                components.last_mut().unwrap().push('_');
            }
            Some('2') => {
                chars.next();
                components.last_mut().unwrap().push(';');
            }
            Some('3') => {
                chars.next();
                components.last_mut().unwrap().push('[');
            }
            Some('0') => {
                chars.next();
                let hex: String = (0..4).filter_map(|_| chars.next()).collect();
                if hex.len() != 4 {
                    return None;
                }
                let cp = u32::from_str_radix(&hex, 16).ok()?;
                components.last_mut().unwrap().push(char::from_u32(cp)?);
            }
            // `__` separates the method name from an overload signature.
            Some('_') => break,
            _ => components.push(String::new()),
        }
    }
    if components.len() < 2 || components.iter().any(String::is_empty) {
        return None;
    }
    let method = components.pop().unwrap();
    Some(JniName {
        class: components,
        method,
    })
}

pub fn scan_file(path: &Path, source_root: &Path, out: &mut ScanResult) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    // Cheap prefilter -- files with no JNI entry points are the vast
    // majority in most trees.
    if !text.contains("Java_") && !text.contains("JNINativeMethod") {
        return Ok(());
    }

    let mut parser = Parser::new();
    let lang: Language = if is_cpp_source(path) {
        tree_sitter_cpp::LANGUAGE.into()
    } else {
        tree_sitter_c::LANGUAGE.into()
    };
    parser
        .set_language(&lang)
        .context("setting tree-sitter language")?;
    let Some(tree) = parser.parse(&text, None) else {
        return Ok(());
    };
    let src = text.as_bytes();

    let rel = path
        .strip_prefix(source_root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf());

    // Table entries can only be attributed to a class through the file's
    // FindClass lookups, so collect those and the tables in one walk, then
    // resolve at the end.
    let mut classes: Vec<String> = Vec::new();
    let mut table_entries: Vec<TableEntry> = Vec::new();
    walk(
        tree.root_node(),
        src,
        &rel,
        out,
        &mut classes,
        &mut table_entries,
    );

    classes.sort();
    classes.dedup();
    if let [class] = classes.as_slice() {
        let class: Vec<String> = class.split('/').map(str::to_string).collect();
        if !class.is_empty() && !class.iter().any(String::is_empty) {
            for entry in table_entries {
                out.exports.push(JniExport {
                    relative_path: rel.clone(),
                    class: class.clone(),
                    java_name: entry.java_name,
                    c_name: entry.c_name,
                    name_span: entry.name_span,
                });
            }
        }
    }

    Ok(())
}

fn is_cpp_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("cc") | Some("cpp") | Some("cxx") | Some("hpp") | Some("hh")
    )
}

#[derive(Debug)]
struct TableEntry {
    java_name: String,
    c_name: String,
    name_span: (u32, u32, u32),
}

fn walk(
    node: Node,
    src: &[u8],
    rel: &Path,
    out: &mut ScanResult,
    classes: &mut Vec<String>,
    table_entries: &mut Vec<TableEntry>,
) {
    match node.kind() {
        "identifier" => {
            collect_exported_fn(node, src, rel, out);
        }
        "declaration" => {
            if declares_type(node, src, "JNINativeMethod") {
                collect_native_method_table(node, src, table_entries);
            }
        }
        "call_expression" => {
            if let Some(class) = find_class_argument(node, src) {
                classes.push(class);
            }
        }
        _ => {}
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk(cursor.node(), src, rel, out, classes, table_entries);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// Record an exported `Java_*` function declarator. The macro soup around
/// JNI signatures means the identifier may sit under a function_declarator,
/// a call_expression (error recovery) or a bare ERROR node.
fn collect_exported_fn(ident: Node, src: &[u8], rel: &Path, out: &mut ScanResult) {
    let Some(parent) = ident.parent() else {
        return;
    };
    if !matches!(
        parent.kind(),
        "function_declarator" | "call_expression" | "ERROR"
    ) {
        return;
    }
    let name = slice(src, ident);
    let Some(demangled) = demangle_jni(&name) else {
        return;
    };
    let start = ident.start_position();
    out.exports.push(JniExport {
        relative_path: rel.to_path_buf(),
        class: demangled.class,
        java_name: demangled.method,
        c_name: name.clone(),
        name_span: (
            start.row as u32,
            start.column as u32,
            (start.column + name.len()) as u32,
        ),
    });
}

/// Whether a declaration's type (plain or `struct`-qualified) is `type_name`.
fn declares_type(node: Node, src: &[u8], type_name: &str) -> bool {
    let mut walker = node.walk();
    let found = node.children(&mut walker).any(|c| match c.kind() {
        "type_identifier" => slice(src, c) == type_name,
        "struct_specifier" => {
            let mut inner = c.walk();
            let matched = c
                .children(&mut inner)
                .any(|cc| cc.kind() == "type_identifier" && slice(src, cc) == type_name);
            matched
        }
        _ => false,
    });
    found
}

/// Collect the `{ "name", "(sig)V", (void*)fn }` entries of a
/// `JNINativeMethod` array initializer.
fn collect_native_method_table(decl: Node, src: &[u8], entries: &mut Vec<TableEntry>) {
    let mut walker = decl.walk();
    for init in decl
        .children(&mut walker)
        .filter(|c| c.kind() == "init_declarator")
    {
        let Some(outer_list) = init
            .children(&mut init.walk())
            .find(|c| c.kind() == "initializer_list")
        else {
            continue;
        };
        for entry in outer_list
            .children(&mut outer_list.walk())
            .filter(|c| c.kind() == "initializer_list")
        {
            if let Some(e) = parse_table_entry(entry, src) {
                entries.push(e);
            }
        }
    }
}

fn parse_table_entry(entry: Node, src: &[u8]) -> Option<TableEntry> {
    let mut java_name = None;
    let mut name_span = None;
    let mut c_name = None;
    let mut walker = entry.walk();
    for c in entry.children(&mut walker) {
        match c.kind() {
            // First string literal is the Java name; the second is the type
            // signature, which we don't need.
            "string_literal" if java_name.is_none() => {
                let raw = slice(src, c);
                let stripped = raw.trim_matches('"');
                java_name = Some(stripped.to_string());
                let start = c.start_position();
                let start_col = (start.column + 1) as u32;
                name_span = Some((
                    start.row as u32,
                    start_col,
                    start_col + stripped.len() as u32,
                ));
            }
            "cast_expression" | "identifier" if c_name.is_none() => {
                c_name = last_identifier(c, src);
            }
            _ => {}
        }
    }
    let (java_name, c_name, name_span) = (java_name?, c_name?, name_span?);
    // Skip sentinel entries like `{ NULL, NULL, NULL }`.
    if java_name.is_empty() || java_name == "NULL" || c_name == "NULL" {
        return None;
    }
    Some(TableEntry {
        java_name,
        c_name,
        name_span,
    })
}

/// For a call to `FindClass` (any receiver shape: `FindClass(...)`,
/// `env->FindClass(...)`, `(*env)->FindClass(...)`), return the class string
/// literal argument in slash form.
fn find_class_argument(call: Node, src: &[u8]) -> Option<String> {
    let callee = call.child(0)?;
    let callee_name = match callee.kind() {
        "identifier" => slice(src, callee),
        "field_expression" => callee
            .children(&mut callee.walk())
            .find(|c| c.kind() == "field_identifier")
            .map(|c| slice(src, c))?,
        _ => return None,
    };
    if callee_name != "FindClass" {
        return None;
    }
    let args = call.child_by_field_name("arguments")?;
    let mut walker = args.walk();
    let lit = args
        .children(&mut walker)
        .find(|c| c.kind() == "string_literal")?;
    let value = slice(src, lit).trim_matches('"').to_string();
    (!value.is_empty()).then_some(value)
}

/// The last identifier in an expression: handles a bare identifier and a
/// cast like `(void *)fn`.
fn last_identifier(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() == "identifier" {
        return Some(slice(src, node));
    }
    let mut last = None;
    let mut cursor = node.walk();
    let mut stack = vec![node];
    while let Some(n) = stack.pop() {
        for c in n.children(&mut cursor) {
            if c.kind() == "identifier" {
                last = Some(slice(src, c));
            }
            stack.push(c);
        }
    }
    last
}

fn slice(src: &[u8], node: Node) -> String {
    std::str::from_utf8(&src[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn scan_str(src: &str) -> ScanResult {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(src.as_bytes()).unwrap();
        let mut r = ScanResult::default();
        scan_file(f.path(), f.path().parent().unwrap(), &mut r).unwrap();
        r
    }

    #[test]
    fn demangles_plain_name() {
        assert_eq!(
            demangle_jni("Java_com_example_Foo_bar"),
            Some(JniName {
                class: vec!["com".into(), "example".into(), "Foo".into()],
                method: "bar".into(),
            })
        );
    }

    #[test]
    fn demangles_default_package() {
        assert_eq!(
            demangle_jni("Java_Foo_bar"),
            Some(JniName {
                class: vec!["Foo".into()],
                method: "bar".into(),
            })
        );
    }

    #[test]
    fn demangles_underscore_escape() {
        assert_eq!(
            demangle_jni("Java_com_example_Foo_do_1stuff"),
            Some(JniName {
                class: vec!["com".into(), "example".into(), "Foo".into()],
                method: "do_stuff".into(),
            })
        );
    }

    #[test]
    fn demangles_nested_class() {
        assert_eq!(
            demangle_jni("Java_com_example_Foo_00024Inner_bar"),
            Some(JniName {
                class: vec!["com".into(), "example".into(), "Foo$Inner".into()],
                method: "bar".into(),
            })
        );
    }

    #[test]
    fn strips_overload_signature() {
        assert_eq!(
            demangle_jni("Java_com_example_Foo_get__I"),
            Some(JniName {
                class: vec!["com".into(), "example".into(), "Foo".into()],
                method: "get".into(),
            })
        );
    }

    #[test]
    fn rejects_non_jni_names() {
        assert_eq!(demangle_jni("Java_"), None);
        assert_eq!(demangle_jni("Java_foo"), None);
        assert_eq!(demangle_jni("Javax_foo_bar"), None);
    }

    #[test]
    fn extracts_exported_function() {
        let src = r#"
            #include <jni.h>

            JNIEXPORT jint JNICALL
            Java_com_example_Counter_increment(JNIEnv *env, jobject obj, jint by) {
                return by + 1;
            }
        "#;
        let r = scan_str(src);
        assert_eq!(r.exports.len(), 1, "got: {:?}", r.exports);
        let e = &r.exports[0];
        assert_eq!(e.java_name, "increment");
        assert_eq!(e.class, vec!["com", "example", "Counter"]);
        assert_eq!(e.c_name, "Java_com_example_Counter_increment");
    }

    #[test]
    fn extracts_native_method_table() {
        let src = r#"
            #include <jni.h>

            static jint native_add(JNIEnv *env, jobject obj, jint a, jint b) {
                return a + b;
            }

            static const JNINativeMethod methods[] = {
                { "add", "(II)I", (void *)native_add },
            };

            jint JNI_OnLoad(JavaVM *vm, void *reserved) {
                JNIEnv *env;
                jclass cls = (*env)->FindClass(env, "com/example/Calc");
                (*env)->RegisterNatives(env, cls, methods, 1);
                return JNI_VERSION_1_6;
            }
        "#;
        let r = scan_str(src);
        assert_eq!(r.exports.len(), 1, "got: {:?}", r.exports);
        let e = &r.exports[0];
        assert_eq!(e.java_name, "add");
        assert_eq!(e.class, vec!["com", "example", "Calc"]);
        assert_eq!(e.c_name, "native_add");
    }

    #[test]
    fn table_without_findclass_is_dropped() {
        let src = r#"
            static const JNINativeMethod methods[] = {
                { "add", "(II)I", (void *)native_add },
            };
        "#;
        let r = scan_str(src);
        assert!(r.exports.is_empty(), "got: {:?}", r.exports);
    }

    #[test]
    fn cpp_findclass_receiver() {
        let src_path = {
            let mut f = tempfile::Builder::new().suffix(".cc").tempfile().unwrap();
            let src = r#"
                static const JNINativeMethod methods[] = {
                    { "mul", "(II)I", (void *)native_mul },
                };
                void reg(JNIEnv *env) {
                    jclass cls = env->FindClass("com/example/Calc");
                    env->RegisterNatives(cls, methods, 1);
                }
            "#;
            f.write_all(src.as_bytes()).unwrap();
            f
        };
        let mut r = ScanResult::default();
        scan_file(src_path.path(), src_path.path().parent().unwrap(), &mut r).unwrap();
        assert_eq!(r.exports.len(), 1, "got: {:?}", r.exports);
        assert_eq!(r.exports[0].class, vec!["com", "example", "Calc"]);
    }
}
