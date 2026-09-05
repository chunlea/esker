//! `EXPLAIN`'s option list, its four output formats, and the plan tree they all render.
//!
//! # One tree, four documents
//!
//! PostgreSQL renders the *same* plan four ways, and the text form is the odd one out: it is a
//! layout, while `JSON`, `XML` and `YAML` are a tree of named fields. So the node vocabulary lives
//! in one place — [`crate::plan::Node`]'s `describe`, which already produces a line and the
//! `Key: Value` details beneath it — and this module turns what that walk found into
//! [`PlanNode`]s. The text form still goes out through the walk that built it, unchanged; the
//! other three are rendered from the tree. Two renderers reading one description is what stops
//! `EXPLAIN` and `EXPLAIN (FORMAT JSON)` from ever describing different plans.
//!
//! # A declared divergence
//!
//! This server has no cost model, so every field PostgreSQL fills from one — `Startup Cost`,
//! `Total Cost`, `Plan Rows`, `Plan Width`, the `Buffers` counters — is **absent** rather than
//! zero. Under [ADR 0031](../../../docs/adr/0031-rails-compatibility-is-measured.md) an invented
//! number would be the worse answer of the two: a client that reads `"Total Cost": 0.00` is being
//! told something false, where one that finds no such key is being told the truth. The corpus
//! records both sides (`tests/captures/pg19_explain_options.txt`, replayed by
//! `tests/corpus/pg19_routing_explain.txt`).

use std::fmt::Write as _;

use crate::plan::Statement;
use crate::value::ColumnType;

/// `EXPLAIN`, the statement it is about, and what was asked of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Explain {
    /// The statement being explained.
    pub statement: Box<Statement>,
    /// `ANALYZE`, which **runs** the statement — that is what the word means on a real server, and
    /// it is why it stays refused for everything that writes. For a `SELECT` it is what puts the
    /// `ScanStats` a columnar answer carries into the plan (`docs/plans/phase-10-routing.md` U3).
    pub analyze: bool,
    /// `FORMAT`, which decides both the document and the type of the one column it comes back in.
    pub format: ExplainFormat,
}

/// What `EXPLAIN (FORMAT ...)` was asked for.
///
/// The four PostgreSQL has, and no fifth: an unrecognized value is
/// [`crate::SqlError::UnrecognizedExplainOptionValue`] rather than a silent fall back to text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExplainFormat {
    /// The default: one row per line of an indented layout.
    #[default]
    Text,
    /// One row, holding a `json` document.
    Json,
    /// One row, holding an `xml` document.
    Xml,
    /// One row, holding a `text` document in YAML's layout — YAML is not a type this server has,
    /// and it is not one PostgreSQL has either. Measured: `EXPLAIN (FORMAT YAML)` declares `text`.
    Yaml,
}

impl ExplainFormat {
    /// The type the one `QUERY PLAN` column is declared as.
    ///
    /// **This is a wire fact, not a rendering detail.** A client reads the column through the OID
    /// in the `RowDescription`, so a `json` document declared `text` is the class of defect ADR
    /// 0031 calls a wrong answer: the value is right and the client cannot use it. Measured with
    /// `\gdesc` against 19beta1, all four.
    #[must_use]
    pub fn column_type(self) -> ColumnType {
        match self {
            ExplainFormat::Text | ExplainFormat::Yaml => ColumnType::Text,
            ExplainFormat::Json => ColumnType::Json,
            ExplainFormat::Xml => ColumnType::Xml,
        }
    }

    /// The option's value as PostgreSQL's grammar leaves it, for the message that refuses one.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "text" => Some(ExplainFormat::Text),
            "json" => Some(ExplainFormat::Json),
            "xml" => Some(ExplainFormat::Xml),
            "yaml" => Some(ExplainFormat::Yaml),
            _ => None,
        }
    }
}

/// One node of a plan, as fields rather than as a line.
///
/// Built from the same `(line, extra)` pair the text walk prints, which is what keeps the two
/// descriptions from drifting apart.
#[derive(Debug, Clone, Default)]
pub struct PlanNode {
    /// What the node is: `Seq Scan`, `Nested Loop`, `Project (2 columns)`.
    pub kind: String,
    /// What it reads, when it reads a named thing.
    pub relation: Option<String>,
    /// The node's own details, in the order the text form prints them.
    pub fields: Vec<(String, String)>,
    /// The subtree beneath it.
    pub children: Vec<PlanNode>,
}

impl PlanNode {
    /// Splits one printed line and its detail lines into fields.
    ///
    /// **The split is at `" on "`**, which is how every node that names a relation prints one —
    /// `Seq Scan on authors`, `Catalog Scan on pg_class`, `Insert on posts`. No node *kind* in the
    /// vocabulary contains that string, and a kind that grew one would put its tail in
    /// `Relation Name` rather than lose it, so the failure is a mislabelled field and never a
    /// dropped one.
    pub(crate) fn new(line: &str, extra: Option<&str>) -> Self {
        let (kind, relation) = match line.split_once(" on ") {
            Some((kind, relation)) => (kind.to_owned(), Some(relation.to_owned())),
            None => (line.to_owned(), None),
        };
        let fields = extra
            .into_iter()
            .flat_map(|extra| extra.split('\n'))
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
            .map(|detail| match detail.split_once(": ") {
                Some((key, value)) => (key.to_owned(), value.to_owned()),
                // A detail line with no `Key: Value` shape still belongs to the node. It is given
                // a key rather than dropped: losing it is the one outcome nobody can debug.
                None => ("Detail".to_owned(), detail.to_owned()),
            })
            .collect();
        PlanNode {
            kind,
            relation,
            fields,
            children: Vec::new(),
        }
    }

    /// A statement with no access path to choose, from the one line it prints.
    ///
    /// `EXPLAIN CREATE TABLE t` has a plan in the sense that it has a name and nothing else; the
    /// three structured formats still have to answer, and this is what they answer with. Any line
    /// after the first becomes a detail of the node rather than a node of its own — there is no
    /// tree here to be the parent of one.
    #[must_use]
    pub fn from_lines(lines: &[String]) -> Self {
        let (first, rest) = lines
            .split_first()
            .map_or(("", &[][..]), |(first, rest)| (first.as_str(), rest));
        let extra = rest.join("\n");
        PlanNode::new(first, (!extra.is_empty()).then_some(extra.as_str()))
    }

    /// Renders this plan in one of the three structured formats, or `None` for `Text` — whose
    /// document is the walk's own lines and never passes through here.
    #[must_use]
    pub fn render(&self, format: ExplainFormat) -> Option<String> {
        match format {
            ExplainFormat::Text => None,
            ExplainFormat::Json => Some(self.json()),
            ExplainFormat::Xml => Some(self.xml()),
            ExplainFormat::Yaml => Some(self.yaml()),
        }
    }

    /// One field per line, in the order the text form prints them: what the node is, what it
    /// reads, its own details, then its subtree.
    fn entries(&self) -> Vec<(&str, &str)> {
        let mut entries = vec![("Node Type", self.kind.as_str())];
        if let Some(relation) = &self.relation {
            entries.push(("Relation Name", relation.as_str()));
        }
        entries.extend(
            self.fields
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        entries
    }

    /// PostgreSQL's `FORMAT JSON`: an array of one object, whose `Plan` is the root node.
    fn json(&self) -> String {
        let mut out = String::from("[\n  {\n");
        out.push_str("    \"Plan\": ");
        self.json_into(4, &mut out);
        out.push_str("\n  }\n]");
        out
    }

    fn json_into(&self, depth: usize, out: &mut String) {
        let pad = " ".repeat(depth);
        let inner = " ".repeat(depth + 2);
        out.push_str("{\n");
        let entries = self.entries();
        for (index, (key, value)) in entries.iter().enumerate() {
            out.push_str(&inner);
            crate::value::json::write_string(key, out);
            out.push_str(": ");
            crate::value::json::write_string(value, out);
            if index + 1 < entries.len() || !self.children.is_empty() {
                out.push(',');
            }
            out.push('\n');
        }
        if !self.children.is_empty() {
            let _ = writeln!(out, "{inner}\"Plans\": [");
            for (index, child) in self.children.iter().enumerate() {
                out.push_str(&inner);
                out.push_str("  ");
                child.json_into(depth + 4, out);
                if index + 1 < self.children.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            let _ = writeln!(out, "{inner}]");
        }
        out.push_str(&pad);
        out.push('}');
    }

    /// PostgreSQL's `FORMAT YAML`: a one-item list whose item is the root node under `Plan`.
    fn yaml(&self) -> String {
        let mut out = String::from("- Plan:\n");
        self.yaml_into(4, &mut out);
        out
    }

    fn yaml_into(&self, depth: usize, out: &mut String) {
        let pad = " ".repeat(depth);
        for (key, value) in self.entries() {
            let _ = writeln!(out, "{pad}{key}: \"{}\"", value.replace('"', "\\\""));
        }
        if !self.children.is_empty() {
            let _ = writeln!(out, "{pad}Plans:");
            for child in &self.children {
                // The list marker sits two columns in, and the item's own fields line up after it,
                // which is what makes a nested `Plans:` legible at four levels down.
                let _ = writeln!(
                    out,
                    "{pad}  - Node Type: \"{}\"",
                    child.kind.replace('"', "\\\"")
                );
                let mut rest = String::new();
                child.yaml_into(depth + 4, &mut rest);
                // The child's own walk re-prints `Node Type`, which the marker line already
                // carried: one line, dropped, rather than two renderers that must agree.
                for line in rest.lines().skip(1) {
                    let _ = writeln!(out, "{line}");
                }
            }
        }
    }

    /// PostgreSQL's `FORMAT XML`: one `explain` element, one `Query`, and `Plan` elements nested
    /// inside `Plans` — with every field name's spaces turned into dashes.
    fn xml(&self) -> String {
        let mut out =
            String::from("<explain xmlns=\"http://www.postgresql.org/2009/explain\">\n  <Query>\n");
        self.xml_into(4, &mut out);
        out.push_str("  </Query>\n</explain>");
        out
    }

    fn xml_into(&self, depth: usize, out: &mut String) {
        let pad = " ".repeat(depth);
        let inner = " ".repeat(depth + 2);
        let _ = writeln!(out, "{pad}<Plan>");
        for (key, value) in self.entries() {
            let tag = key.replace(' ', "-");
            let _ = writeln!(out, "{inner}<{tag}>{}</{tag}>", escape_xml(value));
        }
        if !self.children.is_empty() {
            let _ = writeln!(out, "{inner}<Plans>");
            for child in &self.children {
                child.xml_into(depth + 4, out);
            }
            let _ = writeln!(out, "{inner}</Plans>");
        }
        let _ = writeln!(out, "{pad}</Plan>");
    }
}

/// The five characters an XML text node may not carry as themselves.
fn escape_xml(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}
