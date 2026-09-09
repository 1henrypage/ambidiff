//! Typed request payloads shared by every transport.
//!
//! The stdio engine keeps its `{id, method, params}` envelope and the
//! browser its `{type, id, ...}` envelope; both hand the payload object to
//! the decoders here AFTER envelope parsing, so one set of rules decides
//! what a valid comment, lifecycle, view, or expand request is:
//!
//! - an absent field and an explicit `null` both mean "omitted";
//! - any other value of the wrong JSON type is [`DecodeError::WrongType`];
//! - `line` / `endLine` must be integral and within `1..=u32::MAX`, never
//!   truncated ([`DecodeError::OutOfRange`] otherwise);
//! - `side` must be exactly `old` or `new`; a present unknown side fails
//!   with [`DecodeError::Unknown`], while an omitted side on a line comment
//!   defaults to `new` (the documented rule for added and context lines);
//! - view options default to the unified layout, word diff on, dark theme.
//!
//! The wire spelling of view options (`{mode, wordDiff, theme}`) is also the
//! `opts_json` the wasm painter passes, so [`ViewOptionsWire`] is the one
//! place that spelling lives. Pure and wasm-clean.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::highlight::ThemeChoice;
use crate::review::{NewComment, Side};
use crate::rows::ViewMode;
use crate::view::ViewOptions;

/// Why a request payload was rejected. `field` names the offending key in
/// its wire spelling (`endLine`, `gapId`, `commentId`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("missing required field {field:?}")]
    Missing { field: String },
    #[error("field {field:?} must be {expected}")]
    WrongType {
        field: String,
        expected: &'static str,
    },
    #[error("field {field:?} is out of range (expected an integer in 1..=4294967295)")]
    OutOfRange { field: String },
    #[error("field {field:?} has unknown value {value:?}")]
    Unknown { field: String, value: String },
}

impl DecodeError {
    /// The offending field in wire spelling.
    pub fn field(&self) -> &str {
        match self {
            DecodeError::Missing { field }
            | DecodeError::WrongType { field, .. }
            | DecodeError::OutOfRange { field }
            | DecodeError::Unknown { field, .. } => field,
        }
    }

    /// Stable lowerCamel kind for wire error payloads and fixtures.
    pub fn kind(&self) -> &'static str {
        match self {
            DecodeError::Missing { .. } => "missing",
            DecodeError::WrongType { .. } => "wrongType",
            DecodeError::OutOfRange { .. } => "outOfRange",
            DecodeError::Unknown { .. } => "unknown",
        }
    }
}

/// A new comment as requested over the wire; the author is optional here
/// because transports resolve it (explicit, `$AMBIDIFF_AUTHOR`, `$USER`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommentAddRequest {
    pub path: Option<String>,
    pub side: Option<Side>,
    pub line: Option<u32>,
    pub end_line: Option<u32>,
    pub body: String,
    pub author: Option<String>,
}

impl CommentAddRequest {
    /// Turn the request into the domain input once the author is resolved.
    /// Snippet capture happens later, inside the store lock.
    pub fn into_new_comment(self, author: String) -> NewComment {
        NewComment {
            path: self.path,
            side: self.side,
            line: self.line,
            end_line: self.end_line,
            snippet: None,
            body: self.body,
            author,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentEditRequest {
    pub id: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommentDeleteRequest {
    pub id: String,
}

/// Address / resolve / reopen; the action itself comes from the method or
/// message type, not from the payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecycleRequest {
    pub id: String,
    pub response: Option<String>,
}

/// Highlight theme on the wire; `none` disables highlighting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeWire {
    Dark,
    Light,
    None,
}

impl ThemeWire {
    pub fn highlight(self) -> Option<ThemeChoice> {
        match self {
            ThemeWire::Dark => Some(ThemeChoice::Dark),
            ThemeWire::Light => Some(ThemeChoice::Light),
            ThemeWire::None => None,
        }
    }
}

/// View options as every transport spells them: `{mode, wordDiff, theme}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewOptionsWire {
    pub mode: ViewMode,
    pub word_diff: bool,
    pub theme: ThemeWire,
}

impl Default for ViewOptionsWire {
    fn default() -> Self {
        ViewOptionsWire {
            mode: ViewMode::Unified,
            word_diff: true,
            theme: ThemeWire::Dark,
        }
    }
}

impl From<ViewOptionsWire> for ViewOptions {
    fn from(wire: ViewOptionsWire) -> Self {
        ViewOptions {
            mode: wire.mode,
            word_diff: wire.word_diff,
            highlight: wire.theme.highlight(),
        }
    }
}

impl From<ViewOptions> for ViewOptionsWire {
    fn from(opts: ViewOptions) -> Self {
        ViewOptionsWire {
            mode: opts.mode,
            word_diff: opts.word_diff,
            theme: match opts.highlight {
                Some(ThemeChoice::Dark) => ThemeWire::Dark,
                Some(ThemeChoice::Light) => ThemeWire::Light,
                None => ThemeWire::None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ViewRequest {
    pub path: String,
    pub options: ViewOptionsWire,
}

/// Expand one collapsed gap. Carries the full view options (the plugin
/// sends `mode` only; the rest take their defaults) so the transport can
/// return the resulting view alongside the spliced rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExpandRequest {
    pub path: String,
    pub gap_id: String,
    pub options: ViewOptionsWire,
}

// ---------------------------------------------------------------- helpers

fn object(v: &Value) -> Result<&serde_json::Map<String, Value>, DecodeError> {
    v.as_object().ok_or(DecodeError::WrongType {
        field: String::new(),
        expected: "an object",
    })
}

/// A present, non-null field.
fn present<'a>(v: &'a Value, field: &str) -> Option<&'a Value> {
    match v.get(field) {
        None | Some(Value::Null) => None,
        Some(other) => Some(other),
    }
}

fn opt_str(v: &Value, field: &str) -> Result<Option<String>, DecodeError> {
    match present(v, field) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(DecodeError::WrongType {
            field: field.to_string(),
            expected: "a string",
        }),
    }
}

fn req_str(v: &Value, field: &str) -> Result<String, DecodeError> {
    opt_str(v, field)?.ok_or_else(|| DecodeError::Missing {
        field: field.to_string(),
    })
}

/// A 1-based line coordinate: integral, `1..=u32::MAX`, never truncated.
fn opt_line(v: &Value, field: &str) -> Result<Option<u32>, DecodeError> {
    let Some(raw) = present(v, field) else {
        return Ok(None);
    };
    let Value::Number(n) = raw else {
        return Err(DecodeError::WrongType {
            field: field.to_string(),
            expected: "an integer",
        });
    };
    // Floats (`2.0`), negatives, and anything above u32::MAX are out of
    // range; `as_u64` is None for floats and negatives.
    let out_of_range = || DecodeError::OutOfRange {
        field: field.to_string(),
    };
    let value = n.as_u64().ok_or_else(out_of_range)?;
    let line = u32::try_from(value).map_err(|_| out_of_range())?;
    if line == 0 {
        return Err(out_of_range());
    }
    Ok(Some(line))
}

fn opt_bool(v: &Value, field: &str) -> Result<Option<bool>, DecodeError> {
    match present(v, field) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(DecodeError::WrongType {
            field: field.to_string(),
            expected: "a boolean",
        }),
    }
}

fn opt_side(v: &Value, field: &str) -> Result<Option<Side>, DecodeError> {
    match opt_str(v, field)?.as_deref() {
        None => Ok(None),
        Some("old") => Ok(Some(Side::Old)),
        Some("new") => Ok(Some(Side::New)),
        Some(other) => Err(DecodeError::Unknown {
            field: field.to_string(),
            value: other.to_string(),
        }),
    }
}

// --------------------------------------------------------------- decoders

/// Decode a `comment.add` payload. Field spelling is identical on stdio and
/// in the browser (`path`, `side`, `line`, `endLine`, `body`, `author`).
pub fn decode_comment_add(v: &Value) -> Result<CommentAddRequest, DecodeError> {
    object(v)?;
    let path = opt_str(v, "path")?;
    let line = opt_line(v, "line")?;
    let end_line = opt_line(v, "endLine")?;
    let explicit_side = opt_side(v, "side")?;
    // The documented default: a line comment without a side lands on the
    // new side. Without a line there is nothing to default.
    let side = explicit_side.or(line.map(|_| Side::New));
    let body = req_str(v, "body")?;
    let author = opt_str(v, "author")?;
    Ok(CommentAddRequest {
        path,
        side,
        line,
        end_line,
        body,
        author,
    })
}

/// Decode a `comment.edit` payload; `id_field` is `id` (stdio) or
/// `commentId` (browser).
pub fn decode_comment_edit(v: &Value, id_field: &str) -> Result<CommentEditRequest, DecodeError> {
    object(v)?;
    Ok(CommentEditRequest {
        id: req_str(v, id_field)?,
        body: req_str(v, "body")?,
    })
}

/// Decode a `comment.delete` payload; `id_field` as for edit.
pub fn decode_comment_delete(
    v: &Value,
    id_field: &str,
) -> Result<CommentDeleteRequest, DecodeError> {
    object(v)?;
    Ok(CommentDeleteRequest {
        id: req_str(v, id_field)?,
    })
}

/// Decode an address / resolve / reopen payload; `id_field` as for edit.
/// A present but blank response is treated as omitted (nothing is stored).
pub fn decode_lifecycle(v: &Value, id_field: &str) -> Result<LifecycleRequest, DecodeError> {
    object(v)?;
    let response = opt_str(v, "response")?.filter(|s| !s.trim().is_empty());
    Ok(LifecycleRequest {
        id: req_str(v, id_field)?,
        response,
    })
}

/// Decode `{mode?, wordDiff?, theme?}` with the documented defaults.
pub fn decode_view_options(v: &Value) -> Result<ViewOptionsWire, DecodeError> {
    object(v)?;
    let defaults = ViewOptionsWire::default();
    let mode = match opt_str(v, "mode")?.as_deref() {
        None => defaults.mode,
        Some("unified") => ViewMode::Unified,
        Some("split") => ViewMode::Split,
        Some(other) => {
            return Err(DecodeError::Unknown {
                field: "mode".to_string(),
                value: other.to_string(),
            });
        }
    };
    let word_diff = opt_bool(v, "wordDiff")?.unwrap_or(defaults.word_diff);
    let theme = match opt_str(v, "theme")?.as_deref() {
        None => defaults.theme,
        Some("dark") => ThemeWire::Dark,
        Some("light") => ThemeWire::Light,
        Some("none") => ThemeWire::None,
        Some(other) => {
            return Err(DecodeError::Unknown {
                field: "theme".to_string(),
                value: other.to_string(),
            });
        }
    };
    Ok(ViewOptionsWire {
        mode,
        word_diff,
        theme,
    })
}

/// Decode a `view` payload: `{path, mode?, wordDiff?, theme?}`.
pub fn decode_view(v: &Value) -> Result<ViewRequest, DecodeError> {
    object(v)?;
    Ok(ViewRequest {
        path: req_str(v, "path")?,
        options: decode_view_options(v)?,
    })
}

/// Decode an `expand` payload: `{path, gapId, mode?, wordDiff?, theme?}`.
pub fn decode_expand(v: &Value) -> Result<ExpandRequest, DecodeError> {
    object(v)?;
    Ok(ExpandRequest {
        path: req_str(v, "path")?,
        gap_id: req_str(v, "gapId")?,
        options: decode_view_options(v)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn omitted_side_on_a_line_comment_defaults_to_new() {
        let req = decode_comment_add(&json!({"path": "a.rs", "line": 3, "body": "b"})).expect("ok");
        assert_eq!(req.side, Some(Side::New));
        assert_eq!(req.line, Some(3));
        assert_eq!(req.end_line, None);
    }

    #[test]
    fn null_is_omitted_and_no_line_means_no_side() {
        let req =
            decode_comment_add(&json!({"path": null, "line": null, "side": null, "body": "b"}))
                .expect("ok");
        assert_eq!(req.path, None);
        assert_eq!(req.side, None);
        assert_eq!(req.line, None);
    }

    #[test]
    fn explicit_old_side_is_kept_and_unknown_side_fails() {
        let req = decode_comment_add(&json!({"path": "a", "line": 1, "side": "old", "body": "b"}))
            .expect("ok");
        assert_eq!(req.side, Some(Side::Old));
        let err = decode_comment_add(&json!({"path": "a", "line": 1, "side": "left", "body": "b"}))
            .expect_err("unknown side");
        assert_eq!(
            err,
            DecodeError::Unknown {
                field: "side".into(),
                value: "left".into()
            }
        );
    }

    #[test]
    fn line_coordinates_are_never_truncated() {
        for (value, kind) in [
            (json!(4_294_967_297u64), "outOfRange"),
            (json!(4_294_967_296u64), "outOfRange"),
            (json!(0), "outOfRange"),
            (json!(-1), "outOfRange"),
            (json!(2.5), "outOfRange"),
            (json!("2"), "wrongType"),
            (json!(true), "wrongType"),
        ] {
            let err = decode_comment_add(&json!({"path": "a", "line": value, "body": "b"}))
                .expect_err("rejected");
            assert_eq!(err.kind(), kind, "{value}");
            assert_eq!(err.field(), "line");
        }
        let req = decode_comment_add(&json!({"path": "a", "line": 4_294_967_295u64, "body": "b"}))
            .expect("u32::MAX is representable");
        assert_eq!(req.line, Some(u32::MAX));
    }

    #[test]
    fn end_line_uses_its_wire_spelling_in_errors() {
        let err = decode_comment_add(&json!({"path": "a", "line": 1, "endLine": "x", "body": "b"}))
            .expect_err("rejected");
        assert_eq!(err.field(), "endLine");
    }

    #[test]
    fn body_is_required_and_must_be_a_string() {
        assert_eq!(
            decode_comment_add(&json!({"path": "a"})),
            Err(DecodeError::Missing {
                field: "body".into()
            })
        );
        assert_eq!(
            decode_comment_add(&json!({"body": 5})),
            Err(DecodeError::WrongType {
                field: "body".into(),
                expected: "a string"
            })
        );
        assert!(
            decode_comment_add(&json!([1])).is_err(),
            "non-object payload"
        );
    }

    #[test]
    fn lifecycle_id_field_differs_per_transport_and_blank_response_is_omitted() {
        let stdio = decode_lifecycle(&json!({"id": "c-1", "response": "done"}), "id").expect("ok");
        assert_eq!(stdio.id, "c-1");
        assert_eq!(stdio.response.as_deref(), Some("done"));
        let web = decode_lifecycle(&json!({"commentId": "c-2", "response": "  "}), "commentId")
            .expect("ok");
        assert_eq!(web.id, "c-2");
        assert_eq!(web.response, None);
        assert_eq!(
            decode_lifecycle(&json!({"id": "c-1"}), "commentId"),
            Err(DecodeError::Missing {
                field: "commentId".into()
            })
        );
    }

    #[test]
    fn edit_and_delete_decode_their_ids() {
        let edit = decode_comment_edit(&json!({"id": "c-1", "body": "new"}), "id").expect("ok");
        assert_eq!((edit.id.as_str(), edit.body.as_str()), ("c-1", "new"));
        let del = decode_comment_delete(&json!({"commentId": "c-9"}), "commentId").expect("ok");
        assert_eq!(del.id, "c-9");
        assert!(decode_comment_delete(&json!({"commentId": 9}), "commentId").is_err());
    }

    #[test]
    fn view_options_default_and_reject_unknown_values() {
        let opts = decode_view_options(&json!({})).expect("defaults");
        assert_eq!(opts, ViewOptionsWire::default());
        let opts =
            decode_view_options(&json!({"mode": "split", "wordDiff": false, "theme": "none"}))
                .expect("ok");
        assert_eq!(opts.mode, ViewMode::Split);
        assert!(!opts.word_diff);
        assert_eq!(opts.theme, ThemeWire::None);
        assert_eq!(
            decode_view_options(&json!({"mode": "sideways"})),
            Err(DecodeError::Unknown {
                field: "mode".into(),
                value: "sideways".into()
            })
        );
        assert_eq!(
            decode_view_options(&json!({"wordDiff": "yes"})),
            Err(DecodeError::WrongType {
                field: "wordDiff".into(),
                expected: "a boolean"
            })
        );
        assert_eq!(
            decode_view_options(&json!({"theme": "sepia"})),
            Err(DecodeError::Unknown {
                field: "theme".into(),
                value: "sepia".into()
            })
        );
    }

    #[test]
    fn view_and_expand_require_their_paths_and_gap_ids() {
        let view = decode_view(&json!({"path": "a.rs", "theme": "light"})).expect("ok");
        assert_eq!(view.path, "a.rs");
        assert_eq!(view.options.theme, ThemeWire::Light);
        assert_eq!(
            decode_view(&json!({"mode": "split"})),
            Err(DecodeError::Missing {
                field: "path".into()
            })
        );
        let expand = decode_expand(&json!({"path": "a.rs", "gapId": "before:0"})).expect("ok");
        assert_eq!(expand.gap_id, "before:0");
        assert_eq!(expand.options.mode, ViewMode::Unified);
        assert_eq!(
            decode_expand(&json!({"path": "a.rs"})),
            Err(DecodeError::Missing {
                field: "gapId".into()
            })
        );
    }

    #[test]
    fn view_options_round_trip_through_the_core_type() {
        for wire in [
            ViewOptionsWire::default(),
            ViewOptionsWire {
                mode: ViewMode::Split,
                word_diff: false,
                theme: ThemeWire::None,
            },
            ViewOptionsWire {
                mode: ViewMode::Unified,
                word_diff: true,
                theme: ThemeWire::Light,
            },
        ] {
            let core: ViewOptions = wire.into();
            assert_eq!(ViewOptionsWire::from(core), wire);
        }
        let json = serde_json::to_value(ViewOptionsWire::default()).expect("json");
        assert_eq!(
            json,
            json!({"mode": "unified", "wordDiff": true, "theme": "dark"})
        );
    }
}
