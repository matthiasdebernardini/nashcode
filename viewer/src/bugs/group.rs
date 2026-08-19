//! Grouping, mechanism `nashcode-v1`.
//!
//! An event's grouping key decides which issue it joins. The order, per
//! `goals/error-tracking/goal.md`:
//!
//! 1. An explicit SDK `fingerprint` wins. Parts join with ` ⋄ `, and `{{ default }}`
//!    substitutes the key the server would have computed.
//! 2. A native event (a frame with an `instruction_addr` inside a known debug image)
//!    groups on `(debug_id, instruction_addr − image_addr)` of the top in-app frame.
//!    That is stable per build and needs no symbolication.
//! 3. Otherwise the **last** exception in the chain: `{type}: {parameterized value}`.
//!    `mechanism.synthetic` replaces the type with the crash function name, because a
//!    synthetic type like `SIGSEGV` says nothing.
//! 4. No exception at all: `Log Message: {parameterized message}`.
//!
//! Parameterization is the step that makes message grouping credible: ids,
//! timestamps and addresses become placeholders, so two events that differ only by a
//! UUID are one issue. The rules are Sentry's, reimplemented from the public
//! description — uuid, url, email, date, ip, quoted string, hex, int, first two
//! lines, value capped at 1024 and type at 128.
//!
//! The mechanism name is stored per issue. Changing the algorithm in place would
//! split every open issue in half, so a future `nashcode-v2` has to be a new name and
//! an explicit migration, never an edit here.

use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The grouping mechanism this module implements. Stored with every issue.
pub const MECHANISM: &str = "nashcode-v1";

/// The separator Bugsink uses between fingerprint parts, kept because it cannot
/// appear in a type name and reads well in the UI.
const JOIN: &str = " ⋄ ";

const MAX_TYPE: usize = 128;
const MAX_VALUE: usize = 1024;

/// Everything grouping decided about one event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grouping {
    /// The readable key. Two events with the same key are one issue.
    pub key: String,
    /// The key hashed, which is what the index looks up.
    pub hash: String,
    /// `{type}: {value}`, what the issue list shows. Unaffected by a custom
    /// fingerprint: a fingerprint changes what groups together, not what it is called.
    pub title: String,
    /// Where it happened: the event's transaction, else the crashing function.
    pub culprit: Option<String>,
    pub level: String,
    pub mechanism: &'static str,
}

/// Group one event payload.
pub fn group(event: &Value) -> Grouping {
    let (ty, value) = exception_title(event);
    let title = format!("{ty}: {}", first_line(&value));
    let default_key = native_key(event).unwrap_or_else(|| format!("{ty}: {value}"));

    let key = match fingerprint(event) {
        Some(parts) => parts
            .iter()
            .map(|part| if part == "{{ default }}" { default_key.clone() } else { part.clone() })
            .collect::<Vec<_>>()
            .join(JOIN),
        None => default_key,
    };

    Grouping {
        hash: hash(&key),
        key,
        title,
        culprit: culprit(event),
        level: event
            .get("level")
            .and_then(Value::as_str)
            .filter(|level| !level.trim().is_empty())
            .unwrap_or("error")
            .to_owned(),
        mechanism: MECHANISM,
    }
}

fn hash(key: &str) -> String {
    let digest = Sha256::digest(key.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The `fingerprint` field, when it is a non-empty list of strings.
fn fingerprint(event: &Value) -> Option<Vec<String>> {
    let parts: Vec<String> = event
        .get("fingerprint")?
        .as_array()?
        .iter()
        .map(|part| match part {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect();
    (!parts.is_empty()).then_some(parts)
}

/// The type and the parameterized value the key and title are built from.
fn exception_title(event: &Value) -> (String, String) {
    let Some(last) = last_exception(event) else {
        let message = message_of(event);
        return ("Log Message".to_owned(), parameterize(&message));
    };

    let raw_type = last
        .get("type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Error");

    // A synthetic exception carries a machine type (`SIGSEGV`, `Fatal Error`) that
    // groups everything in the process together. The crashing function is the useful
    // discriminator instead.
    let synthetic = last
        .get("mechanism")
        .and_then(|mechanism| mechanism.get("synthetic"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let ty = match synthetic.then(|| crash_function(last)).flatten() {
        Some(function) => function,
        None => raw_type.to_owned(),
    };

    let value = last
        .get("value")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_default();

    (truncate(&ty, MAX_TYPE), parameterize(&value))
}

/// The last exception in the chain. Sentry orders `exception.values` oldest cause
/// first, so the last one is the exception that actually reached the handler.
fn last_exception(event: &Value) -> Option<&Value> {
    let exception = event.get("exception")?;
    let values = match exception {
        Value::Array(values) => values,
        Value::Object(_) => exception.get("values")?.as_array()?,
        _ => return None,
    };
    values.last()
}

/// The message of a non-exception event, in the three shapes SDKs send it.
fn message_of(event: &Value) -> String {
    let logentry = event.get("logentry");
    logentry
        .and_then(|entry| entry.get("formatted"))
        .and_then(Value::as_str)
        .or_else(|| logentry.and_then(|entry| entry.get("message")).and_then(Value::as_str))
        .or_else(|| event.get("message").and_then(Value::as_str))
        .or_else(|| {
            event
                .get("message")
                .and_then(|message| message.get("formatted"))
                .and_then(Value::as_str)
        })
        .unwrap_or_default()
        .to_owned()
}

/// The function name of the frame that crashed: the last in-app frame, else the last
/// frame at all.
fn crash_function(exception: &Value) -> Option<String> {
    top_frame(exception)?
        .get("function")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|name| !name.trim().is_empty())
}

fn frames(exception: &Value) -> Option<&Vec<Value>> {
    exception.get("stacktrace")?.get("frames")?.as_array()
}

/// The frame an event is *about*: the deepest in-app one, else the deepest one.
/// Sentry orders frames caller-first, so that is the last entry.
fn top_frame(exception: &Value) -> Option<&Value> {
    let frames = frames(exception)?;
    frames
        .iter()
        .rev()
        .find(|frame| frame.get("in_app").and_then(Value::as_bool).unwrap_or(false))
        .or_else(|| frames.last())
}

fn culprit(event: &Value) -> Option<String> {
    if let Some(transaction) =
        event.get("transaction").and_then(Value::as_str).filter(|value| !value.trim().is_empty())
    {
        return Some(truncate(transaction, MAX_VALUE));
    }
    let frame = last_exception(event).and_then(top_frame)?;
    let function = frame.get("function").and_then(Value::as_str)?;
    match frame.get("module").or_else(|| frame.get("filename")).and_then(Value::as_str) {
        Some(module) => Some(truncate(&format!("{module}.{function}"), MAX_VALUE)),
        None => Some(truncate(function, MAX_VALUE)),
    }
}

// ---- native events ---------------------------------------------------------------

/// `(debug_id, instruction_addr − image_addr)` of the top in-app frame.
///
/// This is the whole trick for unsymbolicated iOS and other native crashes: the
/// offset inside a binary is stable for a given build, so events group correctly per
/// build without a single dSYM on the server. Returns `None` unless the event really
/// is native — a frame with an address, and a debug image that contains it.
fn native_key(event: &Value) -> Option<String> {
    let frame = last_exception(event).and_then(top_frame)?;
    let instruction = parse_addr(frame.get("instruction_addr")?)?;

    let images = event.get("debug_meta")?.get("images")?.as_array()?;
    let mut best: Option<(u64, &Value)> = None;
    for image in images {
        let Some(base) = image.get("image_addr").and_then(parse_addr) else { continue };
        if base > instruction {
            continue;
        }
        // An image with a size only matches when the address is inside it. Without a
        // size, the closest base below the address is the best guess.
        if let Some(size) = image.get("image_size").and_then(parse_addr)
            && instruction >= base.saturating_add(size)
        {
            continue;
        }
        if best.is_none_or(|(previous, _)| base > previous) {
            best = Some((base, image));
        }
    }

    let (base, image) = best?;
    let debug_id = image
        .get("debug_id")
        .or_else(|| image.get("code_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())?;
    Some(format!("native {debug_id}+{:#x}", instruction - base))
}

/// An address in any of the forms SDKs write it: `"0x1f00"`, `"7936"`, or a number.
fn parse_addr(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => {
            let text = text.trim();
            match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
                Some(hex) => u64::from_str_radix(hex, 16).ok(),
                None => text.parse().ok(),
            }
        }
        _ => None,
    }
}

// ---- parameterization ------------------------------------------------------------

/// The one pattern, tried left to right at each position. Order matters: a uuid must
/// win over hex, a url over email and int, a date over ip and int.
static PARAMETERS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?P<uuid>\b[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}\b)",
        r"|(?P<url>\b(?:wss?|https?|ftp)://[^\s/$.?#][^\s]*)",
        r"|(?P<email>\b[\w.!#$%&'*+/=?^`{|}~-]+@[\w-]+(?:\.[\w-]+)+)",
        r"|(?P<date>\b\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?)?\b",
        r"|\b\d{2}:\d{2}:\d{2}(?:\.\d+)?\b)",
        r"|(?P<ip>\b\d{1,3}(?:\.\d{1,3}){3}\b|\b(?:[0-9a-fA-F]{1,4}:){3,7}[0-9a-fA-F]{1,4}\b)",
        r#"|(?P<quoted>"[^"\n]*")"#,
        r"|(?P<hex>\b0[xX][0-9a-fA-F]+\b|\b[0-9a-fA-F]{7,}\b)",
        r"|(?P<int>\b\d+\b)",
    ))
    .expect("the parameterization pattern compiles")
});

/// Replace the parts of a message that change from event to event with placeholders,
/// keeping the first two non-empty lines and capping the result.
///
/// This is what makes `Timeout after 1523ms` and `Timeout after 1611ms` one issue
/// instead of two.
pub fn parameterize(raw: &str) -> String {
    let head: Vec<&str> =
        raw.lines().map(str::trim_end).filter(|line| !line.trim().is_empty()).take(2).collect();
    let head = head.join("\n");

    let replaced = PARAMETERS.replace_all(&head, |caps: &Captures<'_>| {
        for name in ["uuid", "url", "email", "date", "ip", "quoted", "int"] {
            if caps.name(name).is_some() {
                return format!("<{name}>");
            }
        }
        // A bare run of hex digits is only an id if it is actually hexadecimal: `beef`
        // is a word and `1234567` is a number, and neither should read as `<hex>`.
        if let Some(found) = caps.name("hex") {
            let text = found.as_str();
            let hexish = text.starts_with("0x")
                || text.starts_with("0X")
                || (text.bytes().any(|b| b.is_ascii_digit())
                    && text.bytes().any(|b| b.is_ascii_alphabetic()));
            return if hexish { "<hex>".to_owned() } else { text.to_owned() };
        }
        caps[0].to_owned()
    });

    truncate(replaced.trim(), MAX_VALUE)
}

fn first_line(value: &str) -> &str {
    value.lines().next().unwrap_or("").trim()
}

/// Cut to `max` bytes without splitting a character.
fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut end = max;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn exception(ty: &str, value: &str) -> Value {
        json!({
            "event_id": "a".repeat(32),
            "platform": "python",
            "exception": {"values": [{"type": ty, "value": value}]},
        })
    }

    #[test]
    fn messages_that_differ_only_by_an_id_group_together() {
        let one = group(&exception(
            "KeyError",
            "no session 3f0e2c1a-9b7d-4f21-a8c3-5d6e7f809a1b",
        ));
        let two = group(&exception(
            "KeyError",
            "no session 91a2b3c4-d5e6-4718-9a0b-1c2d3e4f5061",
        ));
        assert_eq!(one.key, two.key);
        assert_eq!(one.key, "KeyError: no session <uuid>");
    }

    #[test]
    fn each_parameter_class_becomes_its_placeholder() {
        let cases = [
            ("timeout after 1523 ms", "timeout after <int> ms"),
            ("cannot reach https://example.com/a?b=1", "cannot reach <url>"),
            ("bad address bob.smith@example.com", "bad address <email>"),
            ("refused by 10.0.0.14", "refused by <ip>"),
            ("expired at 2026-08-19T04:05:06Z", "expired at <date>"),
            ("missing key \"user id 7\"", "missing key <quoted>"),
            ("segfault at 0xdeadbeef", "segfault at <hex>"),
            ("blob 3f0e2c1a9b7d4f21a8c35d6e7f809a1b", "blob <hex>"),
        ];
        for (raw, expected) in cases {
            assert_eq!(parameterize(raw), expected, "{raw}");
        }
    }

    #[test]
    fn a_word_made_of_hex_letters_stays_a_word() {
        assert_eq!(parameterize("the coffee decaffeinated"), "the coffee decaffeinated");
    }

    #[test]
    fn only_the_first_two_lines_count() {
        let long = "first line\n\nsecond line\nthird line\nfourth";
        assert_eq!(parameterize(long), "first line\nsecond line");
    }

    #[test]
    fn the_value_is_capped_at_a_kilobyte() {
        assert_eq!(parameterize(&"x".repeat(4000)).len(), MAX_VALUE);
    }

    #[test]
    fn an_explicit_fingerprint_overrides_grouping() {
        let mut event = exception("KeyError", "one");
        event["fingerprint"] = json!(["rpc", "POST", "/foo"]);
        let one = group(&event);

        let mut other = exception("ValueError", "something else entirely");
        other["fingerprint"] = json!(["rpc", "POST", "/foo"]);
        assert_eq!(one.key, group(&other).key);

        // The title still describes the exception, not the fingerprint.
        assert_eq!(one.title, "KeyError: one");
    }

    #[test]
    fn the_default_sentinel_extends_the_computed_key() {
        let mut event = exception("KeyError", "boom 12");
        event["fingerprint"] = json!(["{{ default }}", "tenant-7"]);
        let extended = group(&event);
        assert_eq!(extended.key, format!("KeyError: boom <int>{JOIN}tenant-7"));

        // Extended is not the same issue as unextended.
        assert_ne!(extended.key, group(&exception("KeyError", "boom 12")).key);
    }

    #[test]
    fn the_last_exception_in_the_chain_wins() {
        let event = json!({
            "exception": {"values": [
                {"type": "OSError", "value": "cause"},
                {"type": "RuntimeError", "value": "effect"},
            ]},
        });
        assert_eq!(group(&event).key, "RuntimeError: effect");
    }

    #[test]
    fn a_synthetic_mechanism_groups_by_the_crash_function() {
        let event = json!({
            "exception": {"values": [{
                "type": "SIGSEGV",
                "value": "Segfault",
                "mechanism": {"synthetic": true},
                "stacktrace": {"frames": [
                    {"function": "main", "in_app": true},
                    {"function": "renderFrame", "in_app": true},
                    {"function": "libsystem_stub", "in_app": false},
                ]},
            }]},
        });
        assert_eq!(group(&event).key, "renderFrame: Segfault");
    }

    #[test]
    fn a_native_event_groups_on_the_image_relative_address() {
        let event = json!({
            "exception": {"values": [{
                "type": "EXC_BAD_ACCESS",
                "value": "Attempted to dereference garbage pointer",
                "stacktrace": {"frames": [
                    {"instruction_addr": "0x100004000", "in_app": false},
                    {"instruction_addr": "0x100001120", "in_app": true},
                ]},
            }]},
            "debug_meta": {"images": [
                {"image_addr": "0x100000000", "image_size": 65536, "debug_id": "84a0-b1"},
                {"image_addr": "0x200000000", "image_size": 65536, "debug_id": "other"},
            ]},
        });
        assert_eq!(group(&event).key, "native 84a0-b1+0x1120");
    }

    #[test]
    fn a_native_frame_outside_every_image_falls_back_to_the_exception() {
        let event = json!({
            "exception": {"values": [{
                "type": "EXC_BAD_ACCESS",
                "value": "boom",
                "stacktrace": {"frames": [{"instruction_addr": "0x9", "in_app": true}]},
            }]},
            "debug_meta": {"images": [{"image_addr": "0x100000000", "debug_id": "x"}]},
        });
        assert_eq!(group(&event).key, "EXC_BAD_ACCESS: boom");
    }

    #[test]
    fn an_event_with_no_exception_groups_as_a_log_message() {
        let event = json!({"message": "job 41 finished in 903 ms", "level": "warning"});
        let grouped = group(&event);
        assert_eq!(grouped.key, "Log Message: job <int> finished in <int> ms");
        assert_eq!(grouped.level, "warning");
    }

    #[test]
    fn a_missing_type_reads_as_error_and_the_level_defaults() {
        let event = json!({"exception": {"values": [{"value": "unnamed"}]}});
        let grouped = group(&event);
        assert_eq!(grouped.key, "Error: unnamed");
        assert_eq!(grouped.level, "error");
    }

    #[test]
    fn the_hash_is_a_function_of_the_key_alone() {
        let one = group(&exception("KeyError", "boom"));
        let two = group(&exception("KeyError", "boom"));
        assert_eq!(one.hash, two.hash);
        assert_eq!(one.hash.len(), 64);
        assert_ne!(one.hash, group(&exception("KeyError", "bang")).hash);
    }
}
