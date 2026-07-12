//! C/C++ source scanner for Ruby C extension exports.
//!
//! Parses sources with tree-sitter and extracts the module/class/method
//! registrations of the Ruby C API:
//!
//! * `rb_define_module("Foo")` / `rb_define_class("Foo", super)` and their
//!   `_under` variants. The variable a definition is assigned to is tracked
//!   so later registrations against it resolve to a qualified name
//!   (`Foo::Bar`).
//! * `rb_define_method(recv, "name", c_fn, arity)` and friends
//!   (`rb_define_module_function`, `rb_define_singleton_method`,
//!   `rb_define_private_method`, `rb_define_protected_method`,
//!   `rb_define_global_function`), mapping the Ruby name to the C function.
//!
//! A receiver that is not a tracked variable is resolved from the
//! conventional identifier prefixes (`rb_cObject` -> `Object`, `mFoo` ->
//! `Foo`, `cBar` -> `Bar`); one that resists both is used verbatim as a
//! last resort.
//!
//! Module and class exports carry the enclosing `Init_*` function as their
//! C identifier, so the emitted Ruby symbol can cross-reference the
//! extension entry point captured in the input SCIP index.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone)]
pub struct RubyExport {
    pub relative_path: PathBuf,
    /// Ruby namespace the export lives under (e.g. `Foo::Bar`); empty for a
    /// top-level module or class.
    pub namespace: String,
    /// Ruby-facing name (module/class name, or method name).
    pub ruby_name: String,
    /// C identifier the export wraps; empty when unknown (e.g. a module
    /// defined outside any `Init_*` function).
    pub c_name: String,
    pub kind: RubyExportKind,
    /// Zero-based `(line, start_col, end_col)` -- the range of the name's
    /// string literal (without quotes) in the source.
    pub name_span: (u32, u32, u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RubyExportKind {
    Module,
    Class,
    Method,
    /// A singleton (class-level) method: `rb_define_singleton_method` or
    /// `rb_define_module_function`.
    SingletonMethod,
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub exports: Vec<RubyExport>,
}

pub fn scan_file(path: &Path, source_root: &Path, out: &mut ScanResult) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    // Cheap prefilter -- files that define nothing through the Ruby C API
    // are the vast majority in most trees.
    if !text.contains("rb_define_") {
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

    let mut scan = FileScan {
        src,
        rel: &rel,
        vars: HashMap::new(),
    };
    scan.walk(tree.root_node(), None, out);
    Ok(())
}

fn is_cpp_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("cc") | Some("cpp") | Some("cxx") | Some("hpp") | Some("hh")
    )
}

struct FileScan<'a> {
    src: &'a [u8],
    rel: &'a Path,
    /// C variable -> qualified Ruby name, from tracked
    /// `rb_define_module`/`rb_define_class` assignments.
    vars: HashMap<String, String>,
}

/// A resolved `rb_define_module`/`rb_define_class` call.
struct DefineInfo {
    namespace: String,
    name: String,
    kind: RubyExportKind,
    name_span: (u32, u32, u32),
}

impl DefineInfo {
    fn qualified(&self) -> String {
        if self.namespace.is_empty() {
            self.name.clone()
        } else {
            format!("{}::{}", self.namespace, self.name)
        }
    }
}

impl<'a> FileScan<'a> {
    fn walk(&mut self, node: Node, init_fn: Option<&str>, out: &mut ScanResult) {
        // Definitions almost always happen inside the extension's `Init_*`
        // entry point; remember it so module/class exports can link to it.
        let own_init = if node.kind() == "function_definition" {
            function_name(node, self.src).filter(|n| n.starts_with("Init_"))
        } else {
            None
        };
        let init_fn = own_init.as_deref().or(init_fn);

        match node.kind() {
            // `VALUE mFoo = rb_define_module("Foo");` or `mFoo = ...` --
            // track the variable so later receivers resolve.
            "init_declarator" | "assignment_expression" => {
                self.track_assignment(node);
            }
            "call_expression" => {
                self.handle_call(node, init_fn, out);
            }
            _ => {}
        }

        let mut cursor = node.walk();
        if cursor.goto_first_child() {
            loop {
                self.walk(cursor.node(), init_fn, out);
                if !cursor.goto_next_sibling() {
                    break;
                }
            }
        }
    }

    fn track_assignment(&mut self, node: Node) {
        let mut var = None;
        let mut call = None;
        let mut walker = node.walk();
        for c in node.children(&mut walker) {
            match c.kind() {
                "identifier" if var.is_none() => var = Some(slice(self.src, c)),
                "call_expression" => call = Some(c),
                _ => {}
            }
        }
        let (Some(var), Some(call)) = (var, call) else {
            return;
        };
        if let Some(info) = self.define_call_info(call) {
            self.vars.insert(var, info.qualified());
        }
    }

    fn handle_call(&mut self, call: Node, init_fn: Option<&str>, out: &mut ScanResult) {
        // Module/class definitions are emitted at the call site (assignments
        // only track the variable), so a bare `rb_define_module("Foo");` is
        // covered too.
        if let Some(info) = self.define_call_info(call) {
            out.exports.push(RubyExport {
                relative_path: self.rel.to_path_buf(),
                namespace: info.namespace.clone(),
                ruby_name: info.name.clone(),
                c_name: init_fn.unwrap_or("").to_string(),
                kind: info.kind,
                name_span: info.name_span,
            });
            return;
        }

        let Some(fn_name) = call_fn_name(call, self.src) else {
            return;
        };
        // (receiver_arg, name_arg, fn_arg, kind); global functions live on
        // Kernel and take no receiver.
        let (recv_idx, name_idx, fn_idx, kind) = match fn_name.as_str() {
            "rb_define_method" | "rb_define_private_method" | "rb_define_protected_method" => {
                (Some(0), 1, 2, RubyExportKind::Method)
            }
            "rb_define_module_function" | "rb_define_singleton_method" => {
                (Some(0), 1, 2, RubyExportKind::SingletonMethod)
            }
            "rb_define_global_function" => (None, 0, 1, RubyExportKind::Method),
            _ => return,
        };
        let args = call_args(call);
        let Some((name, span)) = args.get(name_idx).and_then(|n| string_value(*n, self.src)) else {
            return;
        };
        let Some(c_name) = args.get(fn_idx).and_then(|n| last_identifier(*n, self.src)) else {
            return;
        };
        let namespace = match recv_idx {
            Some(i) => {
                let Some(recv) = args.get(i) else {
                    return;
                };
                self.resolve_receiver(*recv)
            }
            None => "Kernel".to_string(),
        };
        out.exports.push(RubyExport {
            relative_path: self.rel.to_path_buf(),
            namespace,
            ruby_name: name,
            c_name,
            kind,
            name_span: span,
        });
    }

    fn define_call_info(&self, call: Node) -> Option<DefineInfo> {
        let fn_name = call_fn_name(call, self.src)?;
        let (kind, under) = match fn_name.as_str() {
            "rb_define_module" => (RubyExportKind::Module, false),
            "rb_define_module_under" => (RubyExportKind::Module, true),
            "rb_define_class" => (RubyExportKind::Class, false),
            "rb_define_class_under" => (RubyExportKind::Class, true),
            _ => return None,
        };
        let args = call_args(call);
        let (namespace, name_arg) = if under {
            (self.resolve_receiver(*args.first()?), *args.get(1)?)
        } else {
            (String::new(), *args.first()?)
        };
        let (name, name_span) = string_value(name_arg, self.src)?;
        Some(DefineInfo {
            namespace,
            name,
            kind,
            name_span,
        })
    }

    /// Resolve a receiver expression to a Ruby namespace name.
    fn resolve_receiver(&self, node: Node) -> String {
        // A nested definition used directly as the receiver:
        // `rb_define_method(rb_define_module("Foo"), ...)`.
        if node.kind() == "call_expression" {
            if let Some(info) = self.define_call_info(node) {
                return info.qualified();
            }
        }
        let ident = slice(self.src, node);
        if let Some(ns) = self.vars.get(&ident) {
            return ns.clone();
        }
        receiver_from_convention(&ident).unwrap_or(ident)
    }
}

/// Resolve a receiver identifier from naming convention: ruby-internal
/// `rb_cObject`/`rb_mKernel`/`rb_eStandardError`, or the extension-local
/// `cFoo`/`mBar`/`eBaz` style.
fn receiver_from_convention(ident: &str) -> Option<String> {
    let rest = ident
        .strip_prefix("rb_")
        .unwrap_or(ident)
        .strip_prefix(['c', 'm', 'e'])?;
    rest.starts_with(char::is_uppercase)
        .then(|| rest.to_string())
}

fn function_name(func: Node, src: &[u8]) -> Option<String> {
    let mut walker = func.walk();
    let decl = func
        .children(&mut walker)
        .find(|c| c.kind() == "function_declarator")?;
    decl.children(&mut decl.walk())
        .find(|c| c.kind() == "identifier")
        .map(|c| slice(src, c))
}

/// The callee identifier of a call_expression, or None for anything more
/// complex than a bare identifier.
fn call_fn_name(call: Node, src: &[u8]) -> Option<String> {
    let f = call.child(0)?;
    (f.kind() == "identifier").then(|| slice(src, f))
}

/// The argument nodes of a call_expression, punctuation stripped.
fn call_args(call: Node) -> Vec<Node> {
    let Some(args) = call.child_by_field_name("arguments") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut walker = args.walk();
    for c in args.children(&mut walker) {
        if c.is_named() && c.kind() != "comment" {
            out.push(c);
        }
    }
    out
}

/// The contents and span (without quotes) of a string_literal node.
fn string_value(node: Node, src: &[u8]) -> Option<(String, (u32, u32, u32))> {
    if node.kind() != "string_literal" {
        return None;
    }
    let raw = slice(src, node);
    let value = raw.trim_matches('"').to_string();
    let start = node.start_position();
    let start_col = (start.column + 1) as u32;
    Some((
        value.clone(),
        (start.row as u32, start_col, start_col + value.len() as u32),
    ))
}

/// The last identifier in an expression: handles a bare identifier, a cast
/// like `(VALUE (*)(ANYARGS))foo`, and a wrapper like `RUBY_METHOD_FUNC(foo)`.
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

    fn find<'a>(r: &'a ScanResult, name: &str) -> &'a RubyExport {
        r.exports
            .iter()
            .find(|e| e.ruby_name == name)
            .unwrap_or_else(|| panic!("no export {name}: {:?}", r.exports))
    }

    #[test]
    fn extracts_module_and_method() {
        let src = r#"
            #include <ruby.h>

            static VALUE foo_bar(VALUE self) { return Qnil; }

            void Init_foo(void) {
                VALUE mFoo = rb_define_module("Foo");
                rb_define_method(mFoo, "bar", foo_bar, 0);
            }
        "#;
        let r = scan_str(src);
        let module = find(&r, "Foo");
        assert_eq!(module.kind, RubyExportKind::Module);
        assert_eq!(module.namespace, "");
        assert_eq!(module.c_name, "Init_foo");
        let method = find(&r, "bar");
        assert_eq!(method.kind, RubyExportKind::Method);
        assert_eq!(method.namespace, "Foo");
        assert_eq!(method.c_name, "foo_bar");
    }

    #[test]
    fn tracks_nested_namespaces() {
        let src = r#"
            void Init_foo(void) {
                VALUE mFoo = rb_define_module("Foo");
                VALUE cBar = rb_define_class_under(mFoo, "Bar", rb_cObject);
                rb_define_method(cBar, "baz", bar_baz, 1);
                rb_define_singleton_method(cBar, "make", bar_make, 0);
            }
        "#;
        let r = scan_str(src);
        let class = find(&r, "Bar");
        assert_eq!(class.kind, RubyExportKind::Class);
        assert_eq!(class.namespace, "Foo");
        let method = find(&r, "baz");
        assert_eq!(method.namespace, "Foo::Bar");
        let singleton = find(&r, "make");
        assert_eq!(singleton.kind, RubyExportKind::SingletonMethod);
        assert_eq!(singleton.namespace, "Foo::Bar");
    }

    #[test]
    fn resolves_builtin_and_conventional_receivers() {
        let src = r#"
            void Init_ext(void) {
                rb_define_method(rb_cObject, "blank?", obj_blank, 0);
                rb_define_method(cString, "shuffle", str_shuffle, 0);
            }
        "#;
        let r = scan_str(src);
        assert_eq!(find(&r, "blank?").namespace, "Object");
        assert_eq!(find(&r, "shuffle").namespace, "String");
    }

    #[test]
    fn global_function_lands_on_kernel() {
        let src = r#"
            void Init_ext(void) {
                rb_define_global_function("frobnicate", ext_frobnicate, -1);
            }
        "#;
        let r = scan_str(src);
        let f = find(&r, "frobnicate");
        assert_eq!(f.namespace, "Kernel");
        assert_eq!(f.c_name, "ext_frobnicate");
    }

    #[test]
    fn unwraps_method_func_casts() {
        let src = r#"
            void Init_ext(void) {
                VALUE c = rb_define_class("Thing", rb_cObject);
                rb_define_method(c, "a", RUBY_METHOD_FUNC(thing_a), 0);
                rb_define_method(c, "b", (VALUE (*)(ANYARGS))thing_b, 2);
            }
        "#;
        let r = scan_str(src);
        assert_eq!(find(&r, "a").c_name, "thing_a");
        assert_eq!(find(&r, "b").c_name, "thing_b");
        assert_eq!(find(&r, "a").namespace, "Thing");
    }

    #[test]
    fn assignment_without_declaration() {
        let src = r#"
            static VALUE mFoo;
            void Init_foo(void) {
                mFoo = rb_define_module("Foo");
                rb_define_module_function(mFoo, "go", foo_go, 0);
            }
        "#;
        let r = scan_str(src);
        let f = find(&r, "go");
        assert_eq!(f.namespace, "Foo");
        assert_eq!(f.kind, RubyExportKind::SingletonMethod);
    }

    #[test]
    fn ignores_unrelated_calls() {
        let src = r#"
            void setup(void) {
                printf("rb_define_method");
                other_call(x, "name", fn, 0);
            }
        "#;
        let r = scan_str(src);
        assert!(r.exports.is_empty(), "got: {:?}", r.exports);
    }
}
