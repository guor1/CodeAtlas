//! Value types shared between extraction, structure and render layers.

use serde::{Deserialize, Serialize};

/// What kind of symbol a row in `symbols` describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Class,
    Interface,
    Enum,
    EnumMember,
    Method,
    Field,
    Constant,
    Annotation,
    Record,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Enum => "enum",
            Self::EnumMember => "enum_member",
            Self::Method => "method",
            Self::Field => "field",
            Self::Constant => "constant",
            Self::Annotation => "annotation",
            Self::Record => "record",
        }
    }

    /// True for symbols that can own a body and therefore participate in call graphs.
    pub fn is_callable(self) -> bool {
        matches!(self, Self::Method)
    }
}

/// A SQL operation observed against a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableOp {
    Select,
    Insert,
    Update,
    Delete,
}

impl TableOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Select => "select",
            Self::Insert => "insert",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

/// How the system can be entered from outside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntrypointKind {
    /// Dubbo RPC service exported via `<dubbo:service>`.
    Dubbo,
    /// HTTP route from Spring MVC annotations.
    Http,
    /// Scheduled job (XXL-Job handler or job-module service).
    Job,
    /// Message queue consumer.
    Mq,
}

impl EntrypointKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Dubbo => "dubbo",
            Self::Http => "http",
            Self::Job => "job",
            Self::Mq => "mq",
        }
    }

    pub fn label_zh(self) -> &'static str {
        match self {
            Self::Dubbo => "Dubbo 接口",
            Self::Http => "HTTP 路由",
            Self::Job => "定时任务",
            Self::Mq => "消息消费",
        }
    }
}

/// A parsed Java type reference, reduced to what resolution needs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Annotation {
    pub name: String,
    /// Raw argument text keyed by parameter name; positional args use `"value"`.
    pub args: Vec<(String, String)>,
}

impl Annotation {
    pub fn arg(&self, key: &str) -> Option<&str> {
        self.args.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// The conventional single argument, accepting both `@X("v")` and `@X(value = "v")`.
    pub fn value(&self) -> Option<&str> {
        self.arg("value")
    }
}

/// One symbol as produced by a language probe, before it gets an id.
///
/// `Serialize`/`Deserialize` let a file's whole parse result round-trip through
/// `parse_cache`, which is how `catlas sync` avoids re-parsing unchanged files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawSymbol {
    pub kind: SymbolKind,
    pub name: String,
    pub fqn: Option<String>,
    pub signature: Option<String>,
    pub start_line: u32,
    pub end_line: u32,
    pub doc: Option<String>,
    pub visibility: Option<String>,
    /// Index into the same batch's symbol vector.
    pub parent: Option<usize>,
    pub annotations: Vec<Annotation>,
    /// Unresolved callee names collected from the body.
    pub calls: Vec<String>,
    /// Names in the `extends`/`implements` clauses of a type declaration.
    ///
    /// These projects call services through their interfaces, so without the
    /// interface-to-implementation link a call graph stops dead at every service
    /// boundary and no trace ever reaches a table.
    pub supertypes: Vec<String>,
}

/// Per-file extraction result from the Java probe.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JavaFile {
    pub package: Option<String>,
    pub imports: Vec<String>,
    pub symbols: Vec<RawSymbol>,
    pub had_parse_error: bool,
}
