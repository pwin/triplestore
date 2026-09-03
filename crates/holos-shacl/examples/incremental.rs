//! Measures what incremental revalidation actually buys.
//!
//! `DESIGN.md` §8 claims incremental revalidation makes SHACL affordable on the write
//! path, and that the cost tracks the size of the change rather than the size of the
//! graph. This is the measurement behind that claim.
//!
//! ```text
//! cargo run --release -p holos-shacl --example incremental -- <data.nt> <shapes.ttl>
//! ```

use holos_shacl::incremental::Change;
use holos_shacl::{CompiledShapes, Options};
use holos_store::{GraphFilter, Store};
use oxrdf::NamedNode;
use oxrdfio::{RdfFormat, RdfParser};
use std::io::BufReader;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let data = args
        .next()
        .ok_or("usage: incremental <data.nt> <shapes.ttl>")?;
    let shapes_file = args
        .next()
        .ok_or("usage: incremental <data.nt> <shapes.ttl>")?;

    let mut store = Store::new();
    let started = Instant::now();
    for quad in RdfParser::from_format(RdfFormat::NTriples)
        .for_reader(BufReader::new(std::fs::File::open(&data)?))
    {
        store.insert(quad?.as_ref())?;
    }
    let load = started.elapsed();

    let shapes_name = NamedNode::new_unchecked("urn:holos:shapes");
    for quad in RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri("http://example.com/")?
        .for_reader(BufReader::new(std::fs::File::open(&shapes_file)?))
    {
        let mut quad = quad?;
        quad.graph_name = shapes_name.clone().into();
        store.insert(quad.as_ref())?;
    }
    let shapes_graph = GraphFilter::Named(
        store
            .lookup_term(shapes_name.as_ref().into())?
            .ok_or("shapes graph did not intern")?,
    );

    let started = Instant::now();
    let compiled = CompiledShapes::compile(
        &store,
        Options {
            data_graph: GraphFilter::Default,
            shapes_graph,
        },
    )?;
    let compile = started.elapsed();

    let started = Instant::now();
    let full = compiled.validate(&store)?;
    let full_time = started.elapsed();

    // One new violating triple, the shape of a single write on the holon Boundary.
    let quad = oxrdf::Quad {
        subject: NamedNode::new_unchecked("http://example.com/person42").into(),
        predicate: NamedNode::new_unchecked("http://example.com/email"),
        object: oxrdf::Literal::new_simple_literal("definitely-not-an-email").into(),
        graph_name: oxrdf::GraphName::DefaultGraph,
    };
    let encoded = store.encode_quad(quad.as_ref())?;
    store.insert_encoded(encoded)?;

    let started = Instant::now();
    let incremental = compiled.revalidate(&store, &[Change::added(encoded)])?;
    let incremental_time = started.elapsed();

    let started = Instant::now();
    let full_again = compiled.validate(&store)?;
    let full_again_time = started.elapsed();

    println!("quads              {}", store.len());
    println!("dictionary terms   {}", store.dictionary_len());
    println!("load               {:.3}s", load.as_secs_f64());
    println!("compile shapes     {:.4}s", compile.as_secs_f64());
    println!(
        "full validation    {:.3}s  ({} results)",
        full_time.as_secs_f64(),
        full.results.len()
    );
    println!(
        "full, after change {:.3}s  ({} results)",
        full_again_time.as_secs_f64(),
        full_again.results.len()
    );
    println!(
        "incremental        {:.5}s ({} results)",
        incremental_time.as_secs_f64(),
        incremental.results.len()
    );
    println!(
        "speed-up           {:.0}x",
        full_again_time.as_secs_f64() / incremental_time.as_secs_f64().max(f64::MIN_POSITIVE)
    );
    Ok(())
}
