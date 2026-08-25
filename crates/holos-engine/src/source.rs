//! Opening an RDF file, including compressed ones.
//!
//! Gzipped N-Triples and N-Quads are how large RDF is actually distributed — DBpedia,
//! Wikidata, and essentially every LOD dump ship as `.nt.gz` or `.nq.gz`, because the
//! format is enormously redundant and compresses about tenfold. A store that cannot read
//! them makes its first step "spend ten minutes and ten times the disk decompressing".
//!
//! # What is handled
//!
//! - `.gz` on any recognised extension: `data.nt.gz`, `dump.nq.gz`, `ontology.ttl.gz`.
//! - **Multi-member** gzip streams. Large dumps are often produced by concatenating
//!   compressed chunks, which is valid gzip and which a single-member decoder silently
//!   truncates at the first member's end — losing most of the file and reporting success.
//!   [`MultiGzDecoder`] reads all members, so that failure mode cannot occur.
//! - Decompression is **streamed**, never buffered whole. A 60 GB dump uses no more memory
//!   than a small one.

use crate::EngineError;
use flate2::read::MultiGzDecoder;
use oxrdfio::RdfFormat;
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

/// The RDF format a file name implies, seeing through a `.gz` suffix.
///
/// Returns `None` when the name says nothing useful — including for a bare `.gz` with no
/// inner extension, because guessing at that would be worse than asking.
#[must_use]
pub fn format_for_path(path: &Path) -> Option<RdfFormat> {
    let (path, _) = strip_compression(path);
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "ttl" => RdfFormat::Turtle,
        "nt" => RdfFormat::NTriples,
        "trig" => RdfFormat::TriG,
        "nq" => RdfFormat::NQuads,
        "rdf" | "owl" | "rdfs" => RdfFormat::RdfXml,
        "n3" => RdfFormat::N3,
        "jsonld" | "json" => RdfFormat::JsonLd {
            profile: oxrdfio::JsonLdProfileSet::empty(),
        },
        _ => return None,
    })
}

/// Whether a path names a compressed file this module can read.
#[must_use]
pub fn is_compressed(path: &Path) -> bool {
    strip_compression(path).1
}

/// Splits a `.gz` suffix off, returning the inner path and whether one was there.
fn strip_compression(path: &Path) -> (std::path::PathBuf, bool) {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("gz") || ext.eq_ignore_ascii_case("gzip") => {
            (path.with_extension(""), true)
        }
        _ => (path.to_path_buf(), false),
    }
}

/// Opens an RDF file for reading, decompressing if the name says to.
///
/// # Errors
///
/// Fails if the file cannot be opened, or if no format can be inferred from its name.
pub fn open(path: &Path) -> Result<(RdfFormat, Box<dyn BufRead + Send>), EngineError> {
    let format = format_for_path(path).ok_or_else(|| {
        EngineError::Io(std::io::Error::other(format!(
            "cannot infer an RDF format from `{}`; expected .ttl .nt .trig .nq .rdf .n3 \
             .jsonld, optionally with .gz",
            path.display()
        )))
    })?;
    Ok((format, reader(path)?))
}

/// Opens a reader for a path, decompressing if the name says to.
///
/// # Errors
///
/// Fails if the file cannot be opened.
pub fn reader(path: &Path) -> Result<Box<dyn BufRead + Send>, EngineError> {
    let file = File::open(path)
        .map_err(|e| EngineError::Io(std::io::Error::other(format!("opening {}: {e}", path.display()))))?;
    Ok(wrap(BufReader::new(file), is_compressed(path)))
}

/// Wraps a reader in a decompressor when asked.
///
/// The inner `BufReader` sizes the decompressor's input; the outer one gives the parser a
/// buffered decompressed stream. Both are wanted — without the outer one every read from
/// the parser would go through the inflate machinery a few bytes at a time.
pub fn wrap<R: Read + Send + 'static>(inner: R, compressed: bool) -> Box<dyn BufRead + Send> {
    if compressed {
        Box::new(BufReader::with_capacity(
            64 * 1024,
            MultiGzDecoder::new(inner),
        ))
    } else {
        Box::new(BufReader::with_capacity(64 * 1024, inner))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).expect("write");
        encoder.finish().expect("finish")
    }

    #[test]
    fn the_format_is_seen_through_a_gz_suffix() {
        assert_eq!(
            format_for_path(Path::new("data.nt.gz")),
            Some(RdfFormat::NTriples)
        );
        assert_eq!(
            format_for_path(Path::new("dump.nq.gz")),
            Some(RdfFormat::NQuads)
        );
        assert_eq!(
            format_for_path(Path::new("o.ttl.GZ")),
            Some(RdfFormat::Turtle)
        );
        assert_eq!(format_for_path(Path::new("plain.nt")), Some(RdfFormat::NTriples));
    }

    #[test]
    fn a_bare_gz_is_refused_rather_than_guessed() {
        // Guessing here would mean parsing an unknown format and reporting a syntax error
        // from somewhere unhelpful.
        assert_eq!(format_for_path(Path::new("mystery.gz")), None);
        assert_eq!(format_for_path(Path::new("notes.txt")), None);
    }

    #[test]
    fn compression_is_detected_by_name() {
        assert!(is_compressed(Path::new("a.nt.gz")));
        assert!(is_compressed(Path::new("a.nq.GZIP")));
        assert!(!is_compressed(Path::new("a.nt")));
    }

    #[test]
    fn a_gzip_stream_round_trips() {
        let text = b"<http://a> <http://b> <http://c> .\n";
        let mut out = String::new();
        wrap(std::io::Cursor::new(gzip(text)), true)
            .read_to_string(&mut out)
            .expect("read");
        assert_eq!(out.as_bytes(), text);
    }

    #[test]
    fn concatenated_gzip_members_are_all_read() {
        // The failure this guards against: a single-member decoder stops at the end of the
        // first member and reports success, so a concatenated dump loses everything after
        // its first chunk — silently, which is the worst way to lose data.
        let mut stream = gzip(b"<http://a> <http://b> <http://c> .\n");
        stream.extend(gzip(b"<http://d> <http://e> <http://f> .\n"));

        let mut out = String::new();
        wrap(std::io::Cursor::new(stream), true)
            .read_to_string(&mut out)
            .expect("read");
        assert_eq!(out.lines().count(), 2, "both members must be read: {out:?}");
        assert!(out.contains("http://d"), "the second member is missing");
    }

    #[test]
    fn an_uncompressed_stream_passes_through() {
        let text = b"hello";
        let mut out = String::new();
        wrap(std::io::Cursor::new(text.to_vec()), false)
            .read_to_string(&mut out)
            .expect("read");
        assert_eq!(out, "hello");
    }
}
