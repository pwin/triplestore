//! Prints the query a mapping becomes once a batch of rows is bound into it.
//!
//! `cargo run -p holos-tabular --example debug_mapping`
use holos_engine::Engine;
use holos_security::{Policy, Principal, Session};
use holos_tabular::{
    source::{Csv, CsvOptions, RowSource},
    Mapping,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mapping = Mapping::parse(
        r#"PREFIX ex: <http://example.org/>
           CONSTRUCT { ?person a ex:Person ; ex:name ?name }
           WHERE { BIND(IRI(CONCAT("http://example.org/person/", ?id)) AS ?person) }"#,
    )?;
    println!("mapping variables: {:?}", mapping.variables());

    let mut source = Csv::from_reader(
        std::io::Cursor::new("id,name\n1,Alice\n2,Bob\n".to_owned()),
        &CsvOptions::default(),
    )?;
    let rows = source.next_batch(10, 0)?;
    println!("rows: {rows:#?}");

    let engine = Engine::new();
    let session = Session::open(engine.store(), Principal::anonymous(), Policy::permit_all())?;
    match mapping.apply(&engine, &session, &rows) {
        Ok(triples) => {
            println!("produced {} triples", triples.len());
            for t in triples.iter().take(6) {
                println!("  {t}");
            }
        }
        Err(e) => println!("FAILED: {e}"),
    }
    Ok(())
}
