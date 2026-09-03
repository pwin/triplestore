//! Prints the parsed algebra for a query, so a validation pass can be written against what
//! the parser actually produces rather than against what it might.
fn main() {
    for q in std::env::args().skip(1) {
        match spargebra::SparqlParser::new().parse_query(&q) {
            Ok(parsed) => println!("OK   {q}\n     {parsed:?}\n"),
            Err(e) => println!("ERR  {q}\n     {e}\n"),
        }
    }
}
