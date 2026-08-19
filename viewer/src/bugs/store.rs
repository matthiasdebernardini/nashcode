//! The object store that holds the payloads.
//!
//! `NASHCODE_BUGS_BUCKET` names it. Two forms:
//!
//! - `s3://name` — an S3-compatible bucket, credentials from the AWS environment
//!   variables only, `S3_ENDPOINT` overriding the endpoint for MinIO or Garage.
//! - `file:///path` — a directory. Not a fallback for production; it exists so the
//!   whole ingest path can be exercised against a real object store in a tempdir,
//!   with no S3 and no mock.
//!
//! Anything else is a configuration error: the feature turns itself off and says so.

use std::sync::Arc;

use object_store::ObjectStore;
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::path::Path as ObjectPath;

/// Build the store named by `bucket`. `Err` carries the line an operator needs.
pub fn open(bucket: &str, endpoint: Option<&str>) -> Result<Arc<dyn ObjectStore>, String> {
    if let Some(name) = bucket.strip_prefix("s3://") {
        let name = name.trim_matches('/');
        if name.is_empty() {
            return Err("NASHCODE_BUGS_BUCKET is `s3://` with no bucket name".to_owned());
        }
        let mut builder = AmazonS3Builder::from_env().with_bucket_name(name);
        if let Some(endpoint) = endpoint {
            // A MinIO or Garage endpoint is usually plain HTTP on the same host.
            builder = builder.with_endpoint(endpoint).with_allow_http(true);
        }
        let store = builder.build().map_err(|error| format!("cannot open {bucket}: {error}"))?;
        return Ok(Arc::new(store));
    }

    if let Some(path) = bucket.strip_prefix("file://") {
        if path.is_empty() {
            return Err("NASHCODE_BUGS_BUCKET is `file://` with no path".to_owned());
        }
        // The prefix has to exist before `LocalFileSystem` will take it.
        std::fs::create_dir_all(path)
            .map_err(|error| format!("cannot create {path} for {bucket}: {error}"))?;
        let store = LocalFileSystem::new_with_prefix(path)
            .map_err(|error| format!("cannot open {bucket}: {error}"))?;
        return Ok(Arc::new(store));
    }

    Err(format!(
        "NASHCODE_BUGS_BUCKET must be `s3://name` or `file:///path`, not `{bucket}`"
    ))
}

/// Where a project's raw envelopes live. One object per accepted request, named by
/// arrival time so a listing reads in order.
pub fn envelope_key(project_id: i64, received_at: &str, nonce: &str) -> ObjectPath {
    ObjectPath::from(format!("projects/{project_id}/envelopes/{received_at}-{nonce}.envelope"))
}

/// Where a project's event payloads live. The detail view reads this object; the
/// SQLite row only points at it.
pub fn event_key(project_id: i64, event_id: &str) -> ObjectPath {
    ObjectPath::from(format!("projects/{project_id}/events/{event_id}.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bucket_that_is_not_a_url_is_a_configuration_error() {
        for bad in ["mybucket", "s3://", "file://", "gs://thing"] {
            assert!(open(bad, None).is_err(), "{bad} must not open");
        }
    }

    #[test]
    fn a_file_bucket_creates_its_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bugs");
        let url = format!("file://{}", path.display());
        assert!(open(&url, None).is_ok());
        assert!(path.is_dir());
    }
}
