//! Writing a store out as RDF: every quad a view can see, the default graph and every named
//! graph, in one pass.
//!
//! The command line's `dump`, the WebAssembly `dump()`, Python's `Store.dump()` and the
//! server's snapshot of an in-memory store all come through here, so a dump means the same
//! thing on every surface and restores the same way: load the file back with `--data`,
//! `Store.load()` or `load()`.
//!
//! N-Quads is the format to use for a backup. It is the only line-based format that carries
//! the graph name, so a dump of a quad store round-trips through it without loss; `TriG` does
//! too. A graph-only format refuses a store that has named graphs rather than flattening them.
//!
//! A dump reads through a view, so what it writes is what that view's session may see. A
//! backup wants everything, and its caller opens the session that way.

use crate::view::DatasetView;
use crate::{Engine, EngineError};
use flate2::write::GzEncoder;
use oxrdf::{GraphName, NamedOrBlankNode, Quad, Term};
use oxrdfio::{RdfFormat, RdfSerializer};
use spareval::QueryResults;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

/// The default graph and the named graphs together. A plain `?s ?p ?o` would silently omit
/// every named graph.
const EVERY_QUAD: &str = "SELECT * WHERE { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }";

/// Writes every quad `view` can see to `writer` in `format`.
///
/// Returns the number of quads written and the writer, finished: whatever the format needs at
/// its end has been written, and nothing has been flushed beyond that.
///
/// # Errors
///
/// [`EngineError::BadRequest`] when the store has a named graph and `format` cannot carry one,
/// naming the formats that can. Otherwise a failure reading the store or writing the output.
pub fn write<W: Write>(
    view: &DatasetView<'_>,
    format: RdfFormat,
    writer: W,
) -> Result<(u64, W), EngineError> {
    let QueryResults::Solutions(solutions) = Engine::query(view, EVERY_QUAD, None)? else {
        return Err(EngineError::BadRequest(
            "the dump query did not return solutions".to_owned(),
        ));
    };

    let mut serializer = RdfSerializer::from_format(format).for_writer(writer);
    let mut written = 0_u64;
    for solution in solutions {
        let solution = solution?;
        let (Some(s), Some(p), Some(o)) = (solution.get("s"), solution.get("p"), solution.get("o"))
        else {
            continue;
        };
        // A literal or a triple term in subject position, or anything but an IRI as a
        // predicate, cannot be written as a quad. Nothing in the store can hold one, so these
        // arms are unreachable rather than a silent drop -- but skipping is the safe reading.
        let subject = match s {
            Term::NamedNode(n) => NamedOrBlankNode::NamedNode(n.clone()),
            Term::BlankNode(b) => NamedOrBlankNode::BlankNode(b.clone()),
            _ => continue,
        };
        let Term::NamedNode(predicate) = p else {
            continue;
        };
        let graph_name = match solution.get("g") {
            Some(Term::NamedNode(n)) => GraphName::NamedNode(n.clone()),
            Some(Term::BlankNode(b)) => GraphName::BlankNode(b.clone()),
            _ => GraphName::DefaultGraph,
        };
        let named_graph = !graph_name.is_default_graph();
        let quad = Quad {
            subject,
            predicate: predicate.clone(),
            object: o.clone(),
            graph_name,
        };
        serializer.serialize_quad(quad.as_ref()).map_err(|e| {
            // oxrdfio's own message says what is wrong but not what to do about it, and this
            // is the one failure a caller will actually hit.
            if named_graph && !format.supports_datasets() {
                EngineError::BadRequest(format!(
                    "this store has named graphs and {} cannot carry one; use nquads or \
                     trig, which can ({e})",
                    format.name()
                ))
            } else {
                EngineError::Io(e)
            }
        })?;
        written += 1;
    }
    Ok((written, serializer.finish()?))
}

/// Writes every quad `view` can see to the file at `path`.
///
/// The format is `format`, or the one the file name implies (see
/// [`crate::source::format_for_path`]); a `.gz` suffix compresses the output, so
/// `backup.nq.gz` is what `--data` and `Store.load()` read back.
///
/// The file appears complete or not at all. The dump goes to a temporary file beside `path`
/// and is renamed over it only once it is finished, so a reader -- or the next scheduled
/// restore -- never sees half a dump, and a failure part-way leaves any earlier file at
/// `path` as it was.
///
/// # Errors
///
/// As [`write`], and when the format cannot be inferred or the file cannot be written.
pub fn to_file(
    view: &DatasetView<'_>,
    path: &Path,
    format: Option<RdfFormat>,
) -> Result<u64, EngineError> {
    let format = match format {
        Some(format) => format,
        None => crate::source::format_for_path(path).ok_or_else(|| {
            EngineError::BadRequest(format!(
                "cannot infer an RDF format from `{}`; expected .nq .trig .ttl .nt .rdf .n3 \
                 .jsonld, optionally with .gz",
                path.display()
            ))
        })?,
    };
    let partial = partial_path(path);
    let result = write_file(view, &partial, crate::source::is_compressed(path), format).and_then(
        |written| {
            std::fs::rename(&partial, path)?;
            Ok(written)
        },
    );
    if result.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    result
}

fn write_file(
    view: &DatasetView<'_>,
    partial: &Path,
    compressed: bool,
    format: RdfFormat,
) -> Result<u64, EngineError> {
    let file = BufWriter::new(File::create(partial)?);
    if compressed {
        let (written, encoder) = write(
            view,
            format,
            GzEncoder::new(file, flate2::Compression::default()),
        )?;
        encoder
            .finish()?
            .into_inner()
            .map_err(std::io::IntoInnerError::into_error)?
            .sync_all()?;
        Ok(written)
    } else {
        let (written, file) = write(view, format, file)?;
        file.into_inner()
            .map_err(std::io::IntoInnerError::into_error)?
            .sync_all()?;
        Ok(written)
    }
}

/// `backup.nq.gz` -> `backup.nq.gz.partial`, in the same directory so the rename is atomic.
fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    path.with_file_name(name)
}
