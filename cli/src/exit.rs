//! Which failure class an error belongs to, decided where the error is raised.
//!
//! An agent branches on the exit code, so the class is as much a public
//! interface as any output field. It used to be recovered by matching substrings
//! against the finished message — which meant an upstream response body, or a
//! human's review notes on their way into an error, could pick the class. A
//! reviewer's feedback containing the words "does not exist" turned a viewer 500
//! into exit 3.
//!
//! So the class travels with the error instead. [`Classed`] is a message that
//! knows its own class and keeps the error it wrapped as its source, so it sits
//! in the `anyhow` chain like any other context and prints as exactly its
//! message: the wording every command already chose survives untouched.
//! [`class_of`] reads it back out. Nothing downstream reads prose, and appending
//! anything to a message — an upstream body, a person's sentence — cannot change
//! what the process exits with.

use std::error::Error as StdError;
use std::fmt;

/// The failure classes an agent branches on. The values are agcli's typed exit
/// codes; the names are what to do about them.
///
/// There is no `Error` variant. An error that claims no class is exactly that —
/// unclassified — and becomes agcli's generic `ERROR` at the handler boundary.
/// Making that a variant would invite claiming it, and a claim of "generic" is
/// indistinguishable from having forgotten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// The invocation was wrong. Nothing left this machine.
    Usage,
    /// A profile, repository, viewer, or token that has to exist does not.
    NotFound,
    /// A credential was refused: 401, 403, a rejected probe.
    Auth,
    /// dgit, the viewer, or a remote script failed. Retry, or run `doctor`.
    Api,
}

impl Class {
    /// agcli's exit code for this class.
    pub fn exit_code(self) -> i32 {
        match self {
            Class::Usage => agcli::ExitCode::USAGE,
            Class::NotFound => agcli::ExitCode::NOT_FOUND,
            Class::Auth => agcli::ExitCode::AUTH,
            Class::Api => agcli::ExitCode::API,
        }
    }

    /// The machine-readable code in the error envelope.
    pub fn code(self) -> &'static str {
        match self {
            Class::Usage => "USAGE",
            Class::NotFound => "NOT_FOUND",
            Class::Auth => "AUTH",
            Class::Api => "API",
        }
    }
}

/// A message that carries its own failure class.
///
/// `Display` is the message and nothing else. The error it wrapped, if any, is
/// its `source`, so `{err:#}` reads exactly as `.context(message)` did.
#[derive(Debug)]
pub struct Classed {
    pub class: Class,
    pub message: String,
    source: Option<Box<dyn StdError + Send + Sync + 'static>>,
}

impl fmt::Display for Classed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl StdError for Classed {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.source.as_deref().map(|e| e as &(dyn StdError + 'static))
    }
}

/// Build a classified error, for a site that would otherwise `bail!`.
pub fn classed(class: Class, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(Classed {
        class,
        message: message.into(),
        source: None,
    })
}

/// The class of an error, or `None` when nothing on the way up claimed one.
///
/// The outermost claim wins: a caller that knows better than the layer it
/// wrapped says so by classifying again.
pub fn class_of(error: &anyhow::Error) -> Option<Class> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<Classed>())
        .map(|c| c.class)
}

/// Attach a class and a context message to a `Result`, the way `.context` does.
pub trait Classify<T> {
    /// Wrap the error with `message`, and claim `class` for it.
    fn classify(self, class: Class, message: impl Into<String>) -> anyhow::Result<T>;

    /// The same, for a message only worth building on the failing path.
    fn classify_with<S, F>(self, class: Class, message: F) -> anyhow::Result<T>
    where
        S: Into<String>,
        F: FnOnce() -> S;
}

impl<T, E> Classify<T> for Result<T, E>
where
    E: Into<anyhow::Error>,
{
    fn classify(self, class: Class, message: impl Into<String>) -> anyhow::Result<T> {
        let message = message.into();
        self.classify_with(class, || message)
    }

    fn classify_with<S, F>(self, class: Class, message: F) -> anyhow::Result<T>
    where
        S: Into<String>,
        F: FnOnce() -> S,
    {
        self.map_err(|error| {
            anyhow::Error::new(Classed {
                class,
                message: message().into(),
                source: Some(error.into().into()),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_class_survives_being_wrapped_in_more_context() {
        let e = classed(Class::Auth, "the token was rejected")
            .context("push to origin")
            .context("nashcode init");
        assert_eq!(class_of(&e), Some(Class::Auth));
        // And the wording is still every layer's own.
        assert_eq!(
            format!("{e:#}"),
            "nashcode init: push to origin: the token was rejected"
        );
    }

    #[test]
    fn classify_reads_like_context_and_keeps_the_cause() {
        let io = std::io::Error::other("connection refused");
        let e = Result::<(), _>::Err(io)
            .classify(Class::Api, "POST https://box/widget/gc")
            .unwrap_err();
        assert_eq!(class_of(&e), Some(Class::Api));
        assert_eq!(
            format!("{e:#}"),
            "POST https://box/widget/gc: connection refused"
        );
    }

    #[test]
    fn an_unclassified_error_claims_nothing() {
        let e = anyhow::anyhow!("something went wrong");
        assert_eq!(class_of(&e), None);
        // Including one wrapped in ordinary context.
        assert_eq!(class_of(&e.context("while doing a thing")), None);
    }

    #[test]
    fn appending_upstream_or_human_text_cannot_change_the_class() {
        // The exact shape that used to misfire: an upstream failure whose body
        // carries a person's review notes.
        let body = "does not exist. --provider is required. the token was rejected.";
        let e = classed(Class::Api, format!("https://v returned HTTP 500\n{body}"));
        assert_eq!(class_of(&e), Some(Class::Api));
        assert!(e.to_string().contains("does not exist"));
    }

    #[test]
    fn the_outermost_claim_wins() {
        // A caller that knows the layer below misjudged says so by re-claiming.
        let inner = Result::<(), _>::Err(std::io::Error::other("refused"))
            .classify(Class::Api, "PUT /x/config")
            .unwrap_err();
        let outer = Result::<(), _>::Err(inner)
            .classify(Class::Auth, "the profile's token was rejected")
            .unwrap_err();
        assert_eq!(class_of(&outer), Some(Class::Auth));
    }

    #[test]
    fn every_class_maps_to_its_agcli_exit_code() {
        assert_eq!(Class::Usage.exit_code(), 2);
        assert_eq!(Class::NotFound.exit_code(), 3);
        assert_eq!(Class::Auth.exit_code(), 4);
        assert_eq!(Class::Api.exit_code(), 5);
    }
}
