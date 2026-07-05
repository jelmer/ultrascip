//! C source scanner for CPython C extension exports.
//!
//! Parses each `.c` file with tree-sitter-c and extracts three things:
//!
//! * `static PyMethodDef <arr>[] = { { "<pyname>", (PyCFunction)<c_fn>, ... }, ... };`
//!   — each entry maps a Python name to a C function.
//! * `static struct PyModuleDef <var> = { HEAD, "<modname>", ..., <methods_arr>, ... };`
//!   — the module's Python name and the `PyMethodDef` array it wires up.
//! * `PyMODINIT_FUNC PyInit_<modname>(void) { ... }` — fallback for the
//!   Python module name if the `PyModuleDef` initializer doesn't include a
//!   string literal (rare, but happens when `m_name` is set later).
//!
//! For each Python function we record the C identifier so the emitted Python
//! symbol can cross-reference the C definition captured in the input SCIP
//! index.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone)]
pub struct CExport {
    pub relative_path: PathBuf,
    /// Python-facing name from the `PyMethodDef` initializer string literal.
    pub python_name: String,
    /// C function identifier the initializer casts to `PyCFunction`.
    pub c_name: String,
    pub kind: CExportKind,
    /// Zero-based `(line, start_col, end_col)` — the range of the string
    /// literal (without quotes) inside the `PyMethodDef` entry, or the
    /// identifier's range for module exports.
    pub name_span: (u32, u32, u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CExportKind {
    Module,
    Function,
}

#[derive(Debug, Default)]
pub struct ScanResult {
    pub exports: Vec<CExport>,
}

pub fn scan_file(path: &Path, source_root: &Path, out: &mut ScanResult) -> Result<()> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    // Cheap prefilter — files with no CPython entry points are the vast
    // majority in most trees.
    if !text.contains("PyMethodDef") && !text.contains("PyModuleDef") && !text.contains("PyInit_") {
        return Ok(());
    }

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .context("setting tree-sitter-c language")?;
    let Some(tree) = parser.parse(&text, None) else {
        return Ok(());
    };
    let src = text.as_bytes();
    let root = tree.root_node();

    let rel = path
        .strip_prefix(source_root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf());

    // Two passes:
    //   1. Collect every PyMethodDef array by its C identifier ->
    //      Vec<(python_name, c_name, span)>.
    //   2. Walk PyModuleDef declarations, look up the wired-up array to
    //      determine which module a set of functions belongs to, and emit an
    //      export entry for the module and each function.
    // If no PyModuleDef pairs an array up, we still emit the functions,
    // attributed to a module detected from a PyInit_ function definition in
    // the same file.
    let mut arrays: HashMap<String, Vec<PyMethodEntry>> = HashMap::new();
    let mut module_defs: Vec<PyModuleDefRec> = Vec::new();
    let mut pyinit_modules: Vec<PyInitRec> = Vec::new();

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        match child.kind() {
            "declaration" => {
                collect_declaration(child, src, &mut arrays, &mut module_defs);
            }
            "function_definition" => {
                if let Some(rec) = collect_pyinit(child, src) {
                    pyinit_modules.push(rec);
                }
            }
            _ => {}
        }
    }

    // Attribute each PyMethodDef array to a module. Prefer the m_name string
    // literal from PyModuleDef; fall back to PyInit_<name> if there's only
    // one such fn in the file.
    let mut consumed_arrays = std::collections::HashSet::new();
    for md in &module_defs {
        let module_name = md.m_name.clone();
        let module_span = md.name_span;
        out.exports.push(CExport {
            relative_path: rel.clone(),
            python_name: module_name.clone(),
            c_name: md.c_ident.clone(),
            kind: CExportKind::Module,
            name_span: module_span,
        });
        if let Some(arr) = &md.m_methods {
            consumed_arrays.insert(arr.clone());
            if let Some(entries) = arrays.get(arr) {
                for entry in entries {
                    out.exports.push(CExport {
                        relative_path: rel.clone(),
                        python_name: entry.python_name.clone(),
                        c_name: entry.c_name.clone(),
                        kind: CExportKind::Function,
                        name_span: entry.name_span,
                    });
                }
            }
        }
    }

    // Fallback: arrays with no PyModuleDef in this file. Attribute to the
    // single PyInit_ fn if there is one; otherwise emit under the array's
    // own name (best effort).
    let fallback_module = if module_defs.is_empty() && pyinit_modules.len() == 1 {
        Some(pyinit_modules[0].clone())
    } else {
        None
    };
    if module_defs.is_empty() {
        if let Some(rec) = &fallback_module {
            out.exports.push(CExport {
                relative_path: rel.clone(),
                python_name: rec.module_name.clone(),
                c_name: rec.c_ident.clone(),
                kind: CExportKind::Module,
                name_span: rec.name_span,
            });
        }
        for (arr_name, entries) in &arrays {
            if consumed_arrays.contains(arr_name) {
                continue;
            }
            for entry in entries {
                out.exports.push(CExport {
                    relative_path: rel.clone(),
                    python_name: entry.python_name.clone(),
                    c_name: entry.c_name.clone(),
                    kind: CExportKind::Function,
                    name_span: entry.name_span,
                });
            }
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct PyMethodEntry {
    python_name: String,
    c_name: String,
    name_span: (u32, u32, u32),
}

#[derive(Debug, Clone)]
struct PyModuleDefRec {
    c_ident: String,
    m_name: String,
    m_methods: Option<String>,
    name_span: (u32, u32, u32),
}

#[derive(Debug, Clone)]
struct PyInitRec {
    /// The Python module name (without the `PyInit_` prefix).
    module_name: String,
    c_ident: String,
    name_span: (u32, u32, u32),
}

fn collect_declaration(
    node: Node,
    src: &[u8],
    arrays: &mut HashMap<String, Vec<PyMethodEntry>>,
    module_defs: &mut Vec<PyModuleDefRec>,
) {
    // A declaration's type_identifier tells us what kind of struct this is.
    let type_name = node
        .children(&mut node.walk())
        .find(|c| c.kind() == "type_identifier")
        .map(|c| slice(src, c));
    // Some declarations wrap the type in `struct <Name>`; look for that too.
    let struct_name = node
        .children(&mut node.walk())
        .find(|c| c.kind() == "struct_specifier")
        .and_then(|c| {
            c.children(&mut c.walk())
                .find(|cc| cc.kind() == "type_identifier")
                .map(|cc| slice(src, cc))
        });

    match type_name.as_deref().or(struct_name.as_deref()) {
        Some("PyMethodDef") => collect_method_def(node, src, arrays),
        Some("PyModuleDef") => {
            if let Some(rec) = collect_module_def(node, src) {
                module_defs.push(rec);
            }
        }
        _ => {}
    }
}

fn collect_method_def(decl: Node, src: &[u8], arrays: &mut HashMap<String, Vec<PyMethodEntry>>) {
    // Find init_declarator -> array_declarator (or plain identifier) for the
    // array name, and the initializer_list for its contents.
    let mut walker = decl.walk();
    for init in decl
        .children(&mut walker)
        .filter(|c| c.kind() == "init_declarator")
    {
        let array_name = init
            .children(&mut init.walk())
            .find_map(|c| match c.kind() {
                "array_declarator" => c
                    .children(&mut c.walk())
                    .find(|cc| cc.kind() == "identifier")
                    .map(|cc| slice(src, cc)),
                "identifier" => Some(slice(src, c)),
                _ => None,
            });
        let Some(array_name) = array_name else {
            continue;
        };
        let Some(outer_list) = init
            .children(&mut init.walk())
            .find(|c| c.kind() == "initializer_list")
        else {
            continue;
        };
        let mut entries = Vec::new();
        for entry in outer_list
            .children(&mut outer_list.walk())
            .filter(|c| c.kind() == "initializer_list")
        {
            if let Some(e) = parse_method_entry(entry, src) {
                entries.push(e);
            }
        }
        arrays.insert(array_name, entries);
    }
}

fn parse_method_entry(entry: Node, src: &[u8]) -> Option<PyMethodEntry> {
    // Look for the first string_literal (python name) and the first
    // cast_expression / identifier that gives us the C function.
    let mut python_name = None;
    let mut name_span = None;
    let mut c_name = None;
    let mut walker = entry.walk();
    for c in entry.children(&mut walker) {
        match c.kind() {
            "string_literal" if python_name.is_none() => {
                let raw = slice(src, c);
                let stripped = raw.trim_matches('"');
                if stripped == "NULL" {
                    return None; // sentinel entry
                }
                python_name = Some(stripped.to_string());
                let start = c.start_position();
                // Skip the opening quote so the range points at the identifier text.
                let start_col = (start.column + 1) as u32;
                name_span = Some((
                    start.row as u32,
                    start_col,
                    start_col + stripped.len() as u32,
                ));
            }
            "cast_expression" if c_name.is_none() => {
                c_name = cast_target_ident(c, src);
            }
            "identifier" if c_name.is_none() => {
                // Sometimes the entry is `{ "name", py_fn, ... }` without a cast.
                c_name = Some(slice(src, c));
            }
            _ => {}
        }
    }
    let (pn, cn, span) = (python_name?, c_name?, name_span?);
    // Skip sentinel entries like `{ NULL, NULL, 0, NULL }`.
    if pn.is_empty() || cn == "NULL" {
        return None;
    }
    Some(PyMethodEntry {
        python_name: pn,
        c_name: cn,
        name_span: span,
    })
}

fn cast_target_ident(cast: Node, src: &[u8]) -> Option<String> {
    // cast_expression: ( type ) expr — the expr is the last child.
    let mut last_ident = None;
    let mut walker = cast.walk();
    for c in cast.children(&mut walker) {
        if c.kind() == "identifier" {
            last_ident = Some(slice(src, c));
        }
    }
    last_ident
}

fn collect_module_def(decl: Node, src: &[u8]) -> Option<PyModuleDefRec> {
    let mut walker = decl.walk();
    let init = decl
        .children(&mut walker)
        .find(|c| c.kind() == "init_declarator")?;
    let c_ident = init
        .children(&mut init.walk())
        .find(|c| c.kind() == "identifier")
        .map(|c| slice(src, c))?;
    let list = init
        .children(&mut init.walk())
        .find(|c| c.kind() == "initializer_list")?;
    // First string_literal is m_name.
    let m_name_node = list
        .children(&mut list.walk())
        .find(|c| c.kind() == "string_literal")?;
    let raw = slice(src, m_name_node);
    let m_name = raw.trim_matches('"').to_string();
    let mstart = m_name_node.start_position();
    let name_span = (
        mstart.row as u32,
        (mstart.column + 1) as u32,
        (mstart.column + 1 + m_name.len()) as u32,
    );
    // m_methods is any identifier child (typically the fifth positional
    // field). Take the first non-`PyModuleDef_HEAD_INIT` identifier.
    let m_methods = list
        .children(&mut list.walk())
        .filter(|c| c.kind() == "identifier")
        .map(|c| slice(src, c))
        .find(|name| name != "PyModuleDef_HEAD_INIT" && name != "NULL");
    Some(PyModuleDefRec {
        c_ident,
        m_name,
        m_methods,
        name_span,
    })
}

fn collect_pyinit(func: Node, src: &[u8]) -> Option<PyInitRec> {
    // function_definition -> function_declarator -> identifier
    let mut walker = func.walk();
    let decl = func
        .children(&mut walker)
        .find(|c| c.kind() == "function_declarator")?;
    let ident_node = decl
        .children(&mut decl.walk())
        .find(|c| c.kind() == "identifier")?;
    let name = slice(src, ident_node);
    let module_name = name.strip_prefix("PyInit_")?.to_string();
    let start = ident_node.start_position();
    // The PyInit_ prefix has length 7 in the source; the module name follows.
    let module_col = (start.column + 7) as u32;
    Some(PyInitRec {
        module_name: module_name.clone(),
        c_ident: name,
        name_span: (
            start.row as u32,
            module_col,
            module_col + module_name.len() as u32,
        ),
    })
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
        // The file needs a .c extension so nothing rejects it later; content
        // is what we care about here.
        let mut r = ScanResult::default();
        scan_file(f.path(), f.path().parent().unwrap(), &mut r).unwrap();
        r
    }

    #[test]
    fn extracts_pymethod_and_module() {
        let src = r#"
            #include <Python.h>

            static PyObject *py_is_tree(PyObject *self, PyObject *args) { return NULL; }

            static PyMethodDef methods[] = {
                { "_is_tree", (PyCFunction)py_is_tree, METH_VARARGS, NULL },
                { NULL, NULL, 0, NULL }
            };

            static PyObject *moduleinit(void) {
                static struct PyModuleDef moduledef = {
                    PyModuleDef_HEAD_INIT,
                    "_mymod",
                    NULL, -1,
                    methods,
                    NULL, NULL, NULL, NULL,
                };
                return PyModule_Create(&moduledef);
            }

            PyMODINIT_FUNC PyInit__mymod(void) { return moduleinit(); }
        "#;
        let r = scan_str(src);
        let names: Vec<_> = r.exports.iter().map(|e| e.python_name.clone()).collect();
        assert!(names.contains(&"_mymod".to_string()), "got: {:?}", names);
        assert!(names.contains(&"_is_tree".to_string()), "got: {:?}", names);
        // The function entry should carry the C name too.
        let fun = r
            .exports
            .iter()
            .find(|e| e.python_name == "_is_tree")
            .unwrap();
        assert_eq!(fun.c_name, "py_is_tree");
        assert_eq!(fun.kind, CExportKind::Function);
    }

    #[test]
    fn falls_back_to_pyinit_when_no_moduledef() {
        // Rare / synthetic: methods array with a PyInit_ but no ModuleDef in
        // the same file (e.g. the ModuleDef is in another TU).
        let src = r#"
            #include <Python.h>
            static PyMethodDef methods[] = {
                { "hello", (PyCFunction)c_hello, METH_VARARGS, NULL },
                { NULL, NULL, 0, NULL }
            };
            PyMODINIT_FUNC PyInit__mymod(void) { return NULL; }
        "#;
        let r = scan_str(src);
        let names: Vec<_> = r.exports.iter().map(|e| e.python_name.clone()).collect();
        assert!(names.contains(&"_mymod".to_string()));
        assert!(names.contains(&"hello".to_string()));
    }

    #[test]
    fn ignores_null_sentinel_entry() {
        let src = r#"
            #include <Python.h>
            static PyMethodDef methods[] = {
                { NULL, NULL, 0, NULL }
            };
            static struct PyModuleDef moduledef = {
                PyModuleDef_HEAD_INIT, "_empty", NULL, -1, methods,
            };
        "#;
        let r = scan_str(src);
        let names: Vec<_> = r.exports.iter().map(|e| e.python_name.clone()).collect();
        assert_eq!(names, vec!["_empty"]);
    }
}
