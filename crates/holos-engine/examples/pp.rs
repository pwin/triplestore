fn main() {
    for q in [
        "SELECT * WHERE { ?s <http://e/p>/<http://e/q> ?o }",
        "SELECT * WHERE { ?s ^<http://e/p> ?o }",
        "SELECT * WHERE { ?s <http://e/p>|<http://e/q> ?o }",
        "SELECT * WHERE { ?s <http://e/p>+ ?o }",
        "SELECT * WHERE { ?s <http://e/p> ?o MINUS { ?s <http://e/q> ?x } }",
    ] {
        let p = spargebra::SparqlParser::new().parse_query(q).unwrap();
        println!("--- {q}\n{p:?}\n");
    }
}
