//! Embeddings: one trait, one real implementation, and one honest way to be absent.
//!
//! The real implementation is [`fastembed`] running ONNX in this process. Two things
//! about it are load-bearing:
//!
//! * **The ONNX Runtime is opened, not linked.** `ort`'s `load-dynamic` feature means
//!   the release binary carries no native archive, so it cross-compiles with the rest
//!   of the tree; the shared library is found at run time or the feature reports
//!   itself unavailable. A box without `libonnxruntime` still serves every other
//!   endpoint.
//! * **Only the index path may load the model.** Loading downloads roughly a third of
//!   a gigabyte on first use. That is fine on the CI queue, where the work is already
//!   asynchronous, and unacceptable on a request path, so [`Embeddings::ready`] is
//!   what `/code/similar` checks and a model that is not loaded yet is an answer, not
//!   a wait.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a failed load is believed before it is tried again.
///
/// A missing shared library will still be missing in a second; a download that failed
/// on a flaky link might not be. Retrying every index run would put a several-hundred
/// megabyte download in front of every merge, and never retrying would need a restart
/// to recover from one bad minute.
const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// Why an embedder could not do its job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmbedError {
    /// The build has no embedding support at all (`--no-default-features`).
    NotBuilt,
    /// The runtime or the model could not be loaded, with the reason.
    Unavailable(String),
    /// Loading worked; embedding this batch did not.
    Failed(String),
}

impl std::fmt::Display for EmbedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotBuilt => f.write_str(
                "this build has no embedding support: rebuild with the `embeddings` feature",
            ),
            Self::Unavailable(why) => write!(f, "embeddings are unavailable: {why}"),
            Self::Failed(why) => write!(f, "embedding failed: {why}"),
        }
    }
}

impl std::error::Error for EmbedError {}

/// Anything that turns text into vectors. A trait so the pipeline can be tested
/// without an ONNX runtime, a model download, or a network.
pub trait Embedder: Send + Sync {
    /// The model's identifier, stored against every vector it produces so a model
    /// change is visible in the data rather than silently mixed into it.
    fn model(&self) -> &str;

    /// Embed a batch. The result has one vector per input, in order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError>;
}

/// The model this deployment uses, from `NASHCODE_EMBED_MODEL`.
///
/// The default is pinned in NOTES.md: `JinaEmbeddingsV2BaseCode`, 768 dimensions, the
/// one code-trained encoder fastembed ships.
pub const DEFAULT_MODEL: &str = "JinaEmbeddingsV2BaseCode";

pub fn configured_model() -> String {
    std::env::var("NASHCODE_EMBED_MODEL")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| DEFAULT_MODEL.to_owned())
}

/// The process-wide embedder slot.
///
/// Cheap to clone and shared through the app context. It starts empty and is filled
/// by the first index run; every failure to fill it is remembered so the query path
/// can say *why* rather than "no".
#[derive(Clone, Default)]
pub struct Embeddings {
    state: Arc<Mutex<Slot>>,
}

#[derive(Default)]
struct Slot {
    embedder: Option<Arc<dyn Embedder>>,
    last_error: Option<String>,
    last_attempt: Option<Instant>,
}

impl std::fmt::Debug for Embeddings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Embeddings")
    }
}

impl Embeddings {
    pub fn new() -> Self {
        Self::default()
    }

    /// A slot pre-filled with a specific embedder. Tests use this; so could a future
    /// deployment that wants a different backend.
    pub fn with(embedder: Arc<dyn Embedder>) -> Self {
        Self {
            state: Arc::new(Mutex::new(Slot {
                embedder: Some(embedder),
                ..Default::default()
            })),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The loaded embedder, if there is one. Never loads anything.
    pub fn ready(&self) -> Option<Arc<dyn Embedder>> {
        self.lock().embedder.clone()
    }

    /// Why the slot is empty, in words a person can act on.
    pub fn why_not(&self) -> String {
        let slot = self.lock();
        match (&slot.embedder, &slot.last_error) {
            (Some(_), _) => String::new(),
            (None, Some(error)) => error.clone(),
            (None, None) => {
                "the model is not loaded yet; it loads on the first index run \
                 (POST /:repo/code/index)"
                    .to_owned()
            }
        }
    }

    /// Load the model if it is not loaded already, and hand it back.
    ///
    /// Blocking and slow the first time — it downloads the model. Only the index path
    /// calls this, and only from a blocking task.
    ///
    /// A load that fails is remembered for [`RETRY_AFTER`], and a load that *panics*
    /// is caught: `ort` aborts the thread when the ONNX Runtime shared library is not
    /// on the box, and a missing dependency must be a message, not a crash.
    pub fn load(&self) -> Result<Arc<dyn Embedder>, EmbedError> {
        self.load_with(build_embedder)
    }

    /// [`load`](Self::load) against a specific builder, so the failure paths can be
    /// tested without a runtime, a model, or a network.
    pub fn load_with(
        &self,
        build: impl FnOnce() -> Result<Arc<dyn Embedder>, EmbedError>,
    ) -> Result<Arc<dyn Embedder>, EmbedError> {
        if let Some(embedder) = self.ready() {
            return Ok(embedder);
        }
        {
            let slot = self.lock();
            if let (Some(error), Some(at)) = (&slot.last_error, slot.last_attempt)
                && at.elapsed() < RETRY_AFTER
            {
                return Err(EmbedError::Unavailable(error.clone()));
            }
        }

        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(build))
            .unwrap_or_else(|payload| {
                Err(EmbedError::Unavailable(format!(
                    "the ONNX runtime could not start: {}",
                    panic_message(&payload)
                )))
            });

        let mut slot = self.lock();
        slot.last_attempt = Some(Instant::now());
        match built {
            Ok(embedder) => {
                slot.last_error = None;
                slot.embedder = Some(embedder.clone());
                Ok(embedder)
            }
            Err(error) => {
                let message = error.to_string();
                tracing::warn!(%message, "embeddings unavailable; text search is unaffected");
                slot.last_error = Some(message);
                Err(error)
            }
        }
    }
}

/// The text out of a caught panic, whichever of the two shapes it took.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "no reason given".to_owned())
}

#[cfg(feature = "embeddings")]
fn build_embedder() -> Result<Arc<dyn Embedder>, EmbedError> {
    real::FastEmbedder::load().map(|e| Arc::new(e) as Arc<dyn Embedder>)
}

#[cfg(not(feature = "embeddings"))]
fn build_embedder() -> Result<Arc<dyn Embedder>, EmbedError> {
    Err(EmbedError::NotBuilt)
}

#[cfg(feature = "embeddings")]
mod real {
    use std::sync::Mutex;

    use super::{EmbedError, Embedder, configured_model};

    /// fastembed's `TextEmbedding`, wrapped so the pipeline sees only [`Embedder`].
    ///
    /// The mutex is fastembed's requirement, not ours: `embed` takes `&mut self`.
    /// Indexing is already serialised behind the CI queue, so it costs nothing there,
    /// and a query embeds one short string.
    pub struct FastEmbedder {
        inner: Mutex<fastembed::TextEmbedding>,
        model: String,
    }

    impl FastEmbedder {
        pub fn load() -> Result<Self, EmbedError> {
            let name = configured_model();
            let model = resolve(&name)?;
            let options = fastembed::InitOptions::new(model).with_show_download_progress(false);
            let inner = fastembed::TextEmbedding::try_new(options)
                .map_err(|error| EmbedError::Unavailable(error.to_string()))?;
            Ok(Self { inner: Mutex::new(inner), model: name })
        }
    }

    /// Match `NASHCODE_EMBED_MODEL` against fastembed's own catalogue, by the model
    /// code it publishes and by the enum's own name. An unknown name is an error with
    /// the list in it, not a silent fall back to something else.
    fn resolve(name: &str) -> Result<fastembed::EmbeddingModel, EmbedError> {
        let wanted = name.trim().to_ascii_lowercase();
        let mut known: Vec<String> = Vec::new();
        for info in fastembed::TextEmbedding::list_supported_models() {
            let code = info.model_code.to_ascii_lowercase();
            let debug = format!("{:?}", info.model).to_ascii_lowercase();
            if wanted == code || wanted == debug || code.ends_with(&format!("/{wanted}")) {
                return Ok(info.model);
            }
            known.push(format!("{:?}", info.model));
        }
        known.sort();
        Err(EmbedError::Unavailable(format!(
            "no embedding model named {name}; known models: {}",
            known.join(", ")
        )))
    }

    impl Embedder for FastEmbedder {
        fn model(&self) -> &str {
            &self.model
        }

        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            if texts.is_empty() {
                return Ok(Vec::new());
            }
            let mut inner =
                self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            inner
                .embed(texts, None)
                .map_err(|error| EmbedError::Failed(error.to_string()))
        }
    }
}

/// Cosine similarity of two vectors. Mismatched lengths score zero rather than
/// panicking: a model change leaves old vectors in the table, and they must simply
/// stop matching.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f32;
    let mut norm_a = 0.0f32;
    let mut norm_b = 0.0f32;
    for (x, y) in a.iter().zip(b) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

#[cfg(test)]
pub(crate) mod testing {
    use super::{EmbedError, Embedder};

    /// A deterministic stand-in: a bag-of-words vector over a fixed vocabulary.
    ///
    /// Similar text scores high, unrelated text scores low, and nothing is downloaded,
    /// which is what lets the whole pipeline be tested end to end.
    pub struct WordBag {
        vocabulary: Vec<String>,
    }

    impl WordBag {
        pub fn new(vocabulary: &[&str]) -> Self {
            Self { vocabulary: vocabulary.iter().map(|w| (*w).to_owned()).collect() }
        }
    }

    impl Embedder for WordBag {
        fn model(&self) -> &str {
            "word-bag-test"
        }

        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
            Ok(texts
                .iter()
                .map(|text| {
                    let lower = text.to_ascii_lowercase();
                    self.vocabulary
                        .iter()
                        .map(|word| lower.matches(word.as_str()).count() as f32)
                        .collect()
                })
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_ranks_identical_above_orthogonal() {
        let a = vec![1.0, 0.0, 1.0];
        let same = vec![2.0, 0.0, 2.0];
        let orthogonal = vec![0.0, 1.0, 0.0];
        assert!((cosine(&a, &same) - 1.0).abs() < 1e-6);
        assert_eq!(cosine(&a, &orthogonal), 0.0);
    }

    #[test]
    fn a_length_mismatch_scores_zero_rather_than_panicking() {
        assert_eq!(cosine(&[1.0, 2.0], &[1.0, 2.0, 3.0]), 0.0);
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn an_empty_slot_explains_itself() {
        let slot = Embeddings::new();
        assert!(slot.ready().is_none());
        assert!(slot.why_not().contains("index run"));
    }

    #[test]
    fn a_filled_slot_reports_ready_and_says_nothing() {
        let slot = Embeddings::with(Arc::new(testing::WordBag::new(&["retry"])));
        assert!(slot.ready().is_some());
        assert_eq!(slot.why_not(), "");
    }

    #[test]
    fn a_build_without_the_feature_says_so_instead_of_pretending() {
        // `build_embedder` is the real thing on a feature build and a stub without it;
        // the message the stub returns is what `/code/similar` reports.
        assert!(
            EmbedError::NotBuilt
                .to_string()
                .contains("rebuild with the `embeddings` feature")
        );
        #[cfg(not(feature = "embeddings"))]
        {
            let Err(error) = Embeddings::new().load() else {
                panic!("a build without the feature must have nothing to load");
            };
            assert_eq!(error, EmbedError::NotBuilt);
        }
    }

    #[test]
    fn a_builder_that_panics_becomes_a_message_not_a_crash() {
        // `ort` does exactly this when libonnxruntime is missing, which is the state
        // of every box before the runtime is installed.
        let slot = Embeddings::new();
        let Err(error) = slot.load_with(|| panic!("Failed to load ONNX Runtime dylib")) else {
            panic!("a panicking builder must yield no embedder");
        };
        assert!(error.to_string().contains("Failed to load ONNX Runtime dylib"), "{error}");
        assert!(slot.ready().is_none());
        assert!(slot.why_not().contains("ONNX"), "the reason is remembered");
    }

    #[test]
    fn a_failed_load_is_believed_rather_than_retried_on_every_run() {
        let slot = Embeddings::new();
        let _ = slot.load_with(|| Err(EmbedError::Unavailable("no runtime".to_owned())));

        // A second attempt inside the retry window never reaches the builder, so a
        // missing dependency cannot put a download in front of every merge.
        let Err(error) = slot.load_with(|| panic!("the builder must not run again")) else {
            panic!("the slot must still be empty");
        };
        assert!(error.to_string().contains("no runtime"), "{error}");
    }

    #[test]
    fn a_load_that_succeeds_fills_the_slot_and_clears_the_reason() {
        let slot = Embeddings::new();
        let _ = slot.load_with(|| Err(EmbedError::Unavailable("not yet".to_owned())));
        assert!(slot.ready().is_none());

        // Force past the retry window by handing the slot an embedder directly, which
        // is the same state a later successful load leaves it in.
        let ready = Embeddings::with(Arc::new(testing::WordBag::new(&["retry"])));
        assert!(ready.ready().is_some());
        assert_eq!(ready.why_not(), "");
    }

    #[test]
    fn the_default_model_is_the_pinned_one() {
        // Unless the environment overrides it, which is the documented knob.
        if std::env::var("NASHCODE_EMBED_MODEL").is_err() {
            assert_eq!(configured_model(), DEFAULT_MODEL);
        }
    }
}
