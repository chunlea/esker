//! ADR 0014's containment rule, as a test rather than as a sentence.
//!
//! `sqlparser` is the one large dependency this project takes, and the whole argument for taking it
//! is that replacing it is a bounded job: its types are named in `src/parse/` and nowhere else, so
//! a replacement is that module and nothing above it. Every layer downstream works with
//! `crate::plan`, which this crate owns.
//!
//! That was a rule in an ADR and a reviewer's attention. It is now a test. A `use sqlparser::` that
//! creeps into the executor does not make anything fail today — it compiles, it passes, it works —
//! and it quietly converts a one-module replacement into a rewrite. The failure mode of a
//! containment rule is that nothing goes wrong until the day you try to use it.
//!
//! The check is deliberately a *textual* one over the crate's own source. A type check would only
//! catch a `use`; a crate can also be named in a fully-qualified path, in a macro, or in a doc link
//! that later becomes code. Reading the bytes catches all of them and costs nothing.
//!
//! Two things are not uses and are stripped from a line before it is judged, and both are prose
//! about the rule rather than a way around it:
//!
//! * a citation of `docs/adr/0014-sqlparser.md`, which every module explaining *why* it is
//!   contained wants to point at;
//! * the bare backticked name in a **doc comment** — a module that says "`sqlparser` types stop
//!   here" is documenting the boundary, not crossing it.
//!
//! The second exception is deliberately narrow. It matches the name alone, so a doc link like
//! `` [`sqlparser::ast::Statement`] `` still fails: that names a *type*, and a doc link is exactly
//! the thing that becomes code the day someone finds it useful. The test found four of these the
//! first time it ran and two of them were stale prose still saying "one file", which is a fair
//! return for a rule this blunt.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

/// The one module allowed to name the dependency (ADR 0014, as amended: one *module*).
const CONTAINED: &str = "src/parse";

/// The crate name as it appears in source. Both spellings, because a path may use either.
const NAMES: [&str; 2] = ["sqlparser", "sql_parser"];

/// A citation of the ADR, which is prose about the rule and not a use of the crate. Removed from a
/// line before it is judged, rather than allow-listing whole files: a file may cite the ADR *and*
/// go on to import the crate, and only the second is a failure.
const CITATION: &str = "0014-sqlparser";

/// The bare backticked name, which is how a doc comment refers to the dependency without using it.
/// Stripped only from doc comments, and only in this exact form — no `::`, so a doc link naming a
/// type is still a failure.
const IN_PROSE: &str = "`sqlparser`";

#[test]
fn sqlparser_is_named_nowhere_outside_the_parse_module() {
    let root = crate_root();
    let mut offenders = Vec::new();
    let mut scanned = 0;

    for file in rust_sources(&root.join("src")) {
        let relative = file.strip_prefix(&root).unwrap_or(&file);
        scanned += 1;
        if relative.starts_with(CONTAINED) {
            continue;
        }
        let body = std::fs::read_to_string(&file).unwrap();
        for (number, line) in body.lines().enumerate() {
            let mut judged = line.replace(CITATION, "<adr>");
            if line.trim_start().starts_with("//!") || line.trim_start().starts_with("///") {
                judged = judged.replace(IN_PROSE, "<the parser>");
            }
            if NAMES.iter().any(|name| judged.contains(name)) {
                offenders.push(format!(
                    "{}:{}: {}",
                    relative.display(),
                    number + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        scanned > 10,
        "only {scanned} source files were scanned; the walk is not finding them"
    );
    assert!(
        offenders.is_empty(),
        "ADR 0014: `sqlparser` may be named only in `{CONTAINED}/`. It escaped into:\n{}\n\
         \nA type from it outside that module is what turns a one-module replacement into a \
         rewrite. Lower it into `crate::plan` inside `{CONTAINED}/` and hand that out instead.",
        offenders.join("\n")
    );
}

/// And the module really does name it — a containment test that passes because the dependency is
/// gone entirely would be a test that stopped meaning anything.
#[test]
fn the_parse_module_is_where_it_actually_lives() {
    let root = crate_root();
    let named: Vec<PathBuf> = rust_sources(&root.join(CONTAINED))
        .into_iter()
        .filter(|file| {
            let body = std::fs::read_to_string(file).unwrap();
            NAMES.iter().any(|name| body.contains(name))
        })
        .collect();
    assert!(
        !named.is_empty(),
        "no file in `{CONTAINED}/` names `sqlparser`; either it is gone, in which case this rule \
         and the ADR behind it should go with it, or the walk is broken"
    );
}

/// The manifest is the other place the name legitimately appears, and it has to: something has to
/// declare the dependency. This pins *where*, so the crate cannot be pulled in a second time under
/// another name or as a non-workspace dependency.
#[test]
fn the_dependency_is_declared_once_and_from_the_workspace() {
    let manifest = std::fs::read_to_string(crate_root().join("Cargo.toml")).unwrap();
    let declarations: Vec<&str> = manifest
        .lines()
        .filter(|line| line.trim_start().starts_with("sqlparser"))
        .collect();
    assert_eq!(
        declarations.len(),
        1,
        "`sqlparser` should be declared exactly once: {declarations:?}"
    );
    assert!(
        declarations[0].contains("workspace"),
        "the version is pinned in `[workspace.dependencies]` (ADR 0014), not here: {}",
        declarations[0]
    );
}

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `.rs` file under `directory`, recursively.
fn rust_sources(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            found.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
    found.sort();
    found
}
