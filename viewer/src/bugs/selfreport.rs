//! Dogfooding: nashcode reports its own errors to nashcode.
//!
//! `NASHCODE_BUGS_SELF_DSN` points the viewer's own `tracing` errors at a DSN, which is
//! normally one of its own projects. Unset, none of this exists. There is nothing
//! special about the path an event takes: the Rust SDK posts an envelope to the
//! ordinary door, and the ordinary digest groups it. A tracker that watched itself
//! through a private back channel would be a tracker whose front door is never tested.
//!
//! ## The loop, and the two things that stop it
//!
//! The failure mode is specific and it is not hypothetical. An error raised *inside*
//! the digest is logged; the log is captured; the capture posts an envelope to this
//! same instance; the envelope is queued; the digest picks it up; and if the thing that
//! was broken is still broken, the digest logs again. That is not a slow leak. It is a
//! loop with an HTTP request in it, and it fills a bucket.
//!
//! Two guards, because they fail differently.
//!
//! **By name.** Anything logged from the error-tracking pipeline itself —
//! `nashcode::bugs::*` and `nashcode::web::bugs` — is never reported. Static, obvious,
//! and correct for the code that *is* the pipeline. It misses everything the pipeline
//! calls: a failure inside `git`, `object_store` or SQLite is logged with that module's
//! name, not with ours.
//!
//! **By task.** [`quietly`] marks a task as being inside the pipeline, and the filter
//! reads that mark. The digest worker, both ingest doors, the drainer and the Pushover
//! sender all run inside it, so everything they call is silent too — however deep.
//! This is the guard that catches the transitive case, and it is why the digest worker
//! is wrapped rather than merely named.
//!
//! Neither guard is about volume. Volume is grouping's job: a recurring internal error
//! is one issue with a count on it. The rate cap below is the last line, for the case
//! where a hot path fails in a way neither guard predicted — it bounds the damage to
//! something a person can read rather than something that fills a disk.

use std::sync::atomic::{AtomicU64, Ordering};

use sentry::ClientInitGuard;
use sentry::protocol::Event;
use sentry_tracing::EventFilter;

/// The tag every self-report carries. A person looking at the issue can see at a glance
/// that nashcode is talking about itself rather than about a service it watches.
pub const SELF_TAG: &str = "nashcode.self";

/// The most self-reports that leave in one minute.
///
/// Grouping already turns a repeated error into one issue with a count, so this is not
/// about the issue list. It is about the envelope door: a hot path failing on every
/// request would post one envelope per failure, and sixty a minute is the difference
/// between a bill and a page.
pub const REPORTS_PER_MINUTE: u64 = 60;

/// Module paths whose logs are never reported, because reporting them is what would
/// close the loop.
const NEVER: [&str; 2] = ["nashcode::bugs", "nashcode::web::bugs"];

tokio::task_local! {
    /// Set for the whole life of a task that is inside the error-tracking pipeline.
    static IN_PIPELINE: ();
}

/// Run `future` with self-reporting switched off for everything it does, however deep.
///
/// Wrap the digest, the ingest doors, the drainer and the sender. Anything they call —
/// git, the object store, SQLite — is covered by the same mark, which is the whole
/// point: a name-based guard cannot see through a call into another module.
pub async fn quietly<F: std::future::Future>(future: F) -> F::Output {
    IN_PIPELINE.scope((), future).await
}

/// Is this task inside the pipeline?
pub fn in_pipeline() -> bool {
    IN_PIPELINE.try_with(|()| ()).is_ok()
}

/// Should a log from `target` become a report?
///
/// Pure, so the rule can be checked without a client, a runtime, or a network.
pub fn reportable(target: &str) -> bool {
    !NEVER.iter().any(|banned| target == *banned || target.starts_with(&format!("{banned}::")))
}

/// The rolling one-minute allowance.
///
/// A whole-minute bucket rather than a sliding window: it is one atomic read and one
/// atomic write, it is exact at the only thing that matters — the ceiling — and nothing
/// downstream cares whether the minute started on the minute.
static WINDOW: AtomicU64 = AtomicU64::new(0);
static SENT_IN_WINDOW: AtomicU64 = AtomicU64::new(0);

/// Take one from the allowance, or refuse.
fn within_cap(now_secs: u64) -> bool {
    let minute = now_secs / 60;
    if WINDOW.swap(minute, Ordering::Relaxed) != minute {
        SENT_IN_WINDOW.store(0, Ordering::Relaxed);
    }
    SENT_IN_WINDOW.fetch_add(1, Ordering::Relaxed) < REPORTS_PER_MINUTE
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
}

/// Start the SDK, or do nothing at all.
///
/// `None` back means the feature is off, which is the shipped default and is not a
/// failure. The guard has to be held for the life of the process: dropping it shuts the
/// transport down and the last events never leave.
pub fn init(dsn: Option<&str>, release: Option<String>) -> Option<ClientInitGuard> {
    let dsn = dsn?.trim();
    if dsn.is_empty() {
        return None;
    }
    // Parsed here rather than through `ClientOptions::dsn`, which panics on a string it
    // does not like. A typo in an environment variable must not stop the viewer from
    // starting; it is a line on stderr and a feature that is off.
    let parsed = match dsn.parse::<sentry_types::Dsn>() {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("NASHCODE_BUGS_SELF_DSN is not a DSN ({error}); self-reporting stays off");
            return None;
        }
    };

    let mut options = sentry::ClientOptions::default()
        // The whole reason this exists is to watch production behave badly. There is no
        // second environment to tell it apart from yet, so the field says what it is.
        .environment("nashcode")
        // No user data ever leaves. These events are our own stack traces.
        .send_default_pii(false)
        // Warnings become log lines rather than issues, which is why the `logs` feature
        // is on: the logs page is where the context around an error lives.
        .enable_logs(true)
        .before_send(|mut event: Event<'static>| {
            if !within_cap(now_secs()) {
                return None;
            }
            event.tags.insert(SELF_TAG.to_owned(), "1".to_owned());
            Some(event)
        });
    options.dsn = Some(parsed);
    if let Some(release) = release {
        options = options.release(release);
    }

    let guard = sentry::init(options);
    guard.is_enabled().then_some(guard)
}

/// The `tracing` layer that turns errors into events and warnings into logs.
///
/// Both guards live in the event filter, which `sentry-tracing` calls per event rather
/// than caching per callsite — which matters, because [`in_pipeline`] is a property of
/// the task and not of the line of code.
pub fn layer<S>() -> sentry_tracing::SentryLayer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    sentry_tracing::layer().event_filter(|metadata| {
        if in_pipeline() || !reportable(metadata.target()) {
            return EventFilter::Ignore;
        }
        match *metadata.level() {
            // An error is an issue, and a log line beside it so the logs page has the
            // context the issue page does not.
            tracing::Level::ERROR => EventFilter::Event | EventFilter::Log,
            tracing::Level::WARN | tracing::Level::INFO => EventFilter::Log,
            _ => EventFilter::Ignore,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_the_pipeline_says_about_itself_is_ever_reported() {
        for inside in [
            "nashcode::bugs",
            "nashcode::bugs::digest",
            "nashcode::bugs::pushover",
            "nashcode::bugs::drain",
            "nashcode::web::bugs",
        ] {
            assert!(!reportable(inside), "{inside} would close the loop");
        }
        for outside in [
            "nashcode::ops",
            "nashcode::web::pages",
            "nashcode::git",
            // A near-miss on the prefix is a different module and must still report.
            "nashcode::bugsy",
            "nashcode::web::bugsreport",
        ] {
            assert!(reportable(outside), "{outside} was silenced by mistake");
        }
    }

    #[tokio::test]
    async fn a_task_inside_the_pipeline_stays_quiet_and_its_children_do_not_inherit_it() {
        assert!(!in_pipeline());
        quietly(async {
            assert!(in_pipeline(), "the mark is readable inside the scope");
            // Anything this task calls, however deep, reads the same mark. That is the
            // guard the name check cannot provide.
            async fn deep() -> bool {
                in_pipeline()
            }
            assert!(deep().await);
        })
        .await;
        assert!(!in_pipeline(), "and the mark does not outlive the scope");
    }

    #[test]
    fn the_cap_lets_a_minute_through_and_then_stops() {
        let minute = 1_000_000 * 60;
        let allowed = (0..REPORTS_PER_MINUTE * 2).filter(|_| within_cap(minute)).count();
        assert_eq!(allowed as u64, REPORTS_PER_MINUTE);
        // The next minute starts fresh.
        assert!(within_cap(minute + 60));
    }
}
