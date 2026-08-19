//! The envelope splitter.
//!
//! `sentry-types` 0.49 was the first choice and does not fit. Its `EnvelopeItemType`
//! is a closed serde enum with no unknown variant, so one `{"type":"profile"}` item
//! makes `Envelope::from_slice` fail for the whole envelope — the exact behaviour the
//! protocol calls the number-one thing a server must not do. It also parses `event`
//! items into typed structs, which loses the raw bytes the bucket is supposed to
//! hold. So the splitter is here, and `sentry-types` keeps the jobs it is good at:
//! `Dsn` and `Auth` parsing.
//!
//! Grammar (develop.sentry.dev/sdk/foundations/envelopes):
//!
//! ```text
//! Envelope = Headers { "\n" Item } [ "\n" ] ;
//! Item     = Headers "\n" Payload ;
//! ```
//!
//! Newlines are `\n` only. An item header without `length` runs its payload to the
//! next newline. Unknown item types are kept, never rejected.

/// One item, with its payload still as raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item<'a> {
    /// The `type` header, verbatim. Unknown values are normal.
    pub ty: String,
    pub headers: serde_json::Value,
    pub payload: &'a [u8],
}

/// A split envelope. Payloads borrow the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope<'a> {
    pub headers: serde_json::Value,
    pub items: Vec<Item<'a>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitError {
    /// The envelope header line is missing or is not a JSON object.
    Header(String),
    /// An item header line is missing or is not a JSON object.
    ItemHeader(String),
    /// A `length` ran past the end of the envelope.
    Truncated,
    /// One item is bigger than the per-item cap.
    ItemTooLarge { ty: String, len: usize },
}

impl std::fmt::Display for SplitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Header(detail) => write!(f, "bad envelope header: {detail}"),
            Self::ItemHeader(detail) => write!(f, "bad item header: {detail}"),
            Self::Truncated => f.write_str("envelope ends inside an item payload"),
            Self::ItemTooLarge { ty, len } => write!(f, "{ty} item is {len} bytes"),
        }
    }
}

impl std::error::Error for SplitError {}

/// Relay's per-item cap, and ours.
pub const MAX_ITEM: usize = 1024 * 1024;

impl<'a> Envelope<'a> {
    /// The `event_id` the response must echo: the envelope header wins over the
    /// payload's own, per the spec.
    pub fn event_id(&self) -> Option<String> {
        self.headers
            .get("event_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                self.items
                    .iter()
                    .find(|item| item.ty == "event")
                    .and_then(|item| serde_json::from_slice::<serde_json::Value>(item.payload).ok())
                    .and_then(|event| {
                        event.get("event_id").and_then(serde_json::Value::as_str).map(str::to_owned)
                    })
            })
    }

    /// The `dsn` envelope header, which some SDKs use instead of an auth header.
    pub fn dsn(&self) -> Option<&str> {
        self.headers.get("dsn").and_then(serde_json::Value::as_str)
    }
}

/// Split `bytes` into headers and items. Payloads borrow `bytes`.
pub fn split(bytes: &[u8]) -> Result<Envelope<'_>, SplitError> {
    let (headers, mut offset) = read_line_json(bytes, 0).map_err(SplitError::Header)?;
    if !headers.is_object() {
        return Err(SplitError::Header("not a JSON object".to_owned()));
    }

    let mut items = Vec::new();
    while offset < bytes.len() {
        // A trailing newline, or trailing blank lines, end the envelope.
        if bytes[offset] == b'\n' {
            offset += 1;
            continue;
        }

        let (item_headers, after_header) =
            read_line_json(bytes, offset).map_err(SplitError::ItemHeader)?;
        if !item_headers.is_object() {
            return Err(SplitError::ItemHeader("not a JSON object".to_owned()));
        }
        // A missing `type` is malformed; an unrecognised one is not.
        let Some(ty) = item_headers.get("type").and_then(serde_json::Value::as_str) else {
            return Err(SplitError::ItemHeader("no type".to_owned()));
        };
        let ty = ty.to_owned();

        let declared = item_headers.get("length").and_then(serde_json::Value::as_u64);
        let (payload, after_payload) = match declared {
            Some(length) => {
                let length = usize::try_from(length).map_err(|_| SplitError::Truncated)?;
                if length > MAX_ITEM {
                    return Err(SplitError::ItemTooLarge { ty, len: length });
                }
                let end = after_header.checked_add(length).ok_or(SplitError::Truncated)?;
                if end > bytes.len() {
                    return Err(SplitError::Truncated);
                }
                // The payload is terminated by a newline or by the end of the buffer.
                let next = if bytes.get(end) == Some(&b'\n') { end + 1 } else { end };
                (&bytes[after_header..end], next)
            }
            None => {
                let end = bytes[after_header..]
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .map_or(bytes.len(), |found| after_header + found);
                if end - after_header > MAX_ITEM {
                    return Err(SplitError::ItemTooLarge { ty, len: end - after_header });
                }
                (&bytes[after_header..end], (end + 1).min(bytes.len()))
            }
        };

        items.push(Item { ty, headers: item_headers, payload });
        offset = after_payload;
    }

    Ok(Envelope { headers, items })
}

/// Read one `\n`-terminated JSON line starting at `from`; return it and the offset
/// just past the newline.
fn read_line_json(bytes: &[u8], from: usize) -> Result<(serde_json::Value, usize), String> {
    if from >= bytes.len() {
        return Err("empty".to_owned());
    }
    let end = bytes[from..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |found| from + found);
    let line = &bytes[from..end];
    let value: serde_json::Value =
        serde_json::from_slice(line).map_err(|error| error.to_string())?;
    Ok((value, (end + 1).min(bytes.len())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worked_example_from_the_spec_splits() {
        // Straight out of develop.sentry.dev, byte for byte, BOM and CRLF included.
        let mut raw = Vec::new();
        raw.extend_from_slice(br#"{"event_id":"9ec79c33ec9942ab8353589fcb2e04dc","dsn":"https://e12d836b15bb49d7bbf99e64295d995b:@sentry.io/42"}"#);
        raw.push(b'\n');
        raw.extend_from_slice(
            br#"{"type":"attachment","length":10,"content_type":"text/plain","filename":"hello.txt"}"#,
        );
        raw.push(b'\n');
        raw.extend_from_slice(b"\xef\xbb\xbfHello\r\n");
        raw.push(b'\n');
        raw.extend_from_slice(br#"{"type":"event","length":41,"content_type":"application/json"}"#);
        raw.push(b'\n');
        raw.extend_from_slice(br#"{"message":"hello world","level":"error"}"#);
        raw.push(b'\n');

        let envelope = split(&raw).expect("splits");
        assert_eq!(envelope.items.len(), 2);
        assert_eq!(envelope.items[0].ty, "attachment");
        assert_eq!(envelope.items[0].payload, b"\xef\xbb\xbfHello\r\n");
        assert_eq!(envelope.items[1].ty, "event");
        assert_eq!(envelope.items[1].payload, br#"{"message":"hello world","level":"error"}"#);
        assert_eq!(envelope.event_id().as_deref(), Some("9ec79c33ec9942ab8353589fcb2e04dc"));
    }

    #[test]
    fn an_item_without_a_length_runs_to_the_newline() {
        let raw = b"{}\n{\"type\":\"session\"}\n{\"sid\":\"x\"}\n";
        let envelope = split(raw).expect("splits");
        assert_eq!(envelope.items.len(), 1);
        assert_eq!(envelope.items[0].payload, br#"{"sid":"x"}"#);
    }

    #[test]
    fn the_last_item_may_end_without_a_newline() {
        let raw = b"{}\n{\"type\":\"event\"}\n{\"event_id\":\"a\"}";
        let envelope = split(raw).expect("splits");
        assert_eq!(envelope.items[0].payload, br#"{"event_id":"a"}"#);
    }

    #[test]
    fn an_unknown_item_type_is_kept_not_rejected() {
        let raw = b"{}\n{\"type\":\"profile_chunk\",\"length\":2}\nhi\n{\"type\":\"event\"}\n{}\n";
        let envelope = split(raw).expect("unknown types never fail the split");
        assert_eq!(envelope.items.len(), 2);
        assert_eq!(envelope.items[0].ty, "profile_chunk");
        assert_eq!(envelope.items[1].ty, "event");
    }

    #[test]
    fn a_headers_only_envelope_is_valid_and_empty() {
        assert!(split(b"{}").expect("splits").items.is_empty());
        assert!(split(b"{}\n").expect("splits").items.is_empty());
    }

    #[test]
    fn a_length_past_the_end_is_truncation() {
        let raw = b"{}\n{\"type\":\"event\",\"length\":900}\nshort";
        assert_eq!(split(raw), Err(SplitError::Truncated));
    }

    #[test]
    fn an_item_over_the_cap_is_named_in_the_error() {
        let raw = format!("{{}}\n{{\"type\":\"event\",\"length\":{}}}\n", MAX_ITEM + 1);
        assert!(matches!(
            split(raw.as_bytes()),
            Err(SplitError::ItemTooLarge { len, .. }) if len == MAX_ITEM + 1
        ));
    }

    #[test]
    fn a_missing_or_unparseable_header_is_an_error() {
        assert!(matches!(split(b""), Err(SplitError::Header(_))));
        assert!(matches!(split(b"not json\n"), Err(SplitError::Header(_))));
        assert!(matches!(split(b"[]\n"), Err(SplitError::Header(_))));
        assert!(matches!(split(b"{}\n{\"length\":1}\nx"), Err(SplitError::ItemHeader(_))));
    }

    #[test]
    fn the_payload_event_id_is_the_fallback_when_the_header_has_none() {
        let raw = b"{}\n{\"type\":\"event\"}\n{\"event_id\":\"deadbeef\"}\n";
        assert_eq!(split(raw).unwrap().event_id().as_deref(), Some("deadbeef"));
    }
}
