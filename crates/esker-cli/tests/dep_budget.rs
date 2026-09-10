//! The dependency budget: the second half of the pure-Rust guard.
//!
//! `deny.toml` names crates we already know to be a problem. This test catches the ones we
//! have not seen yet: it walks the real runtime dependency graph from `cargo metadata`,
//! counts it against the budget recorded in `deny.toml`, and checks every crate name against
//! the *patterns* from `CLAUDE.md` — `*-sys`, `cc`, `openssl*`, `ring`, `aws-lc-*`, `libz*`,
//! `zstd*` — which a list of exact names cannot express.
//!
//! Two details matter for it to mean anything:
//!
//! * **Dev dependencies do not count.** `proptest` legitimately pulls in `rand`, and the rule
//!   is about what Esker ships, not about what tests it with.
//! * **The graph is filtered to the host platform.** Without that, `cargo metadata` also
//!   reports the Windows tree — a dozen `windows-*` crates for a target nothing here builds.
//!
//! The JSON is parsed by the small reader at the bottom of this file rather than by `serde`,
//! which is banned (`docs/adr/0002-formats-are-hand-rolled.md`). It is a test-only reader for
//! one known-shaped document, not a general-purpose library.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use json::Json;

/// Name patterns no runtime crate may match (`CLAUDE.md`, "Dependency policy").
///
/// A `*` may appear at either end. These are patterns rather than names on purpose: the point
/// is to catch `some-new-thing-sys` before anyone has heard of it.
const BANNED_PATTERNS: &[&str] = &[
    // Anything that compiles C, C++ or assembly.
    "*-sys",
    "cc",
    "openssl*",
    "ring",
    "aws-lc-*",
    "libz*",
    "zstd*",
    "bzip2*",
    "lz4-sys",
    // Things this project exists to write itself.
    "rocksdb",
    "sled",
    "openraft",
    "raft",
    "crc32c",
    "crc32fast",
    "rand",
    "lru",
    "snap",
    "pgwire",
    // Hand-rolled formats, never serde or prost.
    "serde",
    "serde_derive",
    "prost*",
    "tonic*",
];

#[test]
fn the_runtime_graph_stays_inside_its_budget() {
    let graph = Graph::load();
    let budget = read_budget();

    let external = graph.reachable(EdgeKinds::Normal);
    let count = external.len();

    // Printed so that the number is evidence in a test run, not something to go and derive.
    println!("runtime transitive crates: {count} of {budget} allowed");
    println!("  {}", joined(&external));

    assert!(
        count <= budget,
        "the runtime dependency graph has {count} crates, over the budget of {budget}.\n\
         Crates: {}\n\
         Raising the budget needs an ADR (CLAUDE.md, \"Dependency policy\").",
        joined(&external),
    );

    // A budget test that measured an empty graph would pass forever.
    assert!(
        count > 0,
        "the dependency graph came back empty; the test is measuring nothing"
    );
}

#[test]
fn no_runtime_or_build_crate_matches_a_banned_pattern() {
    let graph = Graph::load();
    // Build dependencies are included here even though they do not count against the budget:
    // a build script is exactly how a C compiler gets into a "pure Rust" graph.
    let names = graph.reachable(EdgeKinds::NormalAndBuild);

    let offenders: Vec<String> = names
        .iter()
        .filter_map(|name| {
            BANNED_PATTERNS
                .iter()
                .find(|pattern| matches_pattern(name, pattern))
                .map(|pattern| format!("{name} (matches `{pattern}`)"))
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "banned crates in the runtime or build graph:\n  {}\n\
         See CLAUDE.md \"Dependency policy\"; adding one of these needs an ADR.",
        offenders.join("\n  ")
    );
}

/// The patterns are only useful if they actually match, and a typo in one would make it match
/// nothing while the test still passed.
#[test]
fn the_ban_patterns_match_what_they_are_meant_to() {
    assert!(matches_pattern("openssl-sys", "*-sys"));
    assert!(matches_pattern("libsqlite3-sys", "*-sys"));
    assert!(matches_pattern("openssl", "openssl*"));
    assert!(matches_pattern("openssl-probe", "openssl*"));
    assert!(matches_pattern("aws-lc-rs", "aws-lc-*"));
    assert!(matches_pattern("zstd-safe", "zstd*"));
    assert!(matches_pattern("cc", "cc"));

    // And must not match things that are fine.
    assert!(!matches_pattern("bytes", "*-sys"));
    assert!(!matches_pattern("system-configuration", "*-sys"));
    assert!(!matches_pattern("crossbeam-skiplist", "cc"));
    assert!(!matches_pattern("random-word", "rand"));
    assert!(!matches_pattern("open", "openssl*"));
}

/// `deny.toml` and this test must agree on the number, so the number lives in one place.
#[test]
fn the_budget_is_readable_and_sane() {
    let budget = read_budget();
    assert!((1..=200).contains(&budget), "implausible budget {budget}");
}

fn joined(names: &BTreeSet<String>) -> String {
    names.iter().cloned().collect::<Vec<_>>().join(", ")
}

/// `*` is allowed at either end of a pattern; everything else is an exact match.
fn matches_pattern(name: &str, pattern: &str) -> bool {
    match (pattern.strip_prefix('*'), pattern.strip_suffix('*')) {
        (Some(suffix), None) => name.ends_with(suffix),
        (None, Some(prefix)) => name.starts_with(prefix),
        (Some(_), Some(_)) => name.contains(pattern.trim_matches('*')),
        (None, None) => name == pattern,
    }
}

/// Reads the budget from the marked comment in `deny.toml`, so that the cargo-deny config and
/// this test cannot drift apart.
fn read_budget() -> usize {
    const MARKER: &str = "# esker-dep-budget = ";
    let path = workspace_root().join("deny.toml");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));

    text.lines()
        .find_map(|line| line.trim().strip_prefix(MARKER))
        .unwrap_or_else(|| panic!("no line starting with `{MARKER}` in {}", path.display()))
        .trim()
        .parse()
        .expect("the dependency budget is not a number")
}

fn workspace_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("cannot resolve the workspace root")
}

/// Which dependency edges to follow.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EdgeKinds {
    /// Only edges that end up in the shipped binary.
    Normal,
    /// Runtime plus build scripts.
    NormalAndBuild,
}

/// The resolved dependency graph, as much of it as this test needs.
struct Graph {
    /// Package id → package name.
    names: BTreeMap<String, String>,
    /// Package id → its dependencies and the kinds of edge that reach them.
    edges: BTreeMap<String, Vec<(String, Vec<Kind>)>>,
    /// Ids of the crates in this workspace.
    members: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Normal,
    Build,
    Dev,
}

impl Graph {
    fn load() -> Self {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let output = Command::new(&cargo)
            .args([
                "metadata",
                "--format-version",
                "1",
                "--locked",
                "--filter-platform",
                &host_triple(),
            ])
            .current_dir(workspace_root())
            .output()
            .expect("failed to run cargo metadata");

        assert!(
            output.status.success(),
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        let text = String::from_utf8(output.stdout).expect("cargo metadata emitted invalid UTF-8");
        let document = json::parse(&text).expect("cargo metadata emitted invalid JSON");

        let mut names = BTreeMap::new();
        for package in document
            .get("packages")
            .and_then(Json::as_array)
            .unwrap_or(&[])
        {
            let (Some(id), Some(name)) = (
                package.get("id").and_then(Json::as_str),
                package.get("name").and_then(Json::as_str),
            ) else {
                continue;
            };
            names.insert(id.to_owned(), name.to_owned());
        }

        let resolve = document
            .get("resolve")
            .expect("cargo metadata has no resolve section");
        let mut edges = BTreeMap::new();
        for node in resolve.get("nodes").and_then(Json::as_array).unwrap_or(&[]) {
            let Some(id) = node.get("id").and_then(Json::as_str) else {
                continue;
            };
            let mut dependencies = Vec::new();
            for dep in node.get("deps").and_then(Json::as_array).unwrap_or(&[]) {
                let Some(pkg) = dep.get("pkg").and_then(Json::as_str) else {
                    continue;
                };
                let kinds = dep
                    .get("dep_kinds")
                    .and_then(Json::as_array)
                    .unwrap_or(&[])
                    .iter()
                    .map(|entry| match entry.get("kind").and_then(Json::as_str) {
                        // cargo writes a normal dependency's kind as null.
                        None => Kind::Normal,
                        Some("build") => Kind::Build,
                        Some(_) => Kind::Dev,
                    })
                    .collect();
                dependencies.push((pkg.to_owned(), kinds));
            }
            edges.insert(id.to_owned(), dependencies);
        }

        let members = document
            .get("workspace_members")
            .and_then(Json::as_array)
            .unwrap_or(&[])
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect();

        Self {
            names,
            edges,
            members,
        }
    }

    /// Names of every external crate reachable from the workspace over the given edge kinds.
    ///
    /// Workspace members are excluded: our own crates are not dependencies we took on.
    fn reachable(&self, follow: EdgeKinds) -> BTreeSet<String> {
        let wanted = |kinds: &[Kind]| {
            kinds.iter().any(|kind| match kind {
                Kind::Normal => true,
                Kind::Build => follow == EdgeKinds::NormalAndBuild,
                Kind::Dev => false,
            })
        };

        let mut seen: BTreeSet<String> = self.members.iter().cloned().collect();
        let mut queue: Vec<String> = self.members.clone();
        let mut external = BTreeSet::new();

        while let Some(id) = queue.pop() {
            for (dependency, kinds) in self.edges.get(&id).into_iter().flatten() {
                if !wanted(kinds) || !seen.insert(dependency.clone()) {
                    continue;
                }
                queue.push(dependency.clone());
                if !self.members.contains(dependency)
                    && let Some(name) = self.names.get(dependency)
                {
                    external.insert(name.clone());
                }
            }
        }

        external
    }
}

fn host_triple() -> String {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .expect("failed to run rustc -vV");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .expect("rustc -vV did not report a host triple")
        .trim()
        .to_owned()
}

/// A minimal JSON reader.
///
/// `serde` is banned, and this only has to read one document whose shape we control. It is
/// deliberately small: parse into a tree, look things up by key, and report a position on
/// failure rather than panicking.
mod json {
    /// A parsed JSON value. Objects keep insertion order in a `Vec`, which is enough for
    /// lookups here and avoids a hash map.
    #[derive(Debug, Clone, PartialEq)]
    pub(super) enum Json {
        Null,
        Bool(bool),
        Number(f64),
        String(String),
        Array(Vec<Json>),
        Object(Vec<(String, Json)>),
    }

    impl Json {
        /// The value at `key`, if this is an object that has one.
        pub(super) fn get(&self, key: &str) -> Option<&Json> {
            match self {
                Json::Object(entries) => entries
                    .iter()
                    .find_map(|(name, value)| (name == key).then_some(value)),
                _ => None,
            }
        }

        pub(super) fn as_array(&self) -> Option<&[Json]> {
            match self {
                Json::Array(items) => Some(items),
                _ => None,
            }
        }

        pub(super) fn as_str(&self) -> Option<&str> {
            match self {
                Json::String(text) => Some(text),
                _ => None,
            }
        }
    }

    /// Parses a complete JSON document.
    pub(super) fn parse(text: &str) -> Result<Json, String> {
        let bytes = text.as_bytes();
        let mut at = 0;
        let value = parse_value(bytes, &mut at)?;
        skip_whitespace(bytes, &mut at);
        if at != bytes.len() {
            return Err(format!("trailing input at byte {at}"));
        }
        Ok(value)
    }

    fn parse_value(bytes: &[u8], at: &mut usize) -> Result<Json, String> {
        skip_whitespace(bytes, at);
        match bytes.get(*at) {
            None => Err("unexpected end of input".to_owned()),
            Some(b'{') => parse_object(bytes, at),
            Some(b'[') => parse_array(bytes, at),
            Some(b'"') => Ok(Json::String(parse_string(bytes, at)?)),
            Some(b't') => literal(bytes, at, "true", Json::Bool(true)),
            Some(b'f') => literal(bytes, at, "false", Json::Bool(false)),
            Some(b'n') => literal(bytes, at, "null", Json::Null),
            Some(_) => parse_number(bytes, at),
        }
    }

    fn literal(bytes: &[u8], at: &mut usize, word: &str, value: Json) -> Result<Json, String> {
        if bytes[*at..].starts_with(word.as_bytes()) {
            *at += word.len();
            Ok(value)
        } else {
            Err(format!("expected `{word}` at byte {at}"))
        }
    }

    fn parse_object(bytes: &[u8], at: &mut usize) -> Result<Json, String> {
        *at += 1; // '{'
        let mut entries = Vec::new();
        skip_whitespace(bytes, at);
        if bytes.get(*at) == Some(&b'}') {
            *at += 1;
            return Ok(Json::Object(entries));
        }
        loop {
            skip_whitespace(bytes, at);
            let key = parse_string(bytes, at)?;
            skip_whitespace(bytes, at);
            if bytes.get(*at) != Some(&b':') {
                return Err(format!("expected `:` at byte {at}"));
            }
            *at += 1;
            entries.push((key, parse_value(bytes, at)?));
            skip_whitespace(bytes, at);
            match bytes.get(*at) {
                Some(b',') => *at += 1,
                Some(b'}') => {
                    *at += 1;
                    return Ok(Json::Object(entries));
                }
                _ => return Err(format!("expected `,` or `}}` at byte {at}")),
            }
        }
    }

    fn parse_array(bytes: &[u8], at: &mut usize) -> Result<Json, String> {
        *at += 1; // '['
        let mut items = Vec::new();
        skip_whitespace(bytes, at);
        if bytes.get(*at) == Some(&b']') {
            *at += 1;
            return Ok(Json::Array(items));
        }
        loop {
            items.push(parse_value(bytes, at)?);
            skip_whitespace(bytes, at);
            match bytes.get(*at) {
                Some(b',') => *at += 1,
                Some(b']') => {
                    *at += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(format!("expected `,` or `]` at byte {at}")),
            }
        }
    }

    fn parse_string(bytes: &[u8], at: &mut usize) -> Result<String, String> {
        if bytes.get(*at) != Some(&b'"') {
            return Err(format!("expected a string at byte {at}"));
        }
        *at += 1;
        let mut out = String::new();
        loop {
            let byte = *bytes.get(*at).ok_or("unterminated string")?;
            *at += 1;
            match byte {
                b'"' => return Ok(out),
                b'\\' => {
                    let escape = *bytes.get(*at).ok_or("unterminated escape")?;
                    *at += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex = bytes.get(*at..*at + 4).ok_or("truncated \\u escape")?;
                            let code = u32::from_str_radix(
                                std::str::from_utf8(hex).map_err(|_| "bad \\u escape")?,
                                16,
                            )
                            .map_err(|_| "bad \\u escape")?;
                            *at += 4;
                            // Surrogate halves are not something cargo emits; a replacement
                            // character keeps the parse total rather than panicking.
                            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                        }
                        other => return Err(format!("unknown escape `\\{}`", other as char)),
                    }
                }
                // Multi-byte UTF-8 arrives one byte at a time; collect and decode at the end.
                _ => {
                    let start = *at - 1;
                    while bytes
                        .get(*at)
                        .is_some_and(|byte| *byte != b'"' && *byte != b'\\')
                    {
                        *at += 1;
                    }
                    let chunk = std::str::from_utf8(&bytes[start..*at])
                        .map_err(|_| "invalid UTF-8 in string")?;
                    out.push_str(chunk);
                }
            }
        }
    }

    fn parse_number(bytes: &[u8], at: &mut usize) -> Result<Json, String> {
        let start = *at;
        while bytes.get(*at).is_some_and(|byte| {
            byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E')
        }) {
            *at += 1;
        }
        let text = std::str::from_utf8(&bytes[start..*at]).map_err(|_| "bad number")?;
        text.parse::<f64>()
            .map(Json::Number)
            .map_err(|_| format!("bad number `{text}` at byte {start}"))
    }

    fn skip_whitespace(bytes: &[u8], at: &mut usize) {
        while bytes
            .get(*at)
            .is_some_and(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        {
            *at += 1;
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_the_shapes_cargo_metadata_uses() {
            let text = r#"{"a": [1, 2.5, -3e2], "b": {"c": null}, "d": "x\nyA", "e": true}"#;
            let value = parse(text).unwrap();
            assert_eq!(value.get("a").unwrap().as_array().unwrap().len(), 3);
            assert_eq!(value.get("b").unwrap().get("c"), Some(&Json::Null));
            assert_eq!(value.get("d").unwrap().as_str(), Some("x\nyA"));
            assert_eq!(value.get("e"), Some(&Json::Bool(true)));
            assert_eq!(value.get("missing"), None);
        }

        #[test]
        fn handles_empty_containers_and_unicode() {
            assert_eq!(parse("[]").unwrap(), Json::Array(Vec::new()));
            assert_eq!(parse("{}").unwrap(), Json::Object(Vec::new()));
            assert_eq!(parse(r#""café ☕""#).unwrap().as_str(), Some("café ☕"));
        }

        #[test]
        fn malformed_input_is_an_error_not_a_panic() {
            for text in ["{", "[1,", r#"{"a" 1}"#, r#""unterminated"#, "tru", ""] {
                assert!(parse(text).is_err(), "`{text}` parsed successfully");
            }
        }
    }
}
