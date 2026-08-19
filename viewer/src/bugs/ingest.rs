//! Reading an envelope off the wire: auth first, size caps second, decompression
//! third.
//!
//! The caps are Relay's, and they exist to stop one request from costing the box its
//! memory. The compressed cap is enforced while the body streams, so an oversized
//! request is refused before it is buffered; the decompressed cap is enforced on the
//! decompressor's output, so a zip bomb stops at 100 MiB rather than at whatever it
//! wanted to expand to.
//!
//! The order matters as much as the caps. A request whose only credential is the
//! `dsn` header inside its own envelope used to be read and expanded in full before
//! anyone checked it, which makes 100 MiB of work available to anybody who can reach
//! the port. [`Reader`] hands back the envelope's first line — where that header
//! lives — before the rest of the body is pulled, so a bad key costs a few hundred
//! bytes. A compressed body has no readable first line, so that path is capped small
//! instead.

use std::io::Read;
use std::pin::Pin;

use futures_core::Stream;
use topcoat::router::{Body, BodyDataStream};

/// Relay's cap on a compressed envelope body.
pub const MAX_COMPRESSED: usize = 20 * 1024 * 1024;
/// Our cap on what a body may decompress to.
pub const MAX_DECOMPRESSED: usize = 100 * 1024 * 1024;

/// How much of a body with no proven credential we will read when it is compressed.
///
/// A compressed body cannot be authenticated without expanding it, so the only honest
/// protection is a small budget. Every SDK that sends compressed envelopes also sends
/// `X-Sentry-Auth` or `?sentry_key=`, both of which are checked before a byte is read
/// and lift this cap; the envelope-`dsn` door stays open for small bodies.
pub const MAX_UNAUTHED_COMPRESSED: usize = 64 * 1024;
/// And what such a body may expand to.
pub const MAX_UNAUTHED_DECOMPRESSED: usize = 4 * 1024 * 1024;
/// The longest envelope header line we will look for a `dsn` in. Real ones carry an
/// event id, a dsn, a sent_at and sometimes a trace context — hundreds of bytes.
pub const MAX_HEADER_LINE: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestError {
    /// Over a size cap. Answers 413; SDKs must not retry.
    TooLarge(&'static str),
    /// The body could not be read.
    Body(String),
    /// The body did not decompress with the encoding it declared.
    Encoding(String),
    /// No `\n` inside [`MAX_HEADER_LINE`], so there is no envelope header to read a
    /// key out of. Answers 403 on the unauthenticated path: it is a refusal, not a
    /// size complaint, and the body stops being read here.
    NoHeaderLine,
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge(what) => write!(f, "{what} is over the size limit"),
            Self::Body(detail) => write!(f, "cannot read the request body: {detail}"),
            Self::Encoding(detail) => write!(f, "cannot decode the request body: {detail}"),
            Self::NoHeaderLine => f.write_str("no envelope header line"),
        }
    }
}

impl std::error::Error for IngestError {}

/// A request body being read in two steps: the envelope header line, then the rest.
///
/// Chunks arrive whole, so the guarantee is "one transport chunk plus whatever the
/// header line needs", not "exactly the header line". That is enough: hyper hands
/// over tens of kilobytes at a time, and the alternative — a byte-at-a-time reader —
/// would cost every honest request to slow one dishonest one.
pub struct Reader {
    stream: Pin<Box<BodyDataStream>>,
    buffer: Vec<u8>,
    ended: bool,
}

impl Reader {
    pub fn new(body: Body) -> Self {
        Self { stream: Box::pin(body.into_data_stream()), buffer: Vec::new(), ended: false }
    }

    /// Pull one more chunk, refusing it if it would take the buffer past `max`.
    /// `false` once the body is finished.
    async fn pull(&mut self, max: usize) -> Result<bool, IngestError> {
        if self.ended {
            return Ok(false);
        }
        match std::future::poll_fn(|cx| self.stream.as_mut().poll_next(cx)).await {
            None => {
                self.ended = true;
                Ok(false)
            }
            Some(Err(error)) => Err(IngestError::Body(error.to_string())),
            Some(Ok(chunk)) => {
                // Tested before the chunk is appended. Appending first and testing
                // after makes the real budget `max` plus one transport chunk, which
                // on the unauthenticated path is most of the budget again.
                if self.buffer.len().saturating_add(chunk.len()) > max {
                    return Err(IngestError::TooLarge("the request body"));
                }
                self.buffer.extend_from_slice(&chunk);
                Ok(true)
            }
        }
    }

    /// The first `\n`-terminated line, without consuming it.
    ///
    /// This is the envelope header, which is the last place an unauthenticated
    /// request can prove itself. A line longer than [`MAX_HEADER_LINE`] is not one.
    pub async fn header_line(&mut self) -> Result<&[u8], IngestError> {
        loop {
            if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                return Ok(&self.buffer[..end]);
            }
            if self.buffer.len() > MAX_HEADER_LINE {
                return Err(IngestError::NoHeaderLine);
            }
            // Uncapped on purpose: a chunk is already in memory when the stream
            // yields it, and a legal envelope's first chunk is routinely larger than
            // one header line. The stop condition above is what bounds this loop.
            if !self.pull(usize::MAX).await? {
                // A header-only envelope with no trailing newline is legal.
                return Ok(&self.buffer[..]);
            }
        }
    }

    /// Read what is left, header line included, refusing the body the moment it
    /// passes `max` rather than at the end. The rest of an oversized body is never
    /// pulled.
    pub async fn read_to_end(mut self, max: usize) -> Result<Vec<u8>, IngestError> {
        // `header_line` may already have buffered more than this caller allows.
        if self.buffer.len() > max {
            return Err(IngestError::TooLarge("the request body"));
        }
        while self.pull(max).await? {}
        Ok(self.buffer)
    }
}

/// Read the body, refusing it the moment it passes `max` rather than at the end.
pub async fn read_capped(body: Body, max: usize) -> Result<Vec<u8>, IngestError> {
    Reader::new(body).read_to_end(max).await
}

/// Decode `raw` according to `content-encoding`.
///
/// An absent, empty, or `identity` encoding passes through. gzip, deflate and br are
/// what the five target SDKs send — Python flips to br on its own whenever the
/// `brotli` module happens to be importable, so br is not optional.
pub fn decompress(encoding: Option<&str>, raw: Vec<u8>) -> Result<Vec<u8>, IngestError> {
    decompress_capped(encoding, raw, MAX_DECOMPRESSED)
}

/// The same, with the expansion budget named. An unauthenticated compressed body gets
/// a small one; see [`MAX_UNAUTHED_DECOMPRESSED`].
pub fn decompress_capped(
    encoding: Option<&str>,
    raw: Vec<u8>,
    max: usize,
) -> Result<Vec<u8>, IngestError> {
    let encoding = encoding.unwrap_or("").trim().to_ascii_lowercase();
    match encoding.as_str() {
        "" | "identity" => {
            if raw.len() > max {
                return Err(IngestError::TooLarge("the decompressed body"));
            }
            Ok(raw)
        }
        // `MultiGzDecoder`, not `GzDecoder`: a body written as several concatenated
        // gzip members is still one legal gzip stream, and `GzDecoder` stops after
        // the first one — which would truncate the envelope silently rather than
        // fail, and a truncated envelope is a lost event.
        "gzip" | "x-gzip" => finish(flate2::read::MultiGzDecoder::new(&raw[..]), max),
        "deflate" | "zlib" => {
            // `deflate` officially means zlib-wrapped, but clients that send it raw
            // exist and cost one retry to support. Only a genuine format error earns
            // that retry: falling back on *any* error would turn an over-cap body
            // into a 400 instead of the 413 the protocol requires, and would expand
            // the bomb a second time to get there.
            match finish(flate2::read::ZlibDecoder::new(&raw[..]), max) {
                Ok(body) => Ok(body),
                Err(error @ IngestError::TooLarge(_)) => Err(error),
                Err(_) => finish(flate2::read::DeflateDecoder::new(&raw[..]), max),
            }
        }
        "br" => finish(brotli_decompressor::Decompressor::new(&raw[..], 8192), max),
        other => Err(IngestError::Encoding(format!("unsupported content-encoding {other}"))),
    }
}

/// Drain a decoder, one byte past the cap so going over is detectable.
fn finish(reader: impl Read, max: usize) -> Result<Vec<u8>, IngestError> {
    let mut out = Vec::new();
    let read = reader
        .take(max as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|error| IngestError::Encoding(error.to_string()))?;
    if read > max {
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

    #[test]
    fn an_over_cap_deflate_body_is_too_large_and_not_a_bad_encoding() {
        // The raw-deflate fallback used to swallow this into an `Encoding` error,
        // which the route answers 400 — and it expanded the bomb a second time on
        // the way.
        let bomb = zlib(&vec![0u8; MAX_DECOMPRESSED + 1024]);
        assert_eq!(
            decompress(Some("deflate"), bomb),
            Err(IngestError::TooLarge("the decompressed body"))
        );
    }

    #[test]
    fn a_raw_deflate_body_still_gets_its_fallback() {
        let payload = b"{}\n{\"type\":\"event\"}\n{\"event_id\":\"x\"}\n";
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload).unwrap();
        let raw = encoder.finish().unwrap();
        assert_eq!(decompress(Some("deflate"), raw).unwrap(), payload);
    }

    #[test]
    fn concatenated_gzip_members_all_decode() {
        // One legal gzip stream can be several members. `GzDecoder` reads the first
        // and stops, which truncates the envelope without erroring.
        let head = b"{}\n{\"type\":\"event\"}\n";
        let tail = b"{\"event_id\":\"x\"}\n";
        let mut body = gzip(head);
        body.extend_from_slice(&gzip(tail));

        let decoded = decompress(Some("gzip"), body).unwrap();
        assert_eq!(decoded, [head.as_slice(), tail.as_slice()].concat());
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
