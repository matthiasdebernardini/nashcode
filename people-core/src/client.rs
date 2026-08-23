//! Talking to a viewer: push the file, and ask when it last arrived.
//!
//! Feature `client`. The viewer itself never compiles this — it answers these two
//! requests rather than making them — so the server build carries no HTTP client it
//! does not already have.

use std::time::Duration;

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::model::PeopleFile;

/// What `PUT /people` answers: what the viewer now holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PushReply {
    pub people: usize,
    pub projects: usize,
    /// RFC3339, the viewer's clock at the moment of the push.
    pub pushed_at: String,
}

/// Send the file to a viewer.
///
/// The body is the same pretty JSON that is on disk, so what the viewer holds and what
/// the operator edits are one text. A viewer that refuses the file answers 400 with
/// the reason, and the reason is what comes back here.
pub fn push(viewer_base: &str, token: Option<&str>, file: &PeopleFile) -> Result<PushReply, String> {
    let url = format!("{}/people", viewer_base.trim_end_matches('/'));
    let mut request = agent().put(&url).header("content-type", "application/json");
    if let Some(header) = authorization(token) {
        request = request.header("authorization", &header);
    }
    let reply = request
        .send(file.to_pretty_json())
        .map_err(|error| format!("PUT {url} did not go through: {error}"))?;
    let status = reply.status().as_u16();
    let body = reply
        .into_body()
        .read_to_string()
        .map_err(|error| format!("PUT {url} answered nothing readable: {error}"))?;
    if status != 200 {
        return Err(format!("{url} returned HTTP {status}\n{}", reason(&body)));
    }
    serde_json::from_str(&body)
        .map_err(|error| format!("{url} answered something that is not a push receipt: {error}"))
}

/// When the viewer's copy last arrived, from `GET /brain`. `None` before any push.
pub fn pushed_at(viewer_base: &str) -> Result<Option<String>, String> {
    let url = format!("{}/brain", viewer_base.trim_end_matches('/'));
    let reply = agent()
        .get(&url)
        .header("accept", "application/json")
        .call()
        .map_err(|error| format!("GET {url} did not go through: {error}"))?;
    let status = reply.status().as_u16();
    let body = reply
        .into_body()
        .read_to_string()
        .map_err(|error| format!("GET {url} answered nothing readable: {error}"))?;
    if status != 200 {
        return Err(format!("{url} returned HTTP {status}\n{}", reason(&body)));
    }
    let brain: serde_json::Value = serde_json::from_str(&body)
        .map_err(|error| format!("{url} answered something that is not the brain: {error}"))?;
    Ok(brain
        .get("people")
        .and_then(|people| people.get("pushed_at"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned))
}

/// One minute is generous for two small requests and short enough that a viewer that
/// is not there fails while somebody is still watching.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(60)))
        // Read statuses here: a 400 carries the reason the file was refused.
        .http_status_as_error(false)
        .user_agent(concat!("nashcode-people/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

/// The same basic credential the rest of nashcode sends: user `x`, the token as the
/// password. The viewer authenticates through Tailscale's identity headers today, so
/// the header is only attached when a caller hands one over.
fn authorization(token: Option<&str>) -> Option<String> {
    let token = token.map(str::trim).filter(|token| !token.is_empty())?;
    let raw = format!("x:{token}");
    Some(format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw)))
}

/// The `error` field of a JSON answer, else the body as it came.
fn reason(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value.get("error").and_then(serde_json::Value::as_str).map(str::to_owned))
        .unwrap_or_else(|| body.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_travels_as_basic_auth_and_an_empty_one_travels_not_at_all() {
        assert_eq!(authorization(Some("sekrit")).as_deref(), Some("Basic eDpzZWtyaXQ="));
        assert_eq!(authorization(Some("  ")), None);
        assert_eq!(authorization(None), None);
    }

    #[test]
    fn the_reason_is_the_error_field_when_there_is_one() {
        assert_eq!(reason(r#"{"error":"no people file"}"#), "no people file");
        assert_eq!(reason("  plain words  "), "plain words");
    }
}
