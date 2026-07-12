# ultrascip

Generate [SCIP](https://github.com/sourcegraph/scip) indexes for a Debian
source package.

## Supported build systems

| Build system            | Indexer          | Language   |
|-------------------------|------------------|------------|
| cargo                   | rust-analyzer    | rust       |
| setup.py                | scip-python      | python     |
| golang                  | scip-go          | go         |
| maven / gradle          | scip-java        | java       |
| node                    | scip-typescript  | typescript |
| gem                     | scip-ruby        | ruby       |
| cmake                   | scip-clang       | cpp        |
| meson (C/C++)           | scip-clang       | cpp        |
| meson (Vala)            | scip-vala        | vala       |
| make                    | scip-clang       | cpp        |
| Makefile.PL             | scip-perl        | perl       |
| Dist::Zilla / M::B::Tiny| scip-perl        | perl       |

When two build systems in a project map to the same language, the output file
is disambiguated by appending the build system name (e.g. `cpp-meson.scip`
alongside `cpp.scip`).

## FFI companions

Where a project exposes symbols across a language boundary, an extra SCIP
index is written alongside the main one, carrying the foreign-side symbols
linked back to the native definitions. Supported bindings:

- Rust C ABI (`#[no_mangle]`, `pub extern "C" fn`) -> `rust-c-abi.scip`.
- Rust PyO3 (`#[pyfunction]`, `#[pymodule]`, `#[pyclass]`,
  `#[pymethods]`) -> `python-pyo3.scip`.
- C/C++ CPython extensions (`PyMethodDef`, `PyModuleDef`, `PyInit_*`)
  -> `python-cpython.scip`.
- C/C++ Node.js native addons (`NAPI_MODULE`, `NODE_MODULE`,
  `Nan::SetMethod`, `napi_property_descriptor`) -> `js-node-addon.scip`.

Each companion is produced by a standalone workspace binary
(`scip-c-abi-augment`, `scip-pyo3-augment`, `scip-c-python-augment`,
`scip-node-addon-augment`) and skipped silently when the source has no
matching bindings.

Gradle indexing currently fails on Debian: `scip-java`'s init script uses
Gradle 4.9+ APIs but Debian ships 4.4.1. Maven works.

## Post-indexing passes

After the language indexers, two host-side tools run against the source tree:

- `debian-lsp scip` (from [debian-lsp](https://github.com/jelmer/debian-lsp))
  writes `debian.scip` covering the Debian packaging files
  (`debian/control`, `debian/rules`, ...). Skipped for trees with no
  `debian/` subdirectory. Disable with `--no-debian-lsp`.
- `scip-tree-sitter` (from
  [scip-tools](https://github.com/jelmer/scip-tools)) writes
  `tree-sitter.scip` with syntax-highlighting tokens for files no language
  indexer covered, deferring to every other `.scip` in the output directory
  via `--exclude-scip`. Disable with `--no-tree-sitter`.

Both are required: if the tool is missing on `PATH` or exits non-zero, the
run fails. Pass `--no-debian-lsp` / `--no-tree-sitter` to skip a pass on
purpose.

## Usage

```
ultrascip --output-all OUT_DIR --session SESSION [--directory DIR]
          [--apt-build-deps] [--offline] [--no-debian-lsp]
          [--no-tree-sitter] [--debug]
```

Options:

- `--directory`, `-d` - source directory to index (default: `.`).
- `--output-all` - directory to write one `<language>.scip` per build system
  into. Created if missing.
- `--session` - session backend, e.g. `plain` or `unshare:sid`.
- `--apt-build-deps` - install the Debian source package's `Build-Depends`
  from `debian/control` via apt before indexing. Useful for indexers that
  need the build environment present (e.g. `scip-python` reading `setup.py`
  metadata).
- `--offline` - isolate the session from the network. Off by default; most
  indexers need the network to install build deps, download release binaries,
  or resolve package registries.
- `--no-debian-lsp` - skip the `debian-lsp scip` pass.
- `--no-tree-sitter` - skip the `scip-tree-sitter` pass.
- `--debug` - verbose logging.

Indexing is best-effort: a failure for one build system does not discard the
indexes already written for the others, but if any indexer fails the process
exits non-zero.

## Release-binary indexers

`scip-clang`, `scip-go` and `scip-java` are not packaged for apt/npm/gem, so
they are downloaded on demand from their GitHub releases into the session's
`/usr/local/bin`. The release tag is resolved from the GitHub API at run time
rather than pinned; set `GITHUB_TOKEN` in the environment to raise the API
rate limit from 60 to 5000 requests/hour.
