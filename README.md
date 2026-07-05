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

Rust crates that expose a C ABI (`#[no_mangle]`, `pub extern "C" fn`) get a
companion `rust-c-abi.scip` written alongside `rust.scip`. It carries C-side
symbols for each export so a downstream C indexer can cross-reference into the
Rust definitions. The companion is produced by the workspace's
`scip-c-abi-augment` binary, which is also usable standalone.

Rust crates that expose Python bindings via PyO3 (`#[pyfunction]`,
`#[pymodule]`, `#[pyclass]`, `#[pymethods]`) get a companion
`python-pyo3.scip` written alongside `rust.scip`, carrying Python-side symbols
for each export so a downstream Python indexer resolves imports into the Rust
definitions. Produced by the workspace's `scip-pyo3-augment` binary, also
usable standalone. Skipped silently when the source has no PyO3 items.

C/C++ projects that ship a CPython extension module (`PyMethodDef`,
`PyModuleDef`, `PyInit_*`) get a companion `python-cpython.scip` written
alongside `cpp.scip`. It carries Python-side symbols for each exported
function so a downstream Python indexer resolves imports into the C
definitions. Produced by `scip-c-python-augment`. Skipped when the source
has no CPython entry points, or when no `[project].name` can be read from a
`pyproject.toml`.

Gradle indexing currently fails on Debian: `scip-java`'s init script uses
Gradle 4.9+ APIs but Debian ships 4.4.1. Maven works.

## Usage

```
ultrascip --output-all OUT_DIR --session SESSION [--directory DIR]
          [--apt-build-deps] [--offline] [--debug]
```

Options:

- `--directory`, `-d` — source directory to index (default: `.`).
- `--output-all` — directory to write one `<language>.scip` per build system
  into. Created if missing.
- `--session` — session backend, e.g. `plain` or `unshare:sid`.
- `--apt-build-deps` — install the Debian source package's `Build-Depends`
  from `debian/control` via apt before indexing. Useful for indexers that
  need the build environment present (e.g. `scip-python` reading `setup.py`
  metadata).
- `--offline` — isolate the session from the network. Off by default; most
  indexers need the network to install build deps, download release binaries,
  or resolve package registries.
- `--debug` — verbose logging.

Indexing is best-effort: a failure for one build system does not discard the
indexes already written for the others, but if any indexer fails the process
exits non-zero.

## Release-binary indexers

`scip-clang`, `scip-go` and `scip-java` are not packaged for apt/npm/gem, so
they are downloaded on demand from their GitHub releases into the session's
`/usr/local/bin`. The release tag is resolved from the GitHub API at run time
rather than pinned; set `GITHUB_TOKEN` in the environment to raise the API
rate limit from 60 to 5000 requests/hour.
