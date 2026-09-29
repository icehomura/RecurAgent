//! `json_query` tool: jq-compatible JSON queries without shelling out.
//!
//! Models reach for `jq` through the `bash` tool, but the host shell is not
//! guaranteed to have it, its output arrives as raw bytes to be re-parsed, and
//! every invocation is a process spawn. This tool runs the same filter language
//! in-process (via `jaq`, a pure-Rust jq) so a query is one tool call with a
//! structured result.
//!
//! Input is either a JSON document passed inline or a path to a `.json` file.
//! The filter is standard jq syntax; `jaq_std` supplies the usual library
//! (`map`, `select`, `reduce`, `group_by`, ...).
//!
//! Effects are read-only: the tool never writes, and a file input is opened
//! read-only.

use crate::error::{Error, Result};
use crate::model::{ContentBlock, TextContent};
use crate::tools::{Tool, ToolEffects, ToolOutput, ToolUpdate};
use jaq_core::load::{Arena, File, Loader};
use jaq_core::{Compiler, Ctx, Vars, data, unwrap_valr};
use jaq_json::{Val, read};

/// Maximum bytes accepted for an inline JSON document or a query input file.
/// Keeps a single tool call from pulling an unbounded document into memory.
const JSON_QUERY_MAX_INPUT_BYTES: u64 = 16 * 1024 * 1024;

/// Maximum number of result values rendered back to the model. A filter like
/// `.[]` over a large array can match far more than fits in context.
const JSON_QUERY_MAX_RESULTS: usize = 1000;

/// Maximum characters of a single rendered result value.
const JSON_QUERY_MAX_RESULT_CHARS: usize = 4000;

/// Run a jq filter over a JSON value, returning one rendered line per output.
///
/// Split out from the tool so the language plumbing is testable without a
/// `ToolOutput` in the way.
///
/// # Errors
/// Returns a validation error when the filter does not parse, does not
/// compile, or the first output value is itself an error.
pub fn run_filter(filter_source: &str, input_json: &str) -> Result<Vec<String>> {
    let input: Val = read::parse_single(input_json.as_bytes()).map_err(|err| {
        Error::validation(format!("json_query: input is not valid JSON: {err}"))
    })?;

    let program = File {
        code: filter_source,
        path: (),
    };

    let defs = jaq_core::defs()
        .chain(jaq_std::defs())
        .chain(jaq_json::defs());
    let funs = jaq_core::funs()
        .chain(jaq_std::funs())
        .chain(jaq_json::funs());

    let loader = Loader::new(defs);
    let arena = Arena::default();
    let modules = loader.load(&arena, program).map_err(|errs| {
        // `load::Error` implements `Debug` only (no `Display`), and the loader
        // reports each failure as a `(source, error)` pair.
        let detail = errs
            .iter()
            .map(|(_, err)| format!("{err:?}"))
            .collect::<Vec<_>>()
            .join("; ");
        Error::validation(format!("json_query: filter did not parse: {detail}"))
    })?;

    let filter = Compiler::default()
        .with_funs(funs)
        .compile(modules)
        .map_err(|errs| {
            // Same `(source, errors)` shape as the loader; `Undefined` is
            // `Debug`-only, so render it that way.
            let detail = errs
                .iter()
                .map(|(_, undefined)| format!("{undefined:?}"))
                .collect::<Vec<_>>()
                .join("; ");
            Error::validation(format!("json_query: filter did not compile: {detail}"))
        })?;

    let ctx = Ctx::<data::JustLut<Val>>::new(&filter.lut, Vars::new([]));
    let mut rendered = Vec::new();
    for value in filter.id.run((ctx, input)).map(unwrap_valr) {
        if rendered.len() >= JSON_QUERY_MAX_RESULTS {
            break;
        }
        let value = value.map_err(|err| {
            Error::validation(format!("json_query: filter failed while running: {err}"))
        })?;
        rendered.push(render_value(&value));
    }
    Ok(rendered)
}

/// Render one output value the way `jq -c` would, clipped to a readable size.
fn render_value(value: &Val) -> String {
    let text = value.to_string();
    if text.chars().count() <= JSON_QUERY_MAX_RESULT_CHARS {
        return text;
    }
    let clipped: String = text.chars().take(JSON_QUERY_MAX_RESULT_CHARS).collect();
    format!("{clipped}… (truncated)")
}

/// The `json_query` built-in tool.
#[derive(Debug, Default, Clone, Copy)]
pub struct JsonQueryTool;

impl JsonQueryTool {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

/// Input accepted by [`JsonQueryTool`].
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct JsonQueryInput {
    /// jq filter expression, e.g. `.[] | select(.status == "open") | .name`.
    filter: String,
    /// Inline JSON document. Exactly one of `json` / `path` is required.
    #[serde(default)]
    json: Option<String>,
    /// Path to a JSON file to query. Exactly one of `json` / `path` is required.
    #[serde(default)]
    path: Option<String>,
}

impl JsonQueryInput {
    /// Resolve the document text from whichever source was supplied.
    fn document(&self) -> Result<String> {
        match (self.json.as_deref(), self.path.as_deref()) {
            (Some(_), Some(_)) => Err(Error::validation(
                "json_query: supply either `json` or `path`, not both",
            )),
            (None, None) => Err(Error::validation(
                "json_query: supply a JSON document via `json` or a file via `path`",
            )),
            (Some(text), None) => Ok(text.to_string()),
            (None, Some(path)) => {
                let metadata = std::fs::metadata(path).map_err(|err| {
                    Error::validation(format!("json_query: cannot read {path}: {err}"))
                })?;
                if metadata.len() > JSON_QUERY_MAX_INPUT_BYTES {
                    return Err(Error::validation(format!(
                        "json_query: {path} is {} bytes, over the {JSON_QUERY_MAX_INPUT_BYTES}-byte limit",
                        metadata.len()
                    )));
                }
                std::fs::read_to_string(path).map_err(|err| {
                    Error::validation(format!("json_query: cannot read {path}: {err}"))
                })
            }
        }
    }
}

#[async_trait::async_trait]
#[allow(clippy::unnecessary_literal_bound)]
impl Tool for JsonQueryTool {
    fn name(&self) -> &str {
        "json_query"
    }

    fn label(&self) -> &str {
        "json_query"
    }

    fn description(&self) -> &str {
        "Query a JSON document with a jq filter expression. Prefer this over \
         piping to a `jq` binary: it runs in-process, so it needs no `jq` on \
         PATH and returns a structured result in one call. Supply the document \
         inline via `json` or as a file via `path`. The filter is standard jq \
         syntax, e.g. `.[] | select(.status == \"open\") | .name` or \
         `{count: length, names: map(.name)}`. Returns one line per output \
         value (like `jq -c`)."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "filter": {
                    "type": "string",
                    "description": "jq filter expression to run over the document."
                },
                "json": {
                    "type": "string",
                    "description": "Inline JSON document to query. Mutually exclusive with `path`."
                },
                "path": {
                    "type": "string",
                    "description": "Path to a JSON file to query. Mutually exclusive with `json`."
                }
            },
            "required": ["filter"],
            "additionalProperties": false
        })
    }

    fn effects(&self) -> ToolEffects {
        // Reads a file or an inline string; never writes.
        ToolEffects::read()
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        input: serde_json::Value,
        _on_update: Option<Box<dyn Fn(ToolUpdate) + Send + Sync>>,
    ) -> Result<ToolOutput> {
        let input: JsonQueryInput = serde_json::from_value(input)
            .map_err(|err| Error::validation(format!("json_query: invalid input: {err}")))?;
        let document = input.document()?;
        let filter_for_run = input.filter.clone();
        // jaq's interpreter is CPU-bound; keep it off the agent loop.
        // `spawn_blocking_io` forces an `io::Result` payload, so the tool's own
        // `Result` is nested inside it (same shape as the AST tools): only a
        // pool/join failure becomes the outer error, keeping validation errors
        // distinguishable for the caller.
        let results = asupersync::runtime::spawn_blocking_io(move || {
            Ok::<_, std::io::Error>(run_filter(&filter_for_run, &document))
        })
        .await
        .map_err(|err| Error::tool("json_query", err.to_string()))??;

        let text = if results.is_empty() {
            "(no output)".to_string()
        } else {
            results.join("\n")
        };
        Ok(ToolOutput {
            content: vec![ContentBlock::Text(TextContent::new(text))],
            details: Some(serde_json::json!({
                "filter": input.filter,
                "resultCount": results.len(),
            })),
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iterates_an_array() {
        let out = run_filter(".[]", r#"["Hello", "world"]"#).expect("filter runs");
        assert_eq!(out, vec!["\"Hello\"", "\"world\""]);
    }

    #[test]
    fn selects_and_projects_fields() {
        let doc = r#"[{"name":"a","status":"open"},{"name":"b","status":"closed"}]"#;
        let out = run_filter(r#"[.[] | select(.status == "open") | .name]"#, doc)
            .expect("filter runs");
        assert_eq!(out, vec!["[\"a\"]"]);
    }

    #[test]
    fn builds_an_object_from_standard_library() {
        let out = run_filter("{n: length}", r#"[1,2,3]"#).expect("filter runs");
        assert_eq!(out, vec!["{\"n\":3}"]);
    }

    #[test]
    fn a_broken_filter_is_a_validation_error_not_a_panic() {
        let err = run_filter(".[", "{}").expect_err("bad filter must fail");
        assert!(
            err.to_string().contains("json_query"),
            "error should name the tool: {err}"
        );
    }

    #[test]
    fn invalid_input_json_is_a_validation_error() {
        let err = run_filter(".", "not json").expect_err("bad input must fail");
        assert!(err.to_string().contains("not valid JSON"), "{err}");
    }

    #[test]
    fn tool_contract_is_read_only_and_requires_a_filter() {
        let tool = JsonQueryTool::new();
        assert_eq!(tool.name(), "json_query");
        let params = tool.parameters();
        assert_eq!(params["required"][0], "filter");
        assert!(tool.effects().reads());
        assert!(!tool.effects().writes());
    }
}
