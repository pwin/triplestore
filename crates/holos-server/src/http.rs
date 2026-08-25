//! The small amount of HTTP plumbing the SPARQL Protocol needs.
//!
//! Query-string and form parsing, percent-decoding, and content negotiation. Deliberately
//! hand-rolled and small: the SPARQL Protocol is a short specification, and pulling a web
//! framework in to serve four endpoints would trade a page of code for a dependency tree.

use sparesults::QueryResultsFormat;
use std::collections::HashMap;

/// Splits a URL into its path and decoded query parameters.
#[must_use]
pub fn split_url(url: &str) -> (String, HashMap<String, String>) {
    match url.split_once('?') {
        None => (url.to_owned(), HashMap::new()),
        Some((path, query)) => (path.to_owned(), parse_form(query)),
    }
}

/// Parameters the SPARQL Protocol says may appear more than once.
///
/// Everything else keeps the first value, matching the protocol's rule that a request with
/// two `query` parameters is malformed rather than a request with a list. These four are
/// different: they build a dataset, and taking only the first would answer over a smaller
/// one than the client asked for — silently, which is the worst way to be wrong.
const REPEATABLE: [&str; 4] = [
    "default-graph-uri",
    "named-graph-uri",
    "using-graph-uri",
    "using-named-graph-uri",
];

/// Parses `application/x-www-form-urlencoded` into its parameters.
///
/// Repeated values of a [`REPEATABLE`] key are joined with a newline, which cannot occur
/// inside a percent-decoded IRI; use [`values`] to read them back as a list.
#[must_use]
pub fn parse_form(body: &str) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    for pair in body.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let (key, value) = (decode(key), decode(value));
        if REPEATABLE.contains(&key.as_str()) {
            out.entry(key)
                .and_modify(|existing| {
                    existing.push('\n');
                    existing.push_str(&value);
                })
                .or_insert(value);
        } else {
            out.entry(key).or_insert(value);
        }
    }
    out
}

/// Whether a parameter is given more than once in an encoded parameter string.
///
/// [`parse_form`] keeps the first value of a non-repeatable key, so by the time a request
/// has become a map the duplicate is gone. The protocol makes two `query` parameters a
/// client error rather than a list, so the check has to happen on the text.
#[must_use]
pub fn given_more_than_once(encoded: &str, key: &str) -> bool {
    encoded
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let name = pair.split_once('=').map_or(*pair, |(name, _)| name);
            decode(name) == key
        })
        .count()
        > 1
}

/// Every value given for a repeatable parameter.
#[must_use]
pub fn values(params: &HashMap<String, String>, key: &str) -> Vec<String> {
    params
        .get(key)
        .map(|v| {
            v.split('\n')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Percent-decoding, with `+` meaning space as form encoding requires.
#[must_use]
pub fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Picks a SPARQL results format from an `Accept` header.
///
/// Quality values are parsed but the choice is a simple best-match: this is a console and
/// a protocol endpoint, not a content-negotiation showcase, and every real SPARQL client
/// sends an unambiguous `Accept`.
#[must_use]
pub fn negotiate_results(accept: Option<&str>) -> QueryResultsFormat {
    let Some(accept) = accept else {
        return QueryResultsFormat::Json;
    };
    let mut best = (QueryResultsFormat::Json, -1.0_f32);
    for part in accept.split(',') {
        let (media, q) = quality(part.trim());
        let format = match media {
            "application/sparql-results+json" | "application/json" => QueryResultsFormat::Json,
            "application/sparql-results+xml" | "application/xml" | "text/xml" => {
                QueryResultsFormat::Xml
            }
            "text/csv" => QueryResultsFormat::Csv,
            "text/tab-separated-values" => QueryResultsFormat::Tsv,
            // A client that will take anything gets JSON, which every SPARQL console reads.
            "*/*" => QueryResultsFormat::Json,
            _ => continue,
        };
        if q > best.1 {
            best = (format, q);
        }
    }
    best.0
}

/// Picks an RDF format from an `Accept` header, for `CONSTRUCT` and the Graph Store.
#[must_use]
pub fn negotiate_rdf(accept: Option<&str>) -> oxrdfio::RdfFormat {
    use oxrdfio::RdfFormat;
    let Some(accept) = accept else {
        return RdfFormat::Turtle;
    };
    let mut best = (RdfFormat::Turtle, -1.0_f32);
    for part in accept.split(',') {
        let (media, q) = quality(part.trim());
        let format = match media {
            "text/turtle" | "application/x-turtle" => RdfFormat::Turtle,
            "application/n-triples" => RdfFormat::NTriples,
            "application/n-quads" => RdfFormat::NQuads,
            "application/trig" => RdfFormat::TriG,
            "application/rdf+xml" => RdfFormat::RdfXml,
            "application/ld+json" => RdfFormat::JsonLd {
                profile: oxrdfio::JsonLdProfileSet::empty(),
            },
            "*/*" => RdfFormat::Turtle,
            _ => continue,
        };
        if q > best.1 {
            best = (format, q);
        }
    }
    best.0
}

/// Splits `type/subtype;q=0.8` into the media type and its quality.
fn quality(part: &str) -> (&str, f32) {
    match part.split_once(';') {
        None => (part, 1.0),
        Some((media, params)) => {
            let q = params
                .split(';')
                .filter_map(|p| p.trim().strip_prefix("q="))
                .find_map(|v| v.parse::<f32>().ok())
                .unwrap_or(1.0);
            (media.trim(), q)
        }
    }
}

/// The media type a results format is served as.
#[must_use]
pub fn results_media_type(format: QueryResultsFormat) -> &'static str {
    match format {
        QueryResultsFormat::Json => "application/sparql-results+json",
        QueryResultsFormat::Xml => "application/sparql-results+xml",
        QueryResultsFormat::Csv => "text/csv",
        QueryResultsFormat::Tsv => "text/tab-separated-values",
        // `QueryResultsFormat` is non-exhaustive upstream; a format this build does not
        // know is served as JSON rather than refusing the request.
        _ => "application/sparql-results+json",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_split_into_path_and_parameters() {
        let (path, params) = split_url("/query?query=SELECT%20*%20WHERE%7B%3Fs%20%3Fp%20%3Fo%7D");
        assert_eq!(path, "/query");
        assert_eq!(params.get("query").unwrap(), "SELECT * WHERE{?s ?p ?o}");
    }

    #[test]
    fn form_bodies_decode_plus_as_space() {
        let params = parse_form("query=SELECT+*&default-graph-uri=urn%3Ag");
        assert_eq!(params.get("query").unwrap(), "SELECT *");
        assert_eq!(params.get("default-graph-uri").unwrap(), "urn:g");
    }

    #[test]
    fn repeatable_dataset_parameters_accumulate() {
        // The protocol allows several of these, and each one adds a graph. Keeping only
        // the first would answer over a smaller dataset than was asked for.
        let params = parse_form("default-graph-uri=urn%3Aa&default-graph-uri=urn%3Ab&query=X");
        assert_eq!(values(&params, "default-graph-uri"), vec!["urn:a", "urn:b"]);
        assert_eq!(params.get("query").unwrap(), "X");
    }

    #[test]
    fn an_absent_repeatable_parameter_is_an_empty_list() {
        assert!(values(&parse_form("query=X"), "default-graph-uri").is_empty());
    }

    #[test]
    fn a_repeated_parameter_keeps_the_first() {
        // The SPARQL Protocol calls two `query` parameters a malformed request; keeping
        // the first is at least deterministic about which one it answered.
        let params = parse_form("query=A&query=B");
        assert_eq!(params.get("query").unwrap(), "A");
    }

    #[test]
    fn accept_headers_select_a_format() {
        assert_eq!(
            negotiate_results(Some("text/csv")),
            QueryResultsFormat::Csv
        );
        assert_eq!(
            negotiate_results(Some("application/sparql-results+xml")),
            QueryResultsFormat::Xml
        );
        // Quality values decide between two acceptable types.
        assert_eq!(
            negotiate_results(Some("text/csv;q=0.5, application/sparql-results+json;q=0.9")),
            QueryResultsFormat::Json
        );
        // An unknown type falls back rather than failing the request.
        assert_eq!(
            negotiate_results(Some("application/pdf")),
            QueryResultsFormat::Json
        );
        assert_eq!(negotiate_results(None), QueryResultsFormat::Json);
    }

    #[test]
    fn rdf_accept_headers_select_a_format() {
        assert_eq!(
            negotiate_rdf(Some("application/n-triples")),
            oxrdfio::RdfFormat::NTriples
        );
        assert_eq!(negotiate_rdf(Some("*/*")), oxrdfio::RdfFormat::Turtle);
    }

    #[test]
    fn a_truncated_escape_is_not_a_panic() {
        // Hostile input reaches this before anything else does.
        assert_eq!(decode("%"), "%");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("a%2"), "a%2");
    }

    #[test]
    fn a_duplicated_parameter_is_visible_in_the_text() {
        // The protocol makes two `query` parameters a client error. `parse_form` keeps the
        // first and drops the rest, so by the time a request is a map the duplicate is
        // gone — which is why this reads the encoded text instead.
        assert!(given_more_than_once("query=ASK%20%7B%7D&query=SELECT%20%2A%20%7B%7D", "query"));
        assert!(!given_more_than_once("query=ASK%20%7B%7D&default-graph-uri=x", "query"));
        assert!(!given_more_than_once("", "query"));
    }

    #[test]
    fn a_parameter_whose_name_is_encoded_still_counts() {
        // `%71uery` is `query`. A client that encodes the name is not thereby allowed two.
        assert!(given_more_than_once("query=a&%71uery=b", "query"));
    }

    #[test]
    fn a_repeatable_parameter_may_of_course_repeat() {
        // The dataset parameters are the exception, and are joined rather than dropped.
        let params = parse_form("default-graph-uri=http%3A%2F%2Fa&default-graph-uri=http%3A%2F%2Fb");
        assert_eq!(values(&params, "default-graph-uri"), ["http://a", "http://b"]);
    }
}
