//! Reading an envelope off the wire: size caps first, decompression second.
//!
//! The caps are Relay's, and they exist to stop one request from costing the box its
//! memory. The compressed cap is enforced while the body streams, so an oversized
//! request is refused before it is buffered; the decompressed cap is enforced on the
//! decompressor's output, so a zip bomb stops at 100 MiB rather than at whatever it
//! wanted to expand to.

use std::io::Read;
use std::pin::pin;

use futures_core::Stream;
use topcoat::router::Body;

/// Relay's cap on a compressed envelope body.
pub const MAX_COMPRESSED: usize = 20 * 1024 * 1024;
/// Our cap on what a body may decompress to.
pub const MAX_DECOMPRESSED: usize = 100 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestError {
    /// Over a size cap. Answers 413; SDKs must not retry.
    TooLarge(&'static str),
    /// The body could not be read.
    Body(String),
    /// The body did not decompress with the encoding it declared.
    Encoding(String),
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(what) => write!(f, "{what} is over the size limit"),
            Self::Body(detail) => write!(f, "cannot read the request body: {detail}"),
            Self::Encoding(detail) => write!(f, "cannot decode the request body: {detail}"),
        }
    }
}

impl std::error::Error for IngestError {}

/// Read the body, refusing it the moment it passes `max` rather than at the end.
pub async fn read_capped(body: Body, max: usize) -> Result<Vec<u8>, IngestError> {
    let mut stream = pin!(body.into_data_stream());
    let mut buffer = Vec::new();
    loop {
        let next = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await;
        match next {
            None => return Ok(buffer),
            Some(Err(error)) => return Err(IngestError::Body(error.to_string())),
            Some(Ok(chunk)) => {
                if buffer.len() + chunk.len() > max {
                    // Stop here. The rest of the body is never read into memory.
                    return Err(IngestError::TooLarge("the request body"));
                }
                buffer.extend_from_slice(&chunk);
            }
        }
    }
}

/// Decode `raw` according to `content-encoding`.
///
/// An absent, empty, or `identity` encoding passes through. gzip, deflate and br are
/// what the five target SDKs send — Python flips to br on its own whenever the
/// `brotli` module happens to be importable, so br is not optional.
pub fn decompress(encoding: Option<&str>, raw: Vec<u8>) -> Result<Vec<u8>, IngestError> {
    let encoding = encoding.unwrap_or("").trim().to_ascii_lowercase();
    match encoding.as_str() {
        "" | "identity" => Ok(raw),
        "gzip" | "x-gzip" => finish(flate2::read::GzDecoder::new(&raw[..])),
        "deflate" | "zlib" => {
            // `deflate` officially means zlib-wrapped, but clients that send it raw
            // exist and cost one retry to support.
            finish(flate2::read::ZlibDecoder::new(&raw[..]))
                .or_else(|_| finish(flate2::read::DeflateDecoder::new(&raw[..])))
        }
        "br" => finish(brotli_decompressor::Decompressor::new(&raw[..], 8192)),
        other => Err(IngestError::Encoding(format!("unsupported content-encoding {other}"))),
    }
}

/// Drain a decoder, one byte past the cap so going over is detectable.
fn finish(reader: impl Read) -> Result<Vec<u8>, IngestError> {
    let mut out = Vec::new();
    let read = reader
        .take(MAX_DECOMPRESSED as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|error| IngestError::Encoding(error.to_string()))?;
    if read > MAX_DECOMPRESSED {
        return Err(IngestError::TooLarge("the decompressed body"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn gzip(payload: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    fn zlib(payload: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn every_encoding_the_sdks_send_round_trips() {
        let payload = b"{}\n{\"type\":\"event\"}\n{\"event_id\":\"x\"}\n";
        assert_eq!(decompress(None, payload.to_vec()).unwrap(), payload);
        assert_eq!(decompress(Some("identity"), payload.to_vec()).unwrap(), payload);
        assert_eq!(decompress(Some("gzip"), gzip(payload)).unwrap(), payload);
        assert_eq!(decompress(Some("deflate"), zlib(payload)).unwrap(), payload);

        let mut brotli = Vec::new();
        let mut writer = brotli::CompressorWriter::new(&mut brotli, 4096, 5, 22);
        writer.write_all(payload).unwrap();
        drop(writer);
        assert_eq!(decompress(Some("br"), brotli).unwrap(), payload);
    }

    #[test]
    fn a_body_that_lies_about_its_encoding_is_an_encoding_error() {
        assert!(matches!(
            decompress(Some("gzip"), b"not gzip at all".to_vec()),
            Err(IngestError::Encoding(_))
        ));
        assert!(matches!(
            decompress(Some("snappy"), b"x".to_vec()),
            Err(IngestError::Encoding(_))
        ));
    }

    #[test]
    fn a_bomb_stops_at_the_decompressed_cap() {
        let bomb = gzip(&vec![0u8; MAX_DECOMPRESSED + 1024]);
        assert!(bomb.len() < 1024 * 1024, "the point is that the compressed form is small");
        assert_eq!(
            decompress(Some("gzip"), bomb),
            Err(IngestError::TooLarge("the decompressed body"))
        );
    }

    #[tokio::test]
    async fn the_compressed_cap_refuses_before_the_body_is_buffered() {
        let body = Body::from(vec![7u8; 64]);
        assert_eq!(read_capped(body, 1024).await.unwrap().len(), 64);

        let body = Body::from(vec![7u8; 64]);
        assert_eq!(
            read_capped(body, 16).await,
            Err(IngestError::TooLarge("the request body"))
        );
    }
}
