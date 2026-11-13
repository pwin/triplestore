//! A SHACL-SPARQL message is filled from a decimal product that needs rounding.
//!
//! The report that prompted this: a message printed `{?ratio}` as written, because the
//! constraint computed `?ratio` as a product of two decimals with more than 18 fractional
//! digits between them, the registry `oxsdatatypes` refused that product, and the variable
//! was left unbound. The constraint still fired, so the error surfaced only as a placeholder
//! in the text. With `vendor/oxsdatatypes` the product is rounded to 18 digits and the
//! message carries it.

use holos_shacl::engine::EngineRun;
use holos_shacl::Options;
use holos_store::{GraphFilter, Store};
use oxrdf::{NamedNodeRef, TermRef};
use oxrdfio::{RdfFormat, RdfParser};

const SHAPES_AND_DATA: &str = r#"
@prefix ex: <http://example.com/> .
@prefix sh: <http://www.w3.org/ns/shacl#> .

ex:OrderShape a sh:NodeShape ;
    sh:targetClass ex:Order ;
    sh:sparql [
        sh:message "ratio is {?ratio}" ;
        sh:select """
            SELECT $this ?ratio WHERE {
                $this <http://example.com/quantity> ?q ; <http://example.com/rate> ?r .
                BIND(?q * ?r AS ?ratio)
            }""" ;
    ] .

ex:order a ex:Order ; ex:quantity 1.3 ; ex:rate 31.583333333333333333 .
"#;

#[test]
fn the_message_carries_the_rounded_product() {
    let mut store = Store::new();
    let parser = RdfParser::from_format(RdfFormat::Turtle)
        .with_base_iri("http://example.com/")
        .expect("base");
    for quad in parser.for_reader(SHAPES_AND_DATA.as_bytes()) {
        store.insert(quad.expect("parse").as_ref()).expect("insert");
    }
    let options = Options {
        data_graph: GraphFilter::Default,
        shapes_graph: GraphFilter::Default,
    };
    let mut run = EngineRun::prepare(&store, options).expect("prepare");
    let report = run.validate().expect("validate");
    let graph = run.report_to_oxrdf(&report);
    let message = NamedNodeRef::new_unchecked("http://www.w3.org/ns/shacl#resultMessage");
    let messages: Vec<String> = graph
        .triples_for_predicate(message)
        .filter_map(|t| match t.object {
            TermRef::Literal(l) => Some(l.value().to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(messages, ["ratio is 41.058333333333333333"]);
}
