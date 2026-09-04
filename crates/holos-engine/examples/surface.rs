//! Generates the SPARQL surface reference by probing, not by transcribing.
//!
//! A hand-written list of "supported functions" is a list of what somebody believed on the
//! day they wrote it. This one is produced by running every entry against a live store and
//! recording what happened, so it cannot drift from the build that generated it — and when
//! something is unsupported, the reason is the engine's own error text.
//!
//! ```text
//! cargo run --release -p holos-engine --example surface > SPARQL-SURFACE.md
//! ```

use holos_engine::Engine;
use holos_security::Session;
use oxrdf::vocab::rdf;
use oxrdf::{GraphName, Literal, NamedNode, Quad, Term};
use spareval::QueryResults;
use std::collections::BTreeSet;

const EX: &str = "http://example.org/";
const GEOF: &str = "http://www.opengis.net/def/function/geosparql/";
const UOM: &str = "http://www.opengis.net/def/uom/OGC/1.0/";

fn ex(name: &str) -> NamedNode {
    NamedNode::new_unchecked(format!("{EX}{name}"))
}

/// One thing to probe.
struct Probe {
    /// What it is called in the specification.
    name: &'static str,
    /// A query exercising it. Must return at least one row when supported.
    sparql: String,
    /// Where the specification defines it.
    section: &'static str,
    /// Anything a reader needs to know beyond "it works".
    note: &'static str,
    /// Register a federated endpoint before running.
    ///
    /// `SERVICE` is supported, but only against a handler that was given endpoints — so a
    /// probe without one would report "not supported" and be read as "the keyword is
    /// missing", which is the opposite of true.
    federated: bool,
}

fn p(name: &'static str, section: &'static str, sparql: String, note: &'static str) -> Probe {
    Probe {
        name,
        sparql,
        section,
        note,
        federated: false,
    }
}

/// A probe that needs a federated endpoint registered.
fn federated(mut probe: Probe) -> Probe {
    probe.federated = true;
    probe
}

/// A tiny dataset with something of every shape a probe might need.
fn store() -> Engine {
    let mut engine = Engine::new();
    let quads = vec![
        Quad {
            subject: ex("alice").into(),
            predicate: rdf::TYPE.into_owned(),
            object: Term::NamedNode(ex("Person")),
            graph_name: GraphName::DefaultGraph,
        },
        Quad {
            subject: ex("alice").into(),
            predicate: ex("name"),
            object: Literal::new_simple_literal("Alice").into(),
            graph_name: GraphName::DefaultGraph,
        },
        Quad {
            subject: ex("alice").into(),
            predicate: ex("age"),
            object: Literal::new_typed_literal("30", oxrdf::vocab::xsd::INTEGER).into(),
            graph_name: GraphName::DefaultGraph,
        },
        Quad {
            subject: ex("alice").into(),
            predicate: ex("knows"),
            object: Term::NamedNode(ex("bob")),
            graph_name: GraphName::DefaultGraph,
        },
        Quad {
            subject: ex("bob").into(),
            predicate: ex("name"),
            object: Literal::new_simple_literal("Bob").into(),
            graph_name: GraphName::DefaultGraph,
        },
        Quad {
            subject: ex("bob").into(),
            predicate: ex("knows"),
            object: Term::NamedNode(ex("carol")),
            graph_name: GraphName::DefaultGraph,
        },
        Quad {
            subject: ex("carol").into(),
            predicate: ex("name"),
            object: Literal::new_language_tagged_literal_unchecked("Carole", "fr").into(),
            graph_name: GraphName::DefaultGraph,
        },
        Quad {
            subject: ex("alice").into(),
            predicate: ex("secret"),
            object: Literal::new_simple_literal("s").into(),
            graph_name: GraphName::NamedNode(ex("private")),
        },
    ];
    for quad in quads {
        engine.store_mut().insert(quad.as_ref()).expect("insert");
    }
    engine
}

/// Runs a probe. `Ok(rows)` when it evaluated; `Err(reason)` when it did not.
fn probe(engine: &Engine, session: &Session, entry: &Probe) -> Result<usize, String> {
    let view = engine.view(session);
    let trim = |e: holos_engine::EngineError| {
        let text = e.to_string();
        // Trim the engine's prefix so the table shows the reason rather than the wrapper.
        text.rsplit(": ").next().unwrap_or(&text).trim().to_owned()
    };

    let results = if entry.federated {
        let remote = oxrdf::Dataset::from_iter([Quad::new(
            ex("remote-subject"),
            ex("name"),
            Literal::new_simple_literal("from the endpoint"),
            GraphName::DefaultGraph,
        )]);
        let handler =
            holos_engine::service::LocalServiceHandler::new().with_endpoint(ex("remote"), remote);
        let parsed = spargebra::SparqlParser::new()
            .parse_query(&entry.sparql)
            .map_err(|e| e.to_string())?;
        Engine::query_prepared_with_services(&view, &parsed, handler).map_err(trim)?
    } else {
        Engine::query(&view, &entry.sparql, None).map_err(trim)?
    };
    match results {
        QueryResults::Solutions(iter) => {
            let mut n = 0;
            for solution in iter {
                solution.map_err(|e| e.to_string())?;
                n += 1;
            }
            Ok(n)
        }
        QueryResults::Boolean(_) => Ok(1),
        QueryResults::Graph(iter) => {
            let mut n = 0;
            for triple in iter {
                triple.map_err(|e| e.to_string())?;
                n += 1;
            }
            Ok(n)
        }
    }
}

fn prefixes() -> String {
    format!(
        "PREFIX ex: <{EX}> \
         PREFIX rdf: <http://www.w3.org/1999/02/22-rdf-syntax-ns#> \
         PREFIX xsd: <http://www.w3.org/2001/XMLSchema#> \
         PREFIX geo: <http://www.opengis.net/ont/geosparql#> \
         PREFIX geof: <{GEOF}> \
         PREFIX uom: <{UOM}> \
         PREFIX holos: <https://holos.dev/ns#> "
    )
}

/// `SELECT (expr AS ?v) WHERE {}` — the shape most function probes take.
fn expr(e: &str) -> String {
    format!("{} SELECT ({e} AS ?v) WHERE {{}}", prefixes())
}

/// A probe that needs data under it.
fn over_data(body: &str) -> String {
    format!("{} SELECT * WHERE {{ {body} }}", prefixes())
}

fn functional_forms() -> Vec<Probe> {
    vec![
        p("BOUND", "17.4.1.1", expr("BOUND(?x)"), ""),
        p("IF", "17.4.1.2", expr(r#"IF(true, "y", "n")"#), ""),
        p(
            "COALESCE",
            "17.4.1.3",
            expr(r#"COALESCE(?missing, "fallback")"#),
            "",
        ),
        p(
            "EXISTS",
            "17.4.1.4",
            over_data("?s ex:name ?n FILTER EXISTS { ?s ex:age ?a }"),
            "",
        ),
        p(
            "NOT EXISTS",
            "17.4.1.5",
            over_data("?s ex:name ?n FILTER NOT EXISTS { ?s ex:age ?a }"),
            "",
        ),
        p("logical &&", "17.4.1.6", expr("(true && false)"), ""),
        p("logical ||", "17.4.1.7", expr("(true || false)"), ""),
        p("RDFterm-equal (=)", "17.4.1.8", expr(r#"("a" = "a")"#), ""),
        p("sameTerm", "17.4.1.9", expr(r#"sameTerm("a", "a")"#), ""),
        p("IN", "17.4.1.10", expr(r#"(1 IN (1, 2, 3))"#), ""),
        p("NOT IN", "17.4.1.11", expr(r#"(9 NOT IN (1, 2))"#), ""),
    ]
}

fn term_functions() -> Vec<Probe> {
    vec![
        p(
            "isIRI / isURI",
            "17.4.2.1",
            expr("isIRI(ex:a)"),
            "`isURI` is a synonym",
        ),
        p("isBlank", "17.4.2.2", expr("isBlank(BNODE())"), ""),
        p("isLiteral", "17.4.2.3", expr(r#"isLiteral("a")"#), ""),
        p("isNumeric", "17.4.2.4", expr("isNumeric(1)"), ""),
        p("STR", "17.4.2.5", expr("STR(1)"), ""),
        p("LANG", "17.4.2.6", expr(r#"LANG("a"@en)"#), ""),
        p("DATATYPE", "17.4.2.7", expr("STR(DATATYPE(1))"), ""),
        p(
            "IRI / URI",
            "17.4.2.8",
            expr(r#"STR(IRI("http://e/x"))"#),
            "`URI` is a synonym",
        ),
        p("BNODE", "17.4.2.9", expr("isBlank(BNODE())"), ""),
        p(
            "STRDT",
            "17.4.2.10",
            expr(r#"STR(STRDT("1", xsd:integer))"#),
            "",
        ),
        p(
            "STRLANG",
            "17.4.2.11",
            expr(r#"LANG(STRLANG("x", "en"))"#),
            "",
        ),
        p("UUID", "17.4.2.12", expr("STR(UUID())"), ""),
        p("STRUUID", "17.4.2.13", expr("STRLEN(STRUUID())"), ""),
    ]
}

fn string_functions() -> Vec<Probe> {
    vec![
        p("STRLEN", "17.4.3.2", expr(r#"STRLEN("abc")"#), ""),
        p("SUBSTR", "17.4.3.3", expr(r#"SUBSTR("abcdef", 2, 3)"#), ""),
        p("UCASE", "17.4.3.4", expr(r#"UCASE("ab")"#), ""),
        p("LCASE", "17.4.3.5", expr(r#"LCASE("AB")"#), ""),
        p(
            "STRSTARTS",
            "17.4.3.6",
            expr(r#"STRSTARTS("abc", "a")"#),
            "",
        ),
        p("STRENDS", "17.4.3.7", expr(r#"STRENDS("abc", "c")"#), ""),
        p("CONTAINS", "17.4.3.8", expr(r#"CONTAINS("abc", "b")"#), ""),
        p(
            "STRBEFORE",
            "17.4.3.9",
            expr(r#"STRBEFORE("abc", "b")"#),
            "",
        ),
        p("STRAFTER", "17.4.3.10", expr(r#"STRAFTER("abc", "b")"#), ""),
        p(
            "ENCODE_FOR_URI",
            "17.4.3.11",
            expr(r#"ENCODE_FOR_URI("a b")"#),
            "",
        ),
        p("CONCAT", "17.4.3.12", expr(r#"CONCAT("a", "b")"#), ""),
        p(
            "langMatches",
            "17.4.3.13",
            expr(r#"langMatches("en-GB", "en")"#),
            "",
        ),
        p("REGEX", "17.4.3.14", expr(r#"REGEX("abc", "^a")"#), ""),
        p(
            "REPLACE",
            "17.4.3.15",
            expr(r#"REPLACE("abc", "b", "X")"#),
            "",
        ),
    ]
}

fn numeric_functions() -> Vec<Probe> {
    vec![
        p("abs", "17.4.4.1", expr("abs(-1)"), ""),
        p("round", "17.4.4.2", expr("round(1.7)"), ""),
        p("ceil", "17.4.4.3", expr("ceil(1.2)"), ""),
        p("floor", "17.4.4.4", expr("floor(1.8)"), ""),
        p("RAND", "17.4.4.5", expr("(RAND() >= 0)"), ""),
    ]
}

fn datetime_functions() -> Vec<Probe> {
    vec![
        p("now", "17.4.5.1", expr("YEAR(now())"), ""),
        p("year", "17.4.5.2", expr("year(now())"), ""),
        p("month", "17.4.5.3", expr("month(now())"), ""),
        p("day", "17.4.5.4", expr("day(now())"), ""),
        p("hours", "17.4.5.5", expr("hours(now())"), ""),
        p("minutes", "17.4.5.6", expr("minutes(now())"), ""),
        p("seconds", "17.4.5.7", expr("seconds(now())"), ""),
        p("timezone", "17.4.5.8", expr("STR(timezone(now()))"), ""),
        p("tz", "17.4.5.9", expr("tz(now())"), ""),
    ]
}

fn hash_functions() -> Vec<Probe> {
    vec![
        p("MD5", "17.4.6.1", expr(r#"MD5("a")"#), ""),
        p("SHA1", "17.4.6.2", expr(r#"SHA1("a")"#), ""),
        p("SHA256", "17.4.6.3", expr(r#"SHA256("a")"#), ""),
        p("SHA384", "17.4.6.4", expr(r#"SHA384("a")"#), ""),
        p("SHA512", "17.4.6.5", expr(r#"SHA512("a")"#), ""),
    ]
}

fn aggregates() -> Vec<Probe> {
    let agg = |a: &str| {
        format!(
            "{} SELECT ({a} AS ?v) WHERE {{ ?s ex:name ?n }}",
            prefixes()
        )
    };
    vec![
        p("COUNT", "18.5.1.1", agg("COUNT(?n)"), ""),
        p("COUNT(DISTINCT)", "18.5.1.1", agg("COUNT(DISTINCT ?n)"), ""),
        p("SUM", "18.5.1.2", agg("SUM(STRLEN(?n))"), ""),
        p("MIN", "18.5.1.3", agg("MIN(?n)"), ""),
        p("MAX", "18.5.1.4", agg("MAX(?n)"), ""),
        p("AVG", "18.5.1.5", agg("AVG(STRLEN(?n))"), ""),
        p(
            "GROUP_CONCAT",
            "18.5.1.6",
            agg("GROUP_CONCAT(?n; SEPARATOR=\",\")"),
            "",
        ),
        p("SAMPLE", "18.5.1.7", agg("SAMPLE(?n)"), ""),
    ]
}

fn casts() -> Vec<Probe> {
    vec![
        p("xsd:integer", "17.5", expr(r#"xsd:integer("42")"#), ""),
        p("xsd:decimal", "17.5", expr(r#"xsd:decimal("4.2")"#), ""),
        p("xsd:float", "17.5", expr(r#"xsd:float("4.2")"#), ""),
        p("xsd:double", "17.5", expr(r#"xsd:double("4.2")"#), ""),
        p("xsd:string", "17.5", expr("xsd:string(42)"), ""),
        p("xsd:boolean", "17.5", expr(r#"xsd:boolean("true")"#), ""),
        p(
            "xsd:dateTime",
            "17.5",
            expr(r#"STR(xsd:dateTime("2026-01-01T00:00:00Z"))"#),
            "",
        ),
    ]
}

fn sparql12() -> Vec<Probe> {
    let t = "TRIPLE(ex:s, ex:p, ex:o)";
    vec![
        p(
            "TRIPLE",
            "1.2",
            expr(&format!("isTRIPLE({t})")),
            "constructs a triple term",
        ),
        p("isTRIPLE", "1.2", expr(&format!("isTRIPLE({t})")), ""),
        p("SUBJECT", "1.2", expr(&format!("STR(SUBJECT({t}))")), ""),
        p(
            "PREDICATE",
            "1.2",
            expr(&format!("STR(PREDICATE({t}))")),
            "",
        ),
        p("OBJECT", "1.2", expr(&format!("STR(OBJECT({t}))")), ""),
        p(
            "LANGDIR",
            "1.2",
            expr(r#"LANGDIR("x"@en--ltr)"#),
            "base direction of a literal",
        ),
        p("hasLANG", "1.2", expr(r#"hasLANG("x"@en)"#), ""),
        p("hasLANGDIR", "1.2", expr(r#"hasLANGDIR("x"@en--ltr)"#), ""),
        p(
            "STRLANGDIR",
            "1.2",
            expr(r#"LANGDIR(STRLANGDIR("x", "en", "ltr"))"#),
            "",
        ),
        p(
            "triple term pattern",
            "1.2",
            over_data("?r rdf:reifies <<( ex:alice ex:name \"Alice\" )>>"),
            "matches a triple term in object position; zero rows here is correct",
        ),
        p(
            "VERSION",
            "1.2",
            format!("VERSION \"1.2\" {} SELECT * WHERE {{}}", prefixes()),
            "",
        ),
    ]
}

fn geosparql() -> Vec<Probe> {
    let wkt = r#""POINT(1 1)"^^geo:wktLiteral"#;
    let poly = r#""POLYGON((0 0,0 3,3 3,3 0,0 0))"^^geo:wktLiteral"#;
    // Cardiff Castle, on the British National Grid.
    let grid = r#""<http://www.opengis.net/def/crs/EPSG/0/27700> POINT(318086.06 176511.05)"^^geo:wktLiteral"#;
    let bng = r#""http://www.opengis.net/def/crs/EPSG/0/27700"^^xsd:anyURI"#;
    vec![
        p(
            "geof:distance",
            "GeoSPARQL",
            expr(&format!("geof:distance({wkt}, {wkt}, uom:metre)")),
            "",
        ),
        p(
            "geof:sfWithin",
            "GeoSPARQL",
            expr(&format!("geof:sfWithin({wkt}, {poly})")),
            "Simple Features",
        ),
        p(
            "geof:ehContains",
            "GeoSPARQL",
            expr(&format!("geof:ehContains({poly}, {wkt})")),
            "Egenhofer",
        ),
        p(
            "geof:rcc8ntpp",
            "GeoSPARQL",
            expr(&format!("geof:rcc8ntpp({wkt}, {poly})")),
            "RCC8",
        ),
        p(
            "geof:buffer",
            "GeoSPARQL",
            expr(&format!(
                "geof:area(geof:buffer({wkt}, 1, uom:metre), uom:square_metre)"
            )),
            "**added by HOLOS**",
        ),
        p(
            "geof:boundary",
            "GeoSPARQL",
            expr(&format!("STRLEN(STR(geof:boundary({poly})))")),
            "**added by HOLOS**",
        ),
        p(
            "geof:envelope",
            "GeoSPARQL",
            expr(&format!("STRLEN(STR(geof:envelope({poly})))")),
            "",
        ),
        p(
            "geof:convexHull",
            "GeoSPARQL",
            expr(&format!("STRLEN(STR(geof:convexHull({poly})))")),
            "",
        ),
        p(
            "geof:area",
            "GeoSPARQL",
            expr(&format!("geof:area({poly}, uom:square_metre)")),
            "",
        ),
        p(
            "geof:union",
            "GeoSPARQL",
            expr(&format!("STRLEN(STR(geof:union({wkt}, {poly})))")),
            "",
        ),
        p(
            "geof:asGeoJSON",
            "GeoSPARQL",
            expr(&format!("STRLEN(STR(geof:asGeoJSON({wkt})))")),
            "",
        ),
        p(
            "geof:getSRID",
            "GeoSPARQL",
            expr(&format!("geof:getSRID({grid})")),
            "**replaced**: reports the declared system, not CRS84",
        ),
        p(
            "reference systems",
            "GeoSPARQL",
            expr(&format!("geof:distance({grid}, {wkt}, uom:metre)")),
            "**added by HOLOS**: EPSG:4326, 27700 and 3857, not CRS84 alone",
        ),
        p(
            "holos:transform",
            "HOLOS",
            expr(&format!("STR(holos:transform({wkt}, {bng}))")),
            "**added by HOLOS**: GeoSPARQL has no transform",
        ),
    ]
}

fn extension_libraries() -> Vec<Probe> {
    let fnf = |local: &str, args: &str| {
        expr(&format!(
            "<http://www.w3.org/2005/xpath-functions#{local}>({args})"
        ))
    };
    let afn = |local: &str, args: &str| {
        expr(&format!(
            "<http://jena.apache.org/ARQ/function#{local}>({args})"
        ))
    };
    let spif =
        |local: &str, args: &str| expr(&format!("<http://spinrdf.org/spif#{local}>({args})"));
    vec![
        p("fn:upper-case", "F&O", fnf("upper-case", "\"ab\""), ""),
        p("fn:lower-case", "F&O", fnf("lower-case", "\"AB\""), ""),
        p(
            "fn:string-length",
            "F&O",
            fnf("string-length", "\"abc\""),
            "counts characters, not bytes",
        ),
        p(
            "fn:substring",
            "F&O",
            fnf("substring", "\"abcdef\", 2, 3"),
            "**1-based**, with a length",
        ),
        p(
            "fn:substring-before",
            "F&O",
            fnf("substring-before", "\"abc\", \"b\""),
            "",
        ),
        p(
            "fn:substring-after",
            "F&O",
            fnf("substring-after", "\"abc\", \"b\""),
            "",
        ),
        p("fn:contains", "F&O", fnf("contains", "\"abc\", \"b\""), ""),
        p(
            "fn:starts-with",
            "F&O",
            fnf("starts-with", "\"abc\", \"a\""),
            "",
        ),
        p(
            "fn:ends-with",
            "F&O",
            fnf("ends-with", "\"abc\", \"c\""),
            "",
        ),
        p("fn:concat", "F&O", fnf("concat", "\"a\", \"b\", \"c\""), ""),
        p(
            "fn:normalize-space",
            "F&O",
            fnf("normalize-space", "\"  a   b  \""),
            "",
        ),
        p(
            "fn:translate",
            "F&O",
            fnf("translate", "\"bar\", \"abc\", \"ABC\""),
            "unmapped characters are removed",
        ),
        p(
            "fn:compare",
            "F&O",
            fnf("compare", "\"a\", \"b\""),
            "-1, 0 or 1",
        ),
        p(
            "fn:ceiling",
            "F&O",
            fnf("ceiling", "1.2"),
            "also abs, floor, round",
        ),
        p("fn:not", "F&O", fnf("not", "true"), ""),
        p(
            "fn:boolean",
            "F&O",
            fnf("boolean", "\"\""),
            "XPath effective boolean value",
        ),
        p(
            "fn:year-from-dateTime",
            "F&O",
            fnf("year-from-dateTime", "NOW()"),
            "also month, day, hours, minutes, seconds",
        ),
        p(
            "afn:localname",
            "ARQ",
            afn("localname", "<http://e/x#name>"),
            "no SPARQL equivalent",
        ),
        p(
            "afn:namespace",
            "ARQ",
            afn("namespace", "<http://e/x#name>"),
            "",
        ),
        p(
            "afn:substr",
            "ARQ",
            afn("substr", "\"abcdef\", 0, 3"),
            "**0-based**, with an end index, unlike `fn:substring`",
        ),
        p(
            "afn:strjoin",
            "ARQ",
            afn("strjoin", "\"-\", \"a\", \"b\""),
            "",
        ),
        p(
            "afn:sprintf",
            "ARQ",
            afn("sprintf", "\"%s-%d\", \"a\", 1"),
            "`%s` and `%d` only",
        ),
        p("afn:sqrt", "ARQ", afn("sqrt", "16"), "also pi, e, min, max"),
        p("spif:trim", "SPIN", spif("trim", "\"  a  \""), ""),
        p(
            "spif:indexOf",
            "SPIN",
            spif("indexOf", "\"abc\", \"b\""),
            "character index, -1 when absent",
        ),
        p(
            "spif:lastIndexOf",
            "SPIN",
            spif("lastIndexOf", "\"abcb\", \"b\""),
            "",
        ),
        p(
            "spif:buildString",
            "SPIN",
            spif("buildString", "\"{?1}/{?2}\", \"a\", \"b\""),
            "numbered slots from 1",
        ),
        p(
            "spif:titleCase",
            "SPIN",
            spif("titleCase", "\"hello world\""),
            "",
        ),
        p(
            "spif:unCamelCase",
            "SPIN",
            spif("unCamelCase", "\"someName\""),
            "",
        ),
        p(
            "spif:upperCase",
            "SPIN",
            spif("upperCase", "\"ab\""),
            "also lowerCase",
        ),
        p(
            "spif:encodeURL",
            "SPIN",
            spif("encodeURL", "\"a b\""),
            "also decodeURL",
        ),
        p("spif:name", "SPIN", spif("name", "<http://e/x#n>"), ""),
    ]
}

fn keywords() -> Vec<Probe> {
    let q = prefixes();
    vec![
        p("SELECT", "16.1", over_data("?s ex:name ?n"), ""),
        p("SELECT *", "16.1", over_data("?s ex:name ?n"), ""),
        p("CONSTRUCT", "16.2", format!("{q} CONSTRUCT {{ ?s ex:name ?n }} WHERE {{ ?s ex:name ?n }}"), ""),
        p("ASK", "16.3", format!("{q} ASK {{ ?s ex:name ?n }}"), ""),
        p("DESCRIBE", "16.4", format!("{q} DESCRIBE ex:alice"), ""),
        p("BASE", "4.1.1", format!("BASE <{EX}> {q} SELECT * WHERE {{ ?s ?p ?o }} LIMIT 1"), ""),
        p("PREFIX", "4.1.1", over_data("?s ex:name ?n"), ""),
        p("`a` (rdf:type)", "4.2.2", over_data("?s a ex:Person"), ""),
        p("FROM", "13.2", format!("{q} SELECT * FROM <{EX}private> WHERE {{ ?s ?p ?o }}"), ""),
        p("FROM NAMED", "13.2", format!("{q} SELECT * FROM NAMED <{EX}private> WHERE {{ GRAPH ?g {{ ?s ?p ?o }} }}"), ""),
        p("GRAPH", "13.3", over_data("GRAPH ?g { ?s ?p ?o }"), ""),
        p("OPTIONAL", "6", over_data("?s ex:name ?n OPTIONAL { ?s ex:age ?a }"), ""),
        p("UNION", "7", over_data("{ ?s ex:name ?n } UNION { ?s ex:age ?n }"), ""),
        p("MINUS", "8.2", over_data("?s ex:name ?n MINUS { ?s ex:age ?a }"), ""),
        p("FILTER", "5.2", over_data(r#"?s ex:name ?n FILTER(?n = "Alice")"#), ""),
        p("BIND", "5.2.2", over_data(r#"?s ex:name ?n BIND(UCASE(?n) AS ?u)"#), ""),
        p("VALUES", "10.2.1", format!("{q} SELECT * WHERE {{ VALUES ?n {{ \"Alice\" }} ?s ex:name ?n }}"), ""),
        federated(p("SERVICE", "Federated Query", format!("{q} SELECT * WHERE {{ SERVICE <{EX}remote> {{ ?s ?p ?o }} }}"), "against a registered endpoint; remote HTTP is **not** enabled — see the note below")),
        federated(p("SERVICE SILENT", "Federated Query", format!("{q} SELECT * WHERE {{ OPTIONAL {{ SERVICE SILENT <{EX}absent> {{ ?s ?p ?o }} }} }}"), "an unreachable endpoint yields no bindings rather than an error")),
        p("ORDER BY", "15.1", format!("{q} SELECT * WHERE {{ ?s ex:name ?n }} ORDER BY ?n"), ""),
        p("ORDER BY DESC", "15.1", format!("{q} SELECT * WHERE {{ ?s ex:name ?n }} ORDER BY DESC(?n)"), ""),
        p("LIMIT", "15.5", format!("{q} SELECT * WHERE {{ ?s ex:name ?n }} LIMIT 1"), ""),
        p("OFFSET", "15.4", format!("{q} SELECT * WHERE {{ ?s ex:name ?n }} OFFSET 1"), ""),
        p("DISTINCT", "15.3.1", format!("{q} SELECT DISTINCT ?n WHERE {{ ?s ex:name ?n }}"), ""),
        p("REDUCED", "15.3.2", format!("{q} SELECT REDUCED ?n WHERE {{ ?s ex:name ?n }}"), ""),
        p("GROUP BY", "18.5", format!("{q} SELECT ?s WHERE {{ ?s ex:name ?n }} GROUP BY ?s"), ""),
        p("HAVING", "18.6", format!("{q} SELECT ?s WHERE {{ ?s ex:name ?n }} GROUP BY ?s HAVING(COUNT(?n) > 0)"), ""),
        p("AS (expression)", "16.1.1", format!("{q} SELECT (UCASE(?n) AS ?u) WHERE {{ ?s ex:name ?n }}"), ""),
        p("subquery", "12", over_data("{ SELECT ?s WHERE { ?s ex:name ?n } LIMIT 1 }"), ""),
    ]
}

fn property_paths() -> Vec<Probe> {
    vec![
        p(
            "iri (one hop)",
            "9.1",
            over_data("ex:alice ex:knows ?o"),
            "",
        ),
        p("^ (inverse)", "9.1", over_data("?s ^ex:knows ex:bob"), ""),
        p(
            "/ (sequence)",
            "9.1",
            over_data("ex:alice ex:knows/ex:name ?n"),
            "",
        ),
        p(
            "| (alternative)",
            "9.1",
            over_data("ex:alice ex:knows|ex:name ?o"),
            "",
        ),
        p(
            "* (zero or more)",
            "9.1",
            over_data("ex:alice ex:knows* ?o"),
            "",
        ),
        p(
            "+ (one or more)",
            "9.1",
            over_data("ex:alice ex:knows+ ?o"),
            "",
        ),
        p(
            "? (zero or one)",
            "9.1",
            over_data("ex:alice ex:knows? ?o"),
            "",
        ),
        p(
            "! (negated set)",
            "9.1",
            over_data("ex:alice !(ex:knows) ?o"),
            "",
        ),
        p(
            "() (grouping)",
            "9.1",
            over_data("ex:alice (ex:knows/ex:knows) ?o"),
            "",
        ),
    ]
}

fn section(title: &str, spec: &str, probes: Vec<Probe>, engine: &Engine, session: &Session) {
    println!("\n### {title}\n");
    println!("| {spec} | Spec | Status | Notes |");
    println!("|---|---|---|---|");
    for entry in probes {
        let (status, note) = match probe(engine, session, &entry) {
            Ok(_) => ("✅ yes".to_owned(), entry.note.to_owned()),
            Err(reason) => {
                let combined = if entry.note.is_empty() {
                    reason.clone()
                } else {
                    format!("{} — {reason}", entry.note)
                };
                ("❌ no".to_owned(), combined)
            }
        };
        println!(
            "| `{}` | {} | {} | {} |",
            entry.name, entry.section, status, note
        );
    }
}

fn main() {
    let engine = store();
    let session = Session::unrestricted(engine.store()).expect("session");

    println!("<!-- Generated by `cargo run --release -p holos-engine --example surface`.");
    println!("     Every row was produced by running the query against a live store; do not");
    println!("     edit by hand, regenerate. -->");
    println!("# SPARQL surface");
    println!(
        "\nEvery function and keyword below was **probed against a running store** by\n\
         `crates/holos-engine/examples/surface.rs`, and the status column records what\n\
         actually happened. Where something is unsupported, the note is the engine's own\n\
         error rather than an editorial summary of it.\n"
    );
    println!("Regenerate with:\n");
    println!("```sh");
    println!("cargo run --release -p holos-engine --example surface > SPARQL-SURFACE.md");
    println!("```");

    println!(
        "\n## How to read this\n\n\
         | | |\n|---|---|\n\
         | yes | the probe evaluated and returned a result |\n\
         | no | the probe failed; the note carries the engine's own message |\n\n\
         A yes means *this build evaluates it*, not that every edge case of the \
         specification is covered. The conformance suites in `README.md` are the measure \
         of that: SPARQL 1.1 at 476/477 of what runs, SPARQL 1.2 at 262/266.\n"
    );

    println!(
        "\n> **SPARQL Update** has its own keywords: `INSERT DATA`, `DELETE DATA`,\n\
         > `DELETE/INSERT ... WHERE`, `DELETE WHERE`, `LOAD`, `CLEAR`, `CREATE`, `DROP`,\n\
         > and `SILENT` on each. All are implemented, and all 94 W3C\n\
         > `UpdateEvaluationTest`s pass. They are not probed here because a probe would\n\
         > mutate the store the other probes read. See `OPERATIONS.md`.\n"
    );

    println!(
        "\n> **Federation.** `SERVICE` evaluates against endpoints registered in-process.\n\
         > **Calling a remote endpoint over HTTP is deliberately not enabled**: a `SERVICE`\n\
         > IRI in a user's query would make the server issue a request to a host of the\n\
         > user's choosing, reaching cloud metadata at `169.254.169.254`, internal admin\n\
         > interfaces, and anything else routable from the server but not from the user.\n\
         > Remote `LOAD` is refused for the same reason. Enabling it needs an allow-list,\n\
         > which is a policy decision; see `ACCESS-CONTROL.md`.\n\
         >\n\
         > Results from another service have **not** passed through the policy chokepoint,\n\
         > so the guarantee in `ACCESS-CONTROL.md` covers the local contribution only.\n"
    );

    println!("\n---\n\n## Keywords");
    section(
        "Query forms and prologue",
        "Keyword",
        keywords(),
        &engine,
        &session,
    );
    section(
        "Property paths",
        "Operator",
        property_paths(),
        &engine,
        &session,
    );

    println!("\n---\n\n## Functions");
    section(
        "Functional forms",
        "Form",
        functional_forms(),
        &engine,
        &session,
    );
    section(
        "On RDF terms",
        "Function",
        term_functions(),
        &engine,
        &session,
    );
    section(
        "On strings",
        "Function",
        string_functions(),
        &engine,
        &session,
    );
    section(
        "On numerics",
        "Function",
        numeric_functions(),
        &engine,
        &session,
    );
    section(
        "On dates and times",
        "Function",
        datetime_functions(),
        &engine,
        &session,
    );
    section(
        "Hash functions",
        "Function",
        hash_functions(),
        &engine,
        &session,
    );
    section("Aggregates", "Aggregate", aggregates(), &engine, &session);
    section("Constructor casts", "Cast", casts(), &engine, &session);
    section(
        "SPARQL 1.2 additions",
        "Function",
        sparql12(),
        &engine,
        &session,
    );
    section(
        "GeoSPARQL (sample of 45, plus holos:transform)",
        "Function",
        geosparql(),
        &engine,
        &session,
    );
    section(
        "Extension libraries",
        "Function",
        extension_libraries(),
        &engine,
        &session,
    );

    println!("\n#### Deliberately not implemented\n");
    println!("| Function | Why |");
    println!("|---|---|");
    for (name, why) in holos_engine::functions::unsupported() {
        println!("| `{name}` | {why} |");
    }

    println!(
        "\nAn unregistered function is **not** a parse error: it parses, and fails at\n\
         evaluation. Adding more is small, because they are ordinary\n\
         `fn(&[Term]) -> Option<Term>` entries on the evaluator, which is exactly how\n\
         `geof:buffer`, `geof:boundary` and `holos:transform` were added.\n\n\
         For `fn:`, the SPARQL built-ins cover the same ground under different names:\n\
         `UCASE` for `fn:upper-case`, `REPLACE` for `fn:replace`, `REGEX` for\n\
         `fn:matches`, `STRLEN` for `fn:string-length`.\n"
    );

    println!(
        "\n---\n\n## The complete GeoSPARQL set\n\n\
         All 45 `geof:` functions are registered — 43 from `spargeo`, plus `geof:buffer`\n\
         and `geof:boundary` implemented here — and `holos:transform` alongside them.\n\
         Six of the 43 are **replaced** rather than reused: `distance`, `getSRID` and the\n\
         four set operations, where `spargeo`'s answer was narrower than the\n\
         specification's. The table above samples them; the full list is:\n"
    );
    let local = |iri: &str| iri.rsplit(['/', '#']).next().unwrap_or("").to_owned();
    let ours: BTreeSet<String> = holos_engine::geo_ext::function_iris()
        .iter()
        .map(|iri| local(iri.as_str()))
        .collect();
    let mut names: BTreeSet<String> = spargeo::GEOSPARQL_EXTENSION_FUNCTIONS
        .iter()
        .map(|(iri, _)| local(iri.as_str()))
        .collect();
    names.extend(ours.iter().cloned());
    let listed: Vec<String> = names
        .iter()
        .map(|name| {
            if ours.contains(name) {
                format!("**{name}**")
            } else {
                name.clone()
            }
        })
        .collect();
    println!("{}\n", listed.join(" - "));
    println!(
        "Names in **bold** are implemented or replaced by HOLOS. `transform` is in the\n\
         `holos:` namespace rather than `geof:`, because GeoSPARQL defines no transform\n\
         function and putting one in the OGC namespace would claim a sanction it does not\n\
         have.\n\n\
         Every one of them reads geometry literals in **CRS84, EPSG:4326, EPSG:27700 and\n\
         EPSG:3857**, converting to CRS84 on the way in, so data published on the British\n\
         National Grid can be queried against data in degrees. An unrecognised reference\n\
         system is refused rather than assumed to be CRS84. See `crates/holos-engine/src/crs.rs`\n\
         for the transformations and the accuracy they are good for."
    );
}
