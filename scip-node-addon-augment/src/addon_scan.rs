//! Node.js native addon source scanner.
//!
//! Walks C and C++ sources with tree-sitter looking for the registration
//! patterns used to expose JavaScript-visible names from a native addon.
//! Recognises:
//!
//! * `NAPI_MODULE(name, init)` / `NODE_MODULE(name, init)` -- module init
//!   entry point; the first argument is the JS-visible module name.
//! * `Nan::SetMethod(target, "name", cxx_fn)`,
//!   `Nan::SetPrototypeMethod(tpl, "name", cxx_fn)`,
//!   `Nan::Export(target, "name", cxx_fn)` -- NAN pattern.
//! * `NODE_SET_METHOD(target, "name", cxx_fn)`,
//!   `NODE_SET_PROTOTYPE_METHOD(tpl, "name", cxx_fn)` -- pre-NAN Node addon
//!   pattern still used by some legacy code.
//! * `napi_set_named_property(env, target, "name", value)` -- direct N-API
//!   single-property registration. The value expression is either an
//!   identifier or a call whose target is captured.
//! * `exports.Set("name", Napi::Function::New(env, cxx_fn))` and the
//!   equivalent `exports.DefineProperties({ ... })` initializer list --
//!   node-addon-api C++ pattern.
//! * `napi_property_descriptor` array initializers with
//!   `.utf8name = "name", .method = cxx_fn` entries. The array is then
//!   referenced from a `napi_define_properties(env, target, count, arr)`
//!   call, but we register every array we see; downstream cross-referencing
//!   just doesn't fire if the C symbol never gets used.
//!
//! Not detected: names that only exist behind a project-local preprocessor
//! macro that uses `#stringification` (e.g. node-iconv's `EXPORT_METHOD(desc,
//! name)`). Those never appear as literal strings in raw source; a future
//! version could preprocess-first via `clang -E` to see them.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tree_sitter::{Language, Node, Parser};

#[derive(Debug, Clone)]
pub struct JsExport {
    pub relative_path: PathBuf,
    /// JS-facing name (e.g. `$connect`, or `exports.foo` -- for module
    /// exports the string is bare).
    pub js_name: String,
    /// C or C++ identifier the export wraps (e.g. `Connection::Connect`,
    /// `init`). Used to look up the C SCIP symbol.
    pub c_name: String,
    pub kind: JsExportKind,
    /// Zero-based `(line, start_col, end_col)` -- the range of the JS-facing
    /// string literal in the source.
    pub name_span: (u32, u32, u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsExportKind {
    Function,
    /// A property registered on a class prototype (`SetPrototypeMethod`).
    /// The `class_name` is inferred by the caller and passed via
    /// [`JsExport::c_name`] when known; here we just tag the shape.
    Method,
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub exports: Vec<JsExport>,
    /// Module name, if we saw a `NAPI_MODULE` / `NODE_MODULE` invocation.
    pub module_name: Option<String>,
    /// C fn identifier that initialises the module (the second arg to
    /// `NAPI_MODULE` / `NODE_MODULE`), so cross-referencing can link the
    /// module export back to the C definition.
    pub module_init: Option<String>,
}

pub fn scan_file(path: &Path, source_root: &Path, out: &mut ScanResult) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    // Prefilter -- most files in a source tree are neither addons nor use N-API.
    if !text.contains("napi_")
        && !text.contains("NODE_")
        && !text.contains("NAPI_")
        && !text.contains("Nan::")
        && !text.contains("Napi::")
    {
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

    walk(tree.root_node(), src, &rel, out);
    Ok(())
}

fn is_cpp_source(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("cc") | Some("cpp") | Some("cxx") | Some("hpp") | Some("hh")
    )
}

fn walk(node: Node, src: &[u8], rel: &Path, out: &mut ScanResult) {
    if node.kind() == "call_expression" {
        if let Some(fn_name) = call_target_name(node, src) {
            classify_call(&fn_name, node, src, rel, out);
        }
    } else if node.kind() == "initializer_list" {
        collect_property_descriptor(node, src, rel, out);
    }

    let mut cursor = node.walk();
    if cursor.goto_first_child() {
        loop {
            walk(cursor.node(), src, rel, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
    }
}

/// For a call_expression, return the callee's identifier as a plain string.
/// Handles bare identifiers (`NAPI_MODULE`, `napi_set_named_property`),
/// qualified names (`Nan::SetMethod`), and template functions.
fn call_target_name(call: Node, src: &[u8]) -> Option<String> {
    let mut walker = call.walk();
    for child in call.children(&mut walker) {
        if child.kind() == "argument_list" {
            continue;
        }
        return Some(node_text(child, src));
    }
    None
}

fn node_text(node: Node, src: &[u8]) -> String {
    std::str::from_utf8(&src[node.byte_range()])
        .unwrap_or("")
        .to_string()
}

fn classify_call(fn_name: &str, call: Node, src: &[u8], rel: &Path, out: &mut ScanResult) {
    // Strip template-function suffix, e.g. `Nan::New<v8::FunctionTemplate>`
    // → `Nan::New`. The angle-bracket portion carries no information we
    // need for classification.
    let bare = fn_name.split('<').next().unwrap_or(fn_name).trim();

    if matches!(bare, "NAPI_MODULE" | "NODE_MODULE") {
        // First arg = module name (bare identifier), second = init fn (bare
        // identifier). Neither should be a string.
        let args = call_args(call);
        if args.len() >= 2 {
            let name = node_text(args[0], src);
            let init = node_text(args[1], src);
            if !name.is_empty() && !init.is_empty() {
                out.module_name = Some(name);
                out.module_init = Some(init);
            }
        }
        return;
    }

    // `Nan::SetMethod(target, "name", fn)`, `Nan::SetPrototypeMethod(tpl,
    // "name", fn)`, `Nan::Export(target, "name", fn)` -- 3-arg forms where
    // arg[1] is the JS name and arg[2] is the C++ identifier.
    if matches!(
        bare,
        "Nan::SetMethod"
            | "Nan::SetPrototypeMethod"
            | "Nan::Export"
            | "NODE_SET_METHOD"
            | "NODE_SET_PROTOTYPE_METHOD"
    ) {
        let args = call_args(call);
        if args.len() >= 3 {
            if let Some(js_name) = string_literal_value(args[1], src) {
                let c_name = qualified_identifier_or_call_target(args[2], src);
                if !c_name.is_empty() {
                    let start = args[1].start_position();
                    let start_col = (start.column + 1) as u32;
                    let kind = if bare.contains("Prototype") {
                        JsExportKind::Method
                    } else {
                        JsExportKind::Function
                    };
                    out.exports.push(JsExport {
                        relative_path: rel.to_path_buf(),
                        js_name: js_name.clone(),
                        c_name,
                        kind,
                        name_span: (
                            start.row as u32,
                            start_col,
                            start_col + js_name.len() as u32,
                        ),
                    });
                }
            }
        }
        return;
    }

    // `napi_set_named_property(env, target, "name", value)` -- 4-arg form.
    if bare == "napi_set_named_property" {
        let args = call_args(call);
        if args.len() >= 4 {
            if let Some(js_name) = string_literal_value(args[2], src) {
                let c_name = qualified_identifier_or_call_target(args[3], src);
                let start = args[2].start_position();
                let start_col = (start.column + 1) as u32;
                out.exports.push(JsExport {
                    relative_path: rel.to_path_buf(),
                    js_name: js_name.clone(),
                    c_name,
                    kind: JsExportKind::Function,
                    name_span: (
                        start.row as u32,
                        start_col,
                        start_col + js_name.len() as u32,
                    ),
                });
            }
        }
        return;
    }

    // `napi_create_function(env, "name", NAPI_AUTO_LENGTH, fn, data, &out)`
    // -- 6-arg form. arg[1] = JS name (may be NULL for anonymous), arg[3] = C fn.
    if bare == "napi_create_function" {
        let args = call_args(call);
        if args.len() >= 4 {
            if let Some(js_name) = string_literal_value(args[1], src) {
                let c_name = qualified_identifier_or_call_target(args[3], src);
                let start = args[1].start_position();
                let start_col = (start.column + 1) as u32;
                out.exports.push(JsExport {
                    relative_path: rel.to_path_buf(),
                    js_name: js_name.clone(),
                    c_name,
                    kind: JsExportKind::Function,
                    name_span: (
                        start.row as u32,
                        start_col,
                        start_col + js_name.len() as u32,
                    ),
                });
            }
        }
        return;
    }

    // node-addon-api C++: `exports.Set("name", Napi::Function::New(env, fn))`.
    // The callee is `<expr>.Set` -- we can't tell it apart from other `.Set`
    // calls from the fn name alone, so we match by argument shape: 2 args,
    // first is a string literal, second contains a `Napi::Function::New` call
    // whose second-or-third arg is the C fn.
    if fn_name.ends_with(".Set") || fn_name.ends_with("->Set") {
        let args = call_args(call);
        if args.len() == 2 {
            if let Some(js_name) = string_literal_value(args[0], src) {
                if let Some(c_name) = extract_napi_function_new_target(args[1], src) {
                    let start = args[0].start_position();
                    let start_col = (start.column + 1) as u32;
                    out.exports.push(JsExport {
                        relative_path: rel.to_path_buf(),
                        js_name: js_name.clone(),
                        c_name,
                        kind: JsExportKind::Function,
                        name_span: (
                            start.row as u32,
                            start_col,
                            start_col + js_name.len() as u32,
                        ),
                    });
                }
            }
        }
    }
}

fn call_args<'a>(call: Node<'a>) -> Vec<Node<'a>> {
    let mut walker = call.walk();
    let args = call
        .children(&mut walker)
        .find(|c| c.kind() == "argument_list");
    let Some(args) = args else {
        return Vec::new();
    };
    let mut arg_walker = args.walk();
    args.children(&mut arg_walker)
        .filter(|c| !matches!(c.kind(), "(" | ")" | ","))
        .collect()
}

/// Return the value of a `string_literal` node with quotes stripped, or None
/// if the node isn't one.
fn string_literal_value(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "string_literal" {
        return None;
    }
    let raw = std::str::from_utf8(&src[node.byte_range()]).ok()?;
    Some(raw.trim_matches('"').to_string())
}

/// Extract the C/C++ identifier being passed as a function pointer.
///
/// Accepts a bare `identifier`, a `qualified_identifier`
/// (`Connection::Connect`), or a call/cast expression -- in the last case we
/// walk the subtree and take the last identifier we see, since that's
/// invariably the wrapped C++ callback.
fn qualified_identifier_or_call_target(node: Node, src: &[u8]) -> String {
    if matches!(
        node.kind(),
        "identifier" | "qualified_identifier" | "field_identifier"
    ) {
        return node_text(node, src);
    }
    // Walk subtree and pick the last non-namespace identifier.
    let mut last: Option<String> = None;
    let mut cursor = node.walk();
    walk_pick_last_ident(&mut cursor, src, &mut last);
    last.unwrap_or_default()
}

fn walk_pick_last_ident(
    cursor: &mut tree_sitter::TreeCursor,
    src: &[u8],
    last: &mut Option<String>,
) {
    let node = cursor.node();
    if matches!(
        node.kind(),
        "identifier" | "qualified_identifier" | "field_identifier"
    ) {
        *last = Some(node_text(node, src));
    }
    if cursor.goto_first_child() {
        loop {
            walk_pick_last_ident(cursor, src, last);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// From `Napi::Function::New(env, cxx_fn)` or
/// `Napi::Function::New(env, cxx_fn, "name")`, extract `cxx_fn`.
fn extract_napi_function_new_target(node: Node, src: &[u8]) -> Option<String> {
    if node.kind() != "call_expression" {
        return None;
    }
    let target = call_target_name(node, src)?;
    // Accept both templated and non-templated Napi::Function::New.
    let target = target.split('<').next().unwrap_or(&target).trim();
    if target != "Napi::Function::New" {
        return None;
    }
    let args = call_args(node);
    if args.len() < 2 {
        return None;
    }
    let ident = qualified_identifier_or_call_target(args[1], src);
    (!ident.is_empty()).then_some(ident)
}

/// Detect a `napi_property_descriptor` initializer entry:
/// `{ .utf8name = "name", .method = fn, ... }` or the positional variant
/// `{ "name", NULL, fn, ... }`.
fn collect_property_descriptor(node: Node, src: &[u8], rel: &Path, out: &mut ScanResult) {
    // The outer initializer_list contains other initializer_lists (rows).
    // We look at each row for the `.utf8name`/`.method` pair. We also accept
    // any row where the first two positional fields are a string literal
    // and an identifier -- that covers `napi_property_descriptor` in
    // positional form.
    let mut js_name: Option<String> = None;
    let mut js_name_span: Option<(u32, u32, u32)> = None;
    let mut c_name: Option<String> = None;
    let mut walker = node.walk();
    for child in node.children(&mut walker) {
        match child.kind() {
            "initializer_pair" => {
                let (designator, value) = split_initializer_pair(child, src);
                match designator.as_deref() {
                    Some("utf8name") | Some("name") => {
                        if let Some(v) = value {
                            if let Some(s) = string_literal_value(v, src) {
                                let sp = v.start_position();
                                let col = (sp.column + 1) as u32;
                                js_name_span = Some((sp.row as u32, col, col + s.len() as u32));
                                js_name = Some(s);
                            }
                        }
                    }
                    Some("method") | Some("value") | Some("getter") => {
                        if let Some(v) = value {
                            let n = qualified_identifier_or_call_target(v, src);
                            if !n.is_empty() {
                                c_name = Some(n);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "string_literal" if js_name.is_none() => {
                if let Some(s) = string_literal_value(child, src) {
                    let sp = child.start_position();
                    let col = (sp.column + 1) as u32;
                    js_name_span = Some((sp.row as u32, col, col + s.len() as u32));
                    js_name = Some(s);
                }
            }
            "identifier" | "qualified_identifier" if c_name.is_none() && js_name.is_some() => {
                let s = node_text(child, src);
                if !s.is_empty() && s != "NULL" {
                    c_name = Some(s);
                }
            }
            _ => {}
        }
    }
    if let (Some(js), Some(c), Some(span)) = (js_name, c_name, js_name_span) {
        out.exports.push(JsExport {
            relative_path: rel.to_path_buf(),
            js_name: js,
            c_name: c,
            kind: JsExportKind::Function,
            name_span: span,
        });
    }
}

fn split_initializer_pair<'a>(node: Node<'a>, src: &[u8]) -> (Option<String>, Option<Node<'a>>) {
    let mut walker = node.walk();
    let mut designator: Option<String> = None;
    let mut value: Option<Node<'a>> = None;
    for child in node.children(&mut walker) {
        match child.kind() {
            "field_designator" => {
                let text = node_text(child, src);
                // `.utf8name` → `utf8name`
                let name = text.trim_start_matches('.').to_string();
                designator = Some(name);
            }
            "=" | "," | "(" | ")" => {}
            _ => {
                if value.is_none() && designator.is_some() {
                    value = Some(child);
                }
            }
        }
    }
    (designator, value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn scan_str_with_ext(src: &str, ext: &str) -> ScanResult {
        let tmpdir = tempfile::tempdir().unwrap();
        let path = tmpdir.path().join(format!("t.{}", ext));
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(src.as_bytes()).unwrap();
        let mut r = ScanResult::default();
        scan_file(&path, tmpdir.path(), &mut r).unwrap();
        r
    }

    #[test]
    fn extracts_nan_set_prototype_method() {
        let src = r#"
            #include "nan.h"
            NAN_MODULE_INIT(Init) {
                Nan::SetPrototypeMethod(tpl, "$connect", Connection::Connect);
                Nan::SetPrototypeMethod(tpl, "$finish", Connection::Finish);
            }
            NODE_MODULE(addon, Init)
        "#;
        let r = scan_str_with_ext(src, "cc");
        let subs: Vec<_> = r
            .exports
            .iter()
            .map(|e| (e.js_name.clone(), e.c_name.clone()))
            .collect();
        assert!(subs.contains(&("$connect".to_string(), "Connection::Connect".to_string())));
        assert!(subs.contains(&("$finish".to_string(), "Connection::Finish".to_string())));
        assert_eq!(r.module_name.as_deref(), Some("addon"));
        assert_eq!(r.module_init.as_deref(), Some("Init"));
    }

    #[test]
    fn extracts_nan_set_method_flat() {
        let src = r#"
            #include "nan.h"
            void Init(v8::Local<v8::Object> target) {
                Nan::SetMethod(target, "hello", hello_impl);
            }
            NODE_MODULE(mod, Init)
        "#;
        let r = scan_str_with_ext(src, "cc");
        let f = r.exports.iter().find(|e| e.js_name == "hello").unwrap();
        assert_eq!(f.c_name, "hello_impl");
        assert_eq!(f.kind, JsExportKind::Function);
    }

    #[test]
    fn extracts_napi_module_name() {
        let src = r#"
            #include <node_api.h>
            napi_value init(napi_env env, napi_value exports) { return exports; }
            NAPI_MODULE(mymod, init)
        "#;
        let r = scan_str_with_ext(src, "c");
        assert_eq!(r.module_name.as_deref(), Some("mymod"));
        assert_eq!(r.module_init.as_deref(), Some("init"));
    }

    #[test]
    fn extracts_napi_set_named_property() {
        let src = r#"
            #include <node_api.h>
            void wire(napi_env env, napi_value exports) {
                napi_value fn;
                napi_create_function(env, "hello", NAPI_AUTO_LENGTH, hello_impl, NULL, &fn);
                napi_set_named_property(env, exports, "hello", fn);
            }
            NAPI_MODULE(m, wire)
        "#;
        let r = scan_str_with_ext(src, "c");
        // The create_function AND the set_named_property both name "hello";
        // we expect at least one export named it.
        assert!(r.exports.iter().any(|e| e.js_name == "hello"));
    }

    #[test]
    fn extracts_property_descriptor_designated() {
        let src = r#"
            #include <node_api.h>
            napi_property_descriptor props[] = {
                { .utf8name = "foo", .method = foo_impl },
                { .utf8name = "bar", .method = bar_impl },
                { 0 }
            };
        "#;
        let r = scan_str_with_ext(src, "c");
        let names: Vec<_> = r.exports.iter().map(|e| e.js_name.clone()).collect();
        assert!(names.contains(&"foo".to_string()));
        assert!(names.contains(&"bar".to_string()));
    }

    #[test]
    fn extracts_node_addon_api_exports_set() {
        // node-addon-api C++ pattern.
        let src = r#"
            #include <napi.h>
            Napi::Object Init(Napi::Env env, Napi::Object exports) {
                exports.Set("hello", Napi::Function::New(env, HelloImpl));
                return exports;
            }
        "#;
        let r = scan_str_with_ext(src, "cc");
        let f = r.exports.iter().find(|e| e.js_name == "hello").unwrap();
        assert_eq!(f.c_name, "HelloImpl");
    }

    #[test]
    fn skips_unrelated_files() {
        let src = "int main(void) { return 0; }";
        let r = scan_str_with_ext(src, "c");
        assert!(r.exports.is_empty());
        assert!(r.module_name.is_none());
    }
}
