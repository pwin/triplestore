//! Loading compressed RDF, end to end.
//!
//! Gzipped N-Triples and N-Quads are how large RDF is distributed, so these paths matter
//! as much as the uncompressed ones. The multi-member case has its own test because a
//! single-member decoder fails it *silently* — reporting success having read only the
//! first chunk — which is the worst way to lose data.

use flate2::write::GzEncoder;
use flate2::Compression;
use holos_engine::{source, Engine};
use oxrdfio::RdfFormat;
use std::io::Write;

fn write_gz(dir: &std::path::Path, name: &str, contents: &[u8]) -> std::path::PathBuf {
    let path = dir.join(name);
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(contents).expect("compress");
    std::fs::write(&path, encoder.finish().expect("finish")).expect("write");
    path
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("holos-gzip-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

#[test]
fn a_gzipped_ntriples_file_loads() {
    let dir = temp_dir("nt");
    let path = write_gz(
        &dir,
        "data.nt.gz",
        b"<http://e/a> <http://e/p> <http://e/o> .\n<http://e/b> <http://e/p> <http://e/o> .\n",
    );

    let (format, reader) = source::open(&path).expect("open");
    assert_eq!(format, RdfFormat::NTriples);

    let mut engine = Engine::new();
    let n = engine.bulk_load(reader, format, None).expect("load");
    assert_eq!(n, 2);
    assert_eq!(engine.store().len(), 2);
}

#[test]
fn a_gzipped_nquads_file_keeps_its_graphs() {
    // The property worth pinning: compression must not flatten named graphs, which is what
    // would happen if the format were inferred as N-Triples from the `.gz` extension.
    let dir = temp_dir("nq");
    let path = write_gz(
        &dir,
        "data.nq.gz",
        b"<http://e/a> <http://e/p> <http://e/o> <http://e/g1> .\n\
          <http://e/b> <http://e/p> <http://e/o> <http://e/g2> .\n",
    );

    let (format, reader) = source::open(&path).expect("open");
    assert_eq!(format, RdfFormat::NQuads);

    let mut engine = Engine::new();
    engine.bulk_load(reader, format, None).expect("load");
    assert_eq!(engine.store().len(), 2);
    assert_eq!(
        engine.store().named_graphs().expect("graphs").len(),
        2,
        "both named graphs must survive the round trip"
    );
}

#[test]
fn concatenated_members_all_load() {
    // Real dumps are often produced by concatenating compressed chunks. A single-member
    // decoder reads the first and stops, reporting success — so this test is the one that
    // would catch a regression to `GzDecoder`.
    let dir = temp_dir("multi");
    let path = dir.join("multi.nt.gz");
    let mut bytes = Vec::new();
    for i in 0..5 {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder
            .write_all(format!("<http://e/s{i}> <http://e/p> <http://e/o> .\n").as_bytes())
            .expect("compress");
        bytes.extend(encoder.finish().expect("finish"));
    }
    std::fs::write(&path, bytes).expect("write");

    let (format, reader) = source::open(&path).expect("open");
    let mut engine = Engine::new();
    let n = engine.bulk_load(reader, format, None).expect("load");
    assert_eq!(n, 5, "every gzip member must be read, not just the first");
}

#[test]
fn a_compressed_and_a_plain_file_give_the_same_store() {
    let dir = temp_dir("same");
    // Fully written out: a bare `42` is Turtle shorthand, not valid N-Triples.
    let triples = b"<http://e/a> <http://e/p> \"x\" .
<http://e/b> <http://e/q> \"42\"^^<http://www.w3.org/2001/XMLSchema#integer> .
";

    let plain = dir.join("plain.nt");
    std::fs::write(&plain, triples).expect("write");
    let compressed = write_gz(&dir, "same.nt.gz", triples);

    let load = |path: &std::path::Path| {
        let (format, reader) = source::open(path).expect("open");
        let mut engine = Engine::new();
        engine.bulk_load(reader, format, None).expect("load");
        engine
    };

    let a = load(&plain);
    let b = load(&compressed);
    assert_eq!(a.store().len(), b.store().len());
    assert_eq!(
        a.store().dictionary_len(),
        b.store().dictionary_len(),
        "compression must not change what gets interned"
    );
}

#[test]
fn an_unknown_extension_is_refused_with_a_useful_message() {
    let dir = temp_dir("unknown");
    let path = write_gz(&dir, "mystery.gz", b"nothing useful");
    // `expect_err` would need Debug on the Ok side, and a boxed reader has none.
    let message = match source::open(&path) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("a bare .gz should not resolve to a format"),
    };
    assert!(
        message.contains(".nt") && message.contains(".gz"),
        "the message should say what is accepted: {message}"
    );
}

#[test]
fn a_truncated_gzip_file_errors_rather_than_loading_nothing() {
    // Silently loading zero quads from a corrupt file would look like an empty dataset.
    let dir = temp_dir("truncated");
    let path = dir.join("bad.nt.gz");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(b"<http://e/a> <http://e/p> <http://e/o> .\n")
        .expect("compress");
    let mut bytes = encoder.finish().expect("finish");
    bytes.truncate(bytes.len() / 2);
    std::fs::write(&path, bytes).expect("write");

    let (format, reader) = source::open(&path).expect("open");
    let mut engine = Engine::new();
    assert!(
        engine.bulk_load(reader, format, None).is_err(),
        "a truncated stream must be an error, not an empty load"
    );
}
