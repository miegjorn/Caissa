//! Handoff Bridge — parsing of `@guilhem handoff` work orders (K-1 / O-1 / O-2).
//!
//! Pierre-Luc sends structured work orders from Matrix #coordination that
//! Guilhem intercepts *before* the conversational flow and dispatches
//! autonomously. The wire format (O-1) is:
//!
//! ```text
//! @guilhem handoff domain:<X> facet:<Y> task:"<description>"
//! ```
//!
//! Optional fields: `context_ref:<farga_project>`, `farga_project:<id>`,
//! `allowed_tools:<tool1,tool2>`.
//!
//! This module is *pure* — message text in, validated struct or typed error
//! out. No I/O, no dispatch. The HTTP interception, Farga traceability signal,
//! dispatcher call, and status polling live in `listen.rs` so this parsing can
//! be unit-tested in isolation (see the `tests` module at the bottom).

/// Component domains the dispatcher knows how to spawn. Mirrors
/// `dispatch::list_specs` and the dispatcher tool schema.
pub const VALID_DOMAINS: &[&str] = &[
    "farga", "gardian", "amassada", "charradissa", "cor", "caissa", "fondament", "occitan",
];

/// Role facets the dispatcher knows how to spawn.
pub const VALID_FACETS: &[&str] = &["architect", "developer", "qa", "infra", "db", "security"];

/// A parsed, validated handoff work order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffRequest {
    pub domain: String,
    pub facet: String,
    pub task: String,
    pub context_ref: Option<String>,
    pub farga_project: Option<String>,
    pub allowed_tools: Option<String>,
}

/// Why a message could not be turned into a [`HandoffRequest`]. Each variant
/// renders (via `Display`) into the Matrix message Guilhem posts back to the
/// room, so the sender learns exactly what to fix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffParseError {
    /// The message is not a handoff at all (no `handoff` marker). Used by the
    /// router to fall through to the conversational flow — never surfaced.
    NotHandoff,
    /// A required field (`domain`, `facet`, or `task`) was absent.
    MissingField(&'static str),
    /// `task:""` — present but empty.
    EmptyTask,
    /// `domain:<x>` where `<x>` is not in [`VALID_DOMAINS`].
    UnknownDomain(String),
    /// `facet:<y>` where `<y>` is not in [`VALID_FACETS`].
    UnknownFacet(String),
}

impl std::fmt::Display for HandoffParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandoffParseError::NotHandoff => write!(f, "not a handoff message"),
            HandoffParseError::MissingField(field) => write!(
                f,
                "handoff rejeté — champ requis manquant : `{field}`. \
                 Format : `@guilhem handoff domain:<X> facet:<Y> task:\"<description>\"`"
            ),
            HandoffParseError::EmptyTask => write!(
                f,
                "handoff rejeté — `task` est vide. Décris la tâche : `task:\"...\"`"
            ),
            HandoffParseError::UnknownDomain(d) => write!(
                f,
                "handoff rejeté — domaine inconnu `{d}`. Domaines valides : {}",
                VALID_DOMAINS.join(" | ")
            ),
            HandoffParseError::UnknownFacet(y) => write!(
                f,
                "handoff rejeté — facet inconnue `{y}`. Facets valides : {}",
                VALID_FACETS.join(" | ")
            ),
        }
    }
}

impl std::error::Error for HandoffParseError {}

/// True if `content` looks like a handoff work order and should be routed to
/// the mechanical dispatch path instead of the conversational flow (K-1).
///
/// Tolerant of an optional `@guilhem` / `guilhem` mention prefix (Charradissa
/// may or may not strip it) but anchored at the start so a conversational
/// sentence that merely mentions the word "handoff" is *not* intercepted.
pub fn is_handoff_message(content: &str) -> bool {
    handoff_marker_end(content).is_some()
}

/// If `content` begins with the handoff marker (optionally prefixed by an
/// `@guilhem` / `guilhem` mention), returns the byte offset just past the
/// `handoff` keyword — i.e. where the `key:value` fields begin. Returns `None`
/// for anything that is not a handoff order.
fn handoff_marker_end(content: &str) -> Option<usize> {
    let trimmed_start = content.len() - content.trim_start().len();
    let rest = &content[trimmed_start..];
    let lower = rest.to_ascii_lowercase();

    // Optional mention prefix: "@guilhem " or "guilhem ".
    let after_mention = if let Some(stripped) = lower.strip_prefix("@guilhem") {
        rest.len() - stripped.len()
    } else if let Some(stripped) = lower.strip_prefix("guilhem") {
        rest.len() - stripped.len()
    } else {
        0
    };

    // Whitespace between the mention (if any) and "handoff".
    let mention_segment = &rest[after_mention..];
    let ws = mention_segment.len() - mention_segment.trim_start().len();
    // A mention must be followed by whitespace before "handoff"; with no
    // mention we're already at the start.
    if after_mention > 0 && ws == 0 {
        return None;
    }
    let kw_start = after_mention + ws;
    let kw_segment = &rest[kw_start..];
    let kw_lower = kw_segment.to_ascii_lowercase();

    let after_kw = kw_lower.strip_prefix("handoff")?;
    // "handoff" must be a whole token: end of string or followed by whitespace.
    if after_kw.is_empty() || after_kw.starts_with(char::is_whitespace) {
        Some(trimmed_start + kw_start + "handoff".len())
    } else {
        None
    }
}

/// Parse and validate a raw Matrix message into a [`HandoffRequest`].
///
/// Fields are `key:value` tokens in any order. `task` is double-quoted and may
/// contain spaces; every other field is a single whitespace-delimited token.
/// Validation order is domain → facet → task so the most structural problem is
/// reported first.
pub fn parse_handoff_message(content: &str) -> Result<HandoffRequest, HandoffParseError> {
    let fields_start = match handoff_marker_end(content) {
        Some(end) => end,
        None => return Err(HandoffParseError::NotHandoff),
    };
    let fields = &content[fields_start..];

    let domain = match extract_token("domain", fields) {
        Some(d) => {
            if !VALID_DOMAINS.contains(&d.as_str()) {
                return Err(HandoffParseError::UnknownDomain(d));
            }
            d
        }
        None => return Err(HandoffParseError::MissingField("domain")),
    };

    let facet = match extract_token("facet", fields) {
        Some(f) => {
            if !VALID_FACETS.contains(&f.as_str()) {
                return Err(HandoffParseError::UnknownFacet(f));
            }
            f
        }
        None => return Err(HandoffParseError::MissingField("facet")),
    };

    let task = match extract_task(fields) {
        Some(t) if t.trim().is_empty() => return Err(HandoffParseError::EmptyTask),
        Some(t) => t,
        None => return Err(HandoffParseError::MissingField("task")),
    };

    Ok(HandoffRequest {
        domain,
        facet,
        task,
        context_ref: extract_token("context_ref", fields),
        farga_project: extract_token("farga_project", fields),
        allowed_tools: extract_token("allowed_tools", fields),
    })
}

/// Extract a single whitespace-delimited `key:value` token's value. Scans
/// token-by-token so a value can never accidentally absorb a later field, and
/// `key` only matches at a token boundary (so `farga_project` is not matched by
/// a search for `project`).
fn extract_token(key: &str, fields: &str) -> Option<String> {
    let prefix = format!("{key}:");
    for token in fields.split_whitespace() {
        if let Some(value) = token.strip_prefix(&prefix) {
            return Some(value.to_string());
        }
    }
    None
}

/// Extract the double-quoted `task:"..."` value, which may contain spaces.
/// Returns `Some("")` for `task:""` (the caller maps that to `EmptyTask`),
/// and `None` when there is no `task:` field at all.
fn extract_task(fields: &str) -> Option<String> {
    let marker = "task:\"";
    let start = fields.find(marker)? + marker.len();
    let rest = &fields[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_handoff_with_mention() {
        assert!(is_handoff_message(
            r#"@guilhem handoff domain:gardian facet:developer task:"fix the token cache""#
        ));
    }

    #[test]
    fn detects_handoff_without_mention() {
        assert!(is_handoff_message(
            r#"handoff domain:farga facet:qa task:"add a regression test""#
        ));
    }

    #[test]
    fn ignores_conversational_message_mentioning_handoff() {
        assert!(!is_handoff_message(
            "guilhem, can you handoff this work to the gardian agent?"
        ));
        assert!(!is_handoff_message("@guilhem what is the status of the dispatcher?"));
    }

    #[test]
    fn parses_valid_message_into_struct() {
        let req = parse_handoff_message(
            r#"@guilhem handoff domain:gardian facet:developer task:"implement two-hop resolution""#,
        )
        .expect("valid handoff should parse");
        assert_eq!(req.domain, "gardian");
        assert_eq!(req.facet, "developer");
        assert_eq!(req.task, "implement two-hop resolution");
        assert_eq!(req.context_ref, None);
        assert_eq!(req.farga_project, None);
        assert_eq!(req.allowed_tools, None);
    }

    #[test]
    fn parses_all_optional_fields() {
        let req = parse_handoff_message(
            r#"@guilhem handoff domain:amassada facet:infra task:"deploy the chart" context_ref:amassada farga_project:proj-42 allowed_tools:Bash,Edit,mcp__farga__write_signal"#,
        )
        .expect("valid handoff should parse");
        assert_eq!(req.domain, "amassada");
        assert_eq!(req.facet, "infra");
        assert_eq!(req.task, "deploy the chart");
        assert_eq!(req.context_ref.as_deref(), Some("amassada"));
        assert_eq!(req.farga_project.as_deref(), Some("proj-42"));
        assert_eq!(
            req.allowed_tools.as_deref(),
            Some("Bash,Edit,mcp__farga__write_signal")
        );
    }

    #[test]
    fn task_may_contain_spaces_and_punctuation() {
        let req = parse_handoff_message(
            r#"handoff domain:cor facet:developer task:"refactor the CLI: split spawn.rs, keep tests green""#,
        )
        .expect("quoted task with spaces should parse");
        assert_eq!(req.task, "refactor the CLI: split spawn.rs, keep tests green");
    }

    #[test]
    fn field_order_is_irrelevant() {
        let req = parse_handoff_message(
            r#"handoff task:"do the thing" facet:qa domain:caissa"#,
        )
        .expect("order should not matter");
        assert_eq!(req.domain, "caissa");
        assert_eq!(req.facet, "qa");
        assert_eq!(req.task, "do the thing");
    }

    #[test]
    fn rejects_unknown_domain() {
        let err = parse_handoff_message(
            r#"handoff domain:nonsense facet:developer task:"x""#,
        )
        .unwrap_err();
        assert_eq!(err, HandoffParseError::UnknownDomain("nonsense".to_string()));
    }

    #[test]
    fn rejects_unknown_facet() {
        let err = parse_handoff_message(
            r#"handoff domain:gardian facet:wizard task:"x""#,
        )
        .unwrap_err();
        assert_eq!(err, HandoffParseError::UnknownFacet("wizard".to_string()));
    }

    #[test]
    fn rejects_empty_task() {
        let err = parse_handoff_message(r#"handoff domain:gardian facet:developer task:"""#)
            .unwrap_err();
        assert_eq!(err, HandoffParseError::EmptyTask);
    }

    #[test]
    fn rejects_missing_facet() {
        let err =
            parse_handoff_message(r#"handoff domain:gardian task:"do something""#).unwrap_err();
        assert_eq!(err, HandoffParseError::MissingField("facet"));
    }

    #[test]
    fn rejects_missing_domain() {
        let err =
            parse_handoff_message(r#"handoff facet:developer task:"do something""#).unwrap_err();
        assert_eq!(err, HandoffParseError::MissingField("domain"));
    }

    #[test]
    fn rejects_missing_task() {
        let err =
            parse_handoff_message(r#"handoff domain:gardian facet:developer"#).unwrap_err();
        assert_eq!(err, HandoffParseError::MissingField("task"));
    }

    #[test]
    fn non_handoff_message_is_not_handoff_error() {
        let err = parse_handoff_message("just a normal question for guilhem").unwrap_err();
        assert_eq!(err, HandoffParseError::NotHandoff);
    }
}
