//! The bind join, checked against the evaluator it bypasses.
//!
//! A fast path is only worth having if it cannot change an answer, and this one takes over
//! whole queries. So every test here runs the same SPARQL twice — once through the fast path
//! and once through `spareval` — and compares. That is the only assertion that matters;
//! everything else is a way of generating cases for it.
//!
//! The comparison is exact and order-insensitive: same rows, same bindings, same
//! multiplicity. A bind join that silently deduplicated, or dropped an unbound column, would
//! pass a weaker check.

use holos_engine::{Engine, QueryOptions};
use holos_security::Session;
use holos_stats::Statistics;
use holos_store::GraphFilter;
use oxrdfio::RdfFormat;
use spareval::QueryResults;
use std::sync::Arc;

const EX: &str = "http://example.com/";

/// People in departments, with enough shape for stars, chains and shared objects.
fn data() -> String {
    let mut turtle = format!("@prefix ex: <{EX}> .\n");
    for i in 0..60 {
        let dept = i % 5;
        turtle.push_str(&format!(
            "ex:p{i} ex:name \"person {i}\" ; ex:badge {i} ; ex:memberOf ex:d{dept} .\n"
        ));
        if i % 3 == 0 {
            turtle.push_str(&format!("ex:p{i} ex:nickname \"nick {i}\" .\n"));
        }
        if i % 7 == 0 {
            turtle.push_str(&format!("ex:p{i} ex:knows ex:p{} .\n", (i + 1) % 60));
        }
    }
    for d in 0..5 {
        turtle.push_str(&format!(
            "ex:d{d} ex:label \"dept {d}\" ; ex:region ex:r{} .\n",
            d % 2
        ));
    }
    turtle
}

fn engine() -> Engine {
    let mut engine = Engine::new();
    engine
        .bulk_load(data().as_bytes(), RdfFormat::Turtle, None)
        .expect("load");
    engine
}

/// Rows as comparable strings, sorted so ordering differences are not mistaken for
/// disagreement — neither path promises an order without `ORDER BY`.
fn rows(engine: &Engine, query: &str, options: &QueryOptions) -> Vec<String> {
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let (results, _) = Engine::query_with(&view, query, options).expect("query");
    let mut out: Vec<String> = match results {
        QueryResults::Solutions(iter) => iter
            .map(|s| {
                let s = s.expect("solution");
                let mut cells: Vec<String> = s
                    .iter()
                    .map(|(v, t)| format!("{}={t}", v.as_str()))
                    .collect();
                cells.sort();
                cells.join(" ")
            })
            .collect(),
        _ => panic!("expected solutions"),
    };
    out.sort();
    out
}

/// The same query both ways.
///
/// `spareval` is reached by asking for an explanation, which the fast path declines to
/// produce — a narrow but reliable way to force the slow path without a test-only switch in
/// production code.
fn both(engine: &Engine, query: &str) -> (Vec<String>, Vec<String>) {
    let stats =
        Arc::new(Statistics::build(engine.store(), GraphFilter::Default).expect("statistics"));
    let fast = rows(
        engine,
        query,
        &QueryOptions::new().reordering(Arc::clone(&stats)),
    );
    let slow = rows(engine, query, &QueryOptions::new().explaining());
    (fast, slow)
}

const P: &str = "PREFIX ex: <http://example.com/>";

#[test]
fn a_star_query_agrees() {
    let engine = engine();
    let (fast, slow) = both(
        &engine,
        &format!("{P} SELECT ?n ?d WHERE {{ ?s ex:name ?n . ?s ex:memberOf ?d }}"),
    );
    assert_eq!(fast, slow);
    assert_eq!(fast.len(), 60);
}

#[test]
fn a_chain_query_agrees() {
    let engine = engine();
    let (fast, slow) = both(
        &engine,
        &format!(
            "{P} SELECT ?n ?label WHERE {{ ?s ex:name ?n . ?s ex:memberOf ?d . ?d ex:label ?label }}"
        ),
    );
    assert_eq!(fast, slow);
    assert!(!fast.is_empty());
}

#[test]
fn a_constant_in_the_pattern_agrees() {
    let engine = engine();
    let (fast, slow) = both(
        &engine,
        &format!("{P} SELECT ?n WHERE {{ ?s ex:memberOf ex:d2 . ?s ex:name ?n }}"),
    );
    assert_eq!(fast, slow);
    assert_eq!(fast.len(), 12, "60 people over 5 departments");
}

#[test]
fn a_predicate_the_store_has_never_seen_agrees() {
    // The dead-branch case: a constant absent from the dictionary. Both paths must return
    // nothing, and the fast one should not have to scan to find that out.
    let engine = engine();
    let (fast, slow) = both(
        &engine,
        &format!("{P} SELECT ?n WHERE {{ ?s ex:noSuchPredicate ?n }}"),
    );
    assert_eq!(fast, slow);
    assert!(fast.is_empty());
}

#[test]
fn limit_and_offset_agree() {
    let engine = engine();
    for (limit, offset) in [(5, 0), (5, 10), (1, 59), (100, 0)] {
        let query =
            format!("{P} SELECT ?n WHERE {{ ?s ex:name ?n }} LIMIT {limit} OFFSET {offset}");
        let (fast, slow) = both(&engine, &query);
        assert_eq!(
            fast.len(),
            slow.len(),
            "LIMIT {limit} OFFSET {offset} row count"
        );
    }
}

#[test]
fn distinct_agrees() {
    let engine = engine();
    // Departments have many members, so without DISTINCT this repeats.
    let (fast, slow) = both(
        &engine,
        &format!("{P} SELECT DISTINCT ?d WHERE {{ ?s ex:memberOf ?d }}"),
    );
    assert_eq!(fast, slow);
    assert_eq!(fast.len(), 5);
}

#[test]
fn duplicates_are_preserved_without_distinct() {
    // A bind join that deduplicated by accident would pass every test that used DISTINCT.
    let engine = engine();
    let (fast, slow) = both(
        &engine,
        &format!("{P} SELECT ?d WHERE {{ ?s ex:memberOf ?d }}"),
    );
    assert_eq!(fast, slow);
    assert_eq!(fast.len(), 60, "one row per member, not one per department");
}

#[test]
fn a_repeated_variable_within_a_pattern_agrees() {
    // `?s ?p ?s` must only match where subject and object are the same term. Getting this
    // wrong produces extra rows, which a superset check would miss.
    let engine = engine();
    let (fast, slow) = both(&engine, &format!("{P} SELECT ?s WHERE {{ ?s ?p ?s }}"));
    assert_eq!(fast, slow);
}

#[test]
fn a_variable_predicate_agrees() {
    let engine = engine();
    let (fast, slow) = both(&engine, &format!("{P} SELECT ?p WHERE {{ ex:p3 ?p ?o }}"));
    assert_eq!(fast, slow);
    assert!(!fast.is_empty());
}

#[test]
fn shapes_outside_the_fragment_still_work() {
    // Each of these must be refused by `plan` and answered by the evaluator. What is checked
    // is that the fast path's presence did not break them.
    //
    // `FILTER` used to be on this list and is not any more; the refusal is asserted rather
    // than described, so that moving something into the fragment cannot leave a comment here
    // quietly claiming it is still outside.
    let engine = engine();
    for query in [
        format!("{P} SELECT ?n WHERE {{ ?s ex:name ?n OPTIONAL {{ ?s ex:nickname ?k }} }}"),
        format!("{P} SELECT ?n WHERE {{ {{ ?s ex:name ?n }} UNION {{ ?s ex:label ?n }} }}"),
        format!("{P} SELECT ?n WHERE {{ ?s ex:name ?n }} ORDER BY ?n LIMIT 3"),
    ] {
        let stats =
            Arc::new(Statistics::build(engine.store(), GraphFilter::Default).expect("statistics"));
        let fetched = rows(
            &engine,
            &query,
            &QueryOptions::new().reordering(Arc::clone(&stats)),
        );
        let reference = rows(&engine, &query, &QueryOptions::new().explaining());
        assert_eq!(fetched, reference, "disagreement on {query}");

        let parsed = spargebra::SparqlParser::new()
            .parse_query(&query)
            .expect("parse");
        assert!(
            holos_engine::bindjoin::plan(&parsed).is_none(),
            "this is supposed to be outside the fragment: {query}"
        );
    }
}

// ------------------------------------------------------------------- giving up

/// The plan for `query`, or a panic — these tests are about what evaluation does, so a
/// query falling outside the fragment is a broken test rather than a result.
fn planned(query: &str) -> holos_engine::bindjoin::Plan {
    let parsed = spargebra::SparqlParser::new()
        .parse_query(query)
        .expect("parse");
    holos_engine::bindjoin::plan(&parsed).expect("inside the fragment")
}

#[test]
fn exceeding_the_budget_defers_rather_than_truncates() {
    // The distinction the return type carries: `None` means *ask the evaluator*, and never
    // *these are all the rows*. A path that truncated at its budget would turn a large
    // correct answer into a small wrong one with nothing to show it had happened.
    use holos_engine::bindjoin::Limits;

    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let plan = planned(&format!("{P} SELECT ?n WHERE {{ ?s ex:name ?n }}"));

    let tight = plan
        .evaluate(
            &view,
            None,
            Limits {
                rows: Some(5),
                token: None,
            },
        )
        .expect("evaluate");
    assert!(
        tight.is_none(),
        "a budget of 5 against 60 rows must defer, not return 5"
    );

    let whole = plan
        .evaluate(&view, None, Limits::default())
        .expect("evaluate")
        .expect("no limits, so an answer");
    assert_eq!(whole.len(), 60, "the same plan, unbudgeted, still answers");
}

#[test]
fn a_cancelled_token_stops_the_fast_path() {
    // Skipping the evaluator means skipping the evaluator's cancellation checks, so this
    // path has to make its own. Without them a timeout is not a timeout: the query runs to
    // completion and the deadline is noticed afterwards, if at all.
    use holos_engine::bindjoin::Limits;
    use spareval::CancellationToken;

    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    // A cross product, so the scan is long enough to reach a token check.
    let plan = planned("SELECT * WHERE { ?a ?b ?c . ?d ?e ?f }");

    let token = CancellationToken::new();
    token.cancel();
    let stopped = plan
        .evaluate(
            &view,
            None,
            Limits {
                rows: None,
                token: Some(&token),
            },
        )
        .expect("evaluate");
    assert!(
        stopped.is_none(),
        "a cancelled token must stop this path, not merely be ignored by it"
    );
}

#[test]
fn a_substitution_is_never_answered_from_the_fast_path() {
    // The regression the W3C run caught. `QueryOptions::substitutions` binds a variable
    // without touching the query text, and the fast path does not model it — so a query
    // carrying one must be refused outright. Answering it here returns the rows for a
    // *different* query: the one without the binding.
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);

    // `?d` is projected because `spareval` only substitutes variables the query returns.
    let query = format!("{P} SELECT ?s ?d WHERE {{ ?s ex:memberOf ?d }}");
    let options = QueryOptions::new().with_substitution(
        oxrdf::Variable::new_unchecked("d"),
        oxrdf::Term::NamedNode(oxrdf::NamedNode::new_unchecked(format!("{EX}nosuchdept"))),
    );
    let (results, _) = Engine::query_with(&view, &query, &options).expect("query");
    match results {
        QueryResults::Solutions(iter) => assert_eq!(
            iter.count(),
            0,
            "the substitution was dropped and the unbound query answered instead"
        ),
        _ => panic!("expected solutions"),
    }
}

#[test]
fn a_from_clause_is_never_answered_from_the_fast_path() {
    // `FROM` lives in `Query::Select::dataset`, beside the pattern rather than inside it, so
    // a check that only reads the pattern sees an answerable query and answers a different
    // one — over the store's default graph instead of the graph the query named. This
    // returned the wrong rows rather than slow ones, which is the failure mode the whole
    // fragment exists to prevent.
    let mut engine = Engine::new();
    engine
        .bulk_load(
            format!("<{EX}indefault> <{EX}p> <{EX}o> .").as_bytes(),
            RdfFormat::NTriples,
            None,
        )
        .expect("load default");
    engine
        .bulk_load_into_graph(
            format!("<{EX}ing1> <{EX}p> <{EX}o> .").as_bytes(),
            RdfFormat::NTriples,
            None,
            &oxrdf::GraphName::NamedNode(oxrdf::NamedNode::new_unchecked(format!("{EX}g1"))),
        )
        .expect("load g1");

    let query = format!("SELECT ?s FROM <{EX}g1> WHERE {{ ?s ?p ?o }}");
    let (fast, slow) = both(&engine, &query);
    assert_eq!(fast, slow, "FROM changed the answer between the two paths");
    assert_eq!(
        fast,
        vec![format!("s=<{EX}ing1>")],
        "FROM <g1> must answer over g1, not over the default graph"
    );
}

#[test]
fn distinct_outside_a_slice_is_refused_rather_than_flattened() {
    // The wrappers are peeled into flags, and the flags are applied in one fixed order:
    // project, distinct, offset, limit. That is `Slice(Distinct(Project(..)))`. The reverse
    // nesting means `DISTINCT` after `LIMIT`, which is a different query, so it must be
    // refused rather than flattened into the same flags.
    use spargebra::algebra::GraphPattern;

    let inner = spargebra::SparqlParser::new()
        .parse_query(&format!("{P} SELECT ?n WHERE {{ ?s ex:name ?n }} LIMIT 5"))
        .expect("parse");
    let spargebra::Query::Select { pattern, .. } = inner else {
        panic!("expected a SELECT");
    };
    // `Distinct { Slice { Project { .. } } }` — not something the grammar can produce, which
    // is exactly why the guard has to be in the code rather than in the assumption.
    let inverted = spargebra::Query::Select {
        dataset: None,
        pattern: GraphPattern::Distinct {
            inner: Box::new(pattern),
        },
        base_iri: None,
    };
    assert!(
        holos_engine::bindjoin::plan(&inverted).is_none(),
        "DISTINCT wrapping a slice must be refused"
    );
}

#[test]
fn the_budget_counts_rows_held_by_distinct_too() {
    // `OFFSET` grows `seen` without growing `out`, so a budget that only watched `out` would
    // let a `DISTINCT` query retain every row of a cross product while reporting that it had
    // spent nothing. Same family as the 13.7 GB cross product, one structure over.
    use holos_engine::bindjoin::Limits;

    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    // 60 distinct rows, all consumed by the offset: `out` never grows past zero.
    let plan = planned(&format!(
        "{P} SELECT DISTINCT ?s ?n WHERE {{ ?s ex:name ?n }} OFFSET 1000"
    ));

    let outcome = plan
        .evaluate(
            &view,
            None,
            Limits {
                rows: Some(5),
                token: None,
            },
        )
        .expect("evaluate");
    assert!(
        outcome.is_none(),
        "60 rows were retained under a budget of 5, and the budget did not notice"
    );
}

#[test]
fn every_entry_point_gives_the_same_answer() {
    // The invariant that was missing, and the reason three bugs survived seventeen tests.
    //
    // There are three ways into evaluation, and they are reached by different callers:
    // `query_with` by the HTTP server, `query` by the Python binding and the audited CLI
    // path, `query_prepared_with_services` by the W3C conformance runner. The fast path was
    // attached to the first only — so the suites that appeared to be its main coverage ran
    // through a function that never called it, and the most-used surface never got it.
    //
    // Comparing the three is what makes "covered by the conformance suites" mean something.
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    let query = format!("{P} SELECT ?n ?d WHERE {{ ?s ex:name ?n . ?s ex:memberOf ?d }}");

    let collect = |results: QueryResults<'_>| -> Vec<String> {
        let mut out: Vec<String> = match results {
            QueryResults::Solutions(iter) => iter
                .map(|s| {
                    let s = s.expect("solution");
                    let mut cells: Vec<String> = s
                        .iter()
                        .map(|(v, t)| format!("{}={t}", v.as_str()))
                        .collect();
                    cells.sort();
                    cells.join(" ")
                })
                .collect(),
            _ => panic!("expected solutions"),
        };
        out.sort();
        out
    };

    let with_options = {
        let (results, _) =
            Engine::query_with(&view, &query, &QueryOptions::new()).expect("query_with");
        collect(results)
    };
    let plain = collect(Engine::query(&view, &query, None).expect("query"));
    let prepared = {
        let parsed = spargebra::SparqlParser::new()
            .parse_query(&query)
            .expect("parse");
        let services = holos_engine::service::LocalServiceHandler::new();
        collect(
            Engine::query_prepared_with_services(&view, &parsed, services)
                .expect("query_prepared_with_services"),
        )
    };
    // And the evaluator itself, so all three are anchored to something outside the operator.
    let reference = rows(&engine, &query, &QueryOptions::new().explaining());

    assert_eq!(
        with_options, reference,
        "query_with disagrees with the evaluator"
    );
    assert_eq!(plain, reference, "query disagrees with the evaluator");
    assert_eq!(
        prepared, reference,
        "query_prepared_with_services disagrees with the evaluator"
    );
    assert_eq!(reference.len(), 60);
}

#[test]
fn the_column_order_of_the_projection_survives() {
    // `rows` above sorts the cells within each row, so every comparison in this file is
    // blind to column order. That is fine for checking bindings and wrong for checking
    // output: `head.vars` in SPARQL JSON, and the column order of CSV and TSV, come
    // straight from this list. A fast path that returned the right values under the right
    // names in the wrong order would pass every other test here and change what a client
    // downloads.
    let engine = engine();
    let session = Session::unrestricted(engine.store()).expect("session");
    let view = engine.view(&session);
    // Deliberately not alphabetical, and not the order the patterns bind them in.
    let query = format!("{P} SELECT ?d ?n ?s WHERE {{ ?s ex:name ?n . ?s ex:memberOf ?d }}");

    let names = |results: QueryResults<'_>| -> Vec<String> {
        match results {
            QueryResults::Solutions(iter) => iter
                .variables()
                .iter()
                .map(|v| v.as_str().to_owned())
                .collect(),
            _ => panic!("expected solutions"),
        }
    };

    let (fast, _) = Engine::query_with(&view, &query, &QueryOptions::new()).expect("query");
    let (slow, _) =
        Engine::query_with(&view, &query, &QueryOptions::new().explaining()).expect("query");

    assert_eq!(
        names(fast),
        names(slow),
        "column order differs between paths"
    );
    assert_eq!(
        names(
            Engine::query_with(&view, &query, &QueryOptions::new())
                .expect("query")
                .0
        ),
        vec!["d", "n", "s"],
        "the projection order in the query text is what a client gets"
    );
}

// ------------------------------------------------------------------------ filters

#[test]
fn filters_agree_with_the_evaluator() {
    // The whole point of borrowing `spareval`'s expression evaluator rather than writing a
    // second one: every case here is decided by the same code the slow path uses, so what is
    // actually being checked is that the *plumbing* — which variables get decoded, at what
    // depth the predicate runs, what an error does — does not change the verdict.
    //
    // The numeric and datatype cases are deliberate. SPARQL comparison is value-based with
    // numeric promotion, and a hand-rolled `=` would get several of these wrong while
    // passing anything written with strings.
    let engine = engine();
    for query in [
        // Comparison on a literal, the ordinary case.
        format!("{P} SELECT ?n WHERE {{ ?s ex:name ?n . FILTER(STRLEN(?n) > 8) }}"),
        // Numeric, where promotion across xsd types is the evaluator's business.
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(?b > 30) }}"),
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(?b >= 30.0) }}"),
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(?b = 7) }}"),
        // A conjunction, which the planner splits and pushes separately.
        format!(
            "{P} SELECT ?b ?d WHERE {{ ?s ex:badge ?b . ?s ex:memberOf ?d . \
             FILTER(?b > 10 && ?d = ex:d3) }}"
        ),
        // A disjunction, which it must not split.
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(?b < 5 || ?b > 55) }}"),
        // Negation, and a nested conjunction inside it.
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(!(?b > 5 && ?b < 50)) }}"),
        // Several separate FILTERs, which nest as wrappers rather than combining.
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(?b > 10) FILTER(?b < 20) }}"),
        // IN, IF and COALESCE: three shapes with their own evaluation rules.
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(?b IN (1, 2, 3)) }}"),
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(IF(?b > 30, true, false)) }}"),
        format!("{P} SELECT ?n WHERE {{ ?s ex:name ?n . FILTER(COALESCE(?n, \"x\") != \"\") }}"),
        // An expression that errors on some rows: STRLEN of a non-literal is an error, and
        // an erroring FILTER eliminates the solution rather than failing the query.
        format!("{P} SELECT ?d WHERE {{ ?s ex:memberOf ?d . FILTER(STRLEN(?d) > 1) }}"),
        // A filter over a term the join binds late, so it is applied at depth rather than
        // on the first pattern.
        format!(
            "{P} SELECT ?n ?label WHERE {{ ?s ex:name ?n . ?s ex:memberOf ?d . \
             ?d ex:label ?label . FILTER(?label != \"dept 0\") }}"
        ),
        // A filter that matches nothing, and one that matches everything.
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(?b > 1000) }}"),
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(?b >= 0) }}"),
        // A filter mentioning `?s`, `?p` or `?o`. Those three names are not arbitrary: the
        // conversion into the evaluator's expression type has no public entry point for a
        // bare expression, so the expression travels inside a throwaway `FILTER` wrapped
        // around a `?s ?p ?o` pattern. If that wrapper's variables could ever capture the
        // expression's own, this is where it would show.
        format!("{P} SELECT ?s WHERE {{ ?s ex:badge ?o . FILTER(?s != ex:p0) }}"),
        format!("{P} SELECT ?o WHERE {{ ?s ex:badge ?o . FILTER(?o > 30) }}"),
        format!("{P} SELECT ?p WHERE {{ ex:p3 ?p ?o . FILTER(?p != ex:name) }}"),
        // Constant filters, which have no variables and are therefore decided at the very
        // first frame, before anything is bound at all.
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(1 > 0) }}"),
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(\"a\" = \"b\") }}"),
        // With the solution modifiers, since filters run before them.
        format!(
            "{P} SELECT DISTINCT ?d WHERE {{ ?s ex:memberOf ?d . ?s ex:badge ?b . \
             FILTER(?b > 20) }}"
        ),
        format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(?b > 10) }} LIMIT 5 OFFSET 3"),
    ] {
        let (fast, slow) = both(&engine, &query);
        assert_eq!(fast, slow, "disagreement on {query}");
    }
}

#[test]
fn a_filter_query_actually_takes_the_fast_path() {
    // Without this, every case in `filters_agree_with_the_evaluator` would still pass if
    // `plan` had quietly started refusing filters again — both paths would simply be the
    // evaluator. The agreement tests are only meaningful while this one holds.
    let parsed = spargebra::SparqlParser::new()
        .parse_query(&format!(
            "{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(?b > 30) }}"
        ))
        .expect("parse");
    assert!(
        holos_engine::bindjoin::plan(&parsed).is_some(),
        "a filtered BGP is supposed to be inside the fragment"
    );
}

#[test]
fn expressions_the_borrowed_evaluator_cannot_judge_are_refused() {
    // `EXISTS` reads a dataset the expression evaluator does not have, and would answer
    // `false` for every solution rather than failing — a silent wrong answer. The
    // context-dependent builtins have no evaluation context here; `NOW` must additionally be
    // constant across a query, which this path has no way to arrange.
    for expression in [
        "EXISTS { ?s ex:nickname ?k }",
        "NOT EXISTS { ?s ex:nickname ?k }",
        "NOW() > \"2020-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>",
        "RAND() < 0.5",
        "STRLEN(STR(UUID())) > 0",
        "STRLEN(STRUUID()) > 0",
        "ISBLANK(BNODE())",
    ] {
        let text = format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER({expression}) }}");
        let parsed = spargebra::SparqlParser::new()
            .parse_query(&text)
            .expect("parse");
        assert!(
            holos_engine::bindjoin::plan(&parsed).is_none(),
            "should have been refused: {expression}"
        );
    }

    // And the answers must still be right, which is the reason refusing is acceptable.
    let engine = engine();
    let (fast, slow) = both(
        &engine,
        &format!(
            "{P} SELECT ?n WHERE {{ ?s ex:name ?n . FILTER(EXISTS {{ ?s ex:nickname ?k }}) }}"
        ),
    );
    assert_eq!(fast, slow);
    assert!(!fast.is_empty(), "20 of 60 people have a nickname");
}

#[test]
fn a_filter_on_an_unbound_variable_is_refused() {
    // A variable the patterns never bind brings the unbound rules with it — `BOUND`,
    // `COALESCE` and error propagation all behave differently there, and every variable this
    // path binds is bound by construction. Rather than model the exception, refuse it.
    let parsed = spargebra::SparqlParser::new()
        .parse_query(&format!(
            "{P} SELECT ?b WHERE {{ ?s ex:badge ?b . FILTER(!BOUND(?missing)) }}"
        ))
        .expect("parse");
    assert!(holos_engine::bindjoin::plan(&parsed).is_none());
}

#[test]
fn the_geosparql_filter_form_composes_and_the_property_form_does_not() {
    // The boundary of a claim that was first made too broadly, pinned so it stays honest.
    //
    // A hand-written `FILTER(geof:...)` over a geometry pattern is `Project(Filter(Bgp))`,
    // which is inside the fragment: `geof:sfWithin` is a custom function on the same
    // evaluator, so it is pushed down like any other predicate.
    //
    // The property shorthand is not, and `FILTER` alone was never going to make it so.
    // `topology::rewrite` turns `?f geo:sfWithin <literal>` into a geometry lookup joined in
    // as a *union* of ordinary patterns, so the rewritten query is
    // `Filter(Join(Bgp, Union(..)))`. Bringing that into the fragment needs `JOIN` and
    // `UNION`, which is a much larger piece of work than this one.
    use holos_engine::topology;

    const GEO: &str = "http://www.opengis.net/ont/geosparql#";
    const WINDOW: &str = "POLYGON((0 0, 10 0, 10 10, 0 10, 0 0))";
    let prefixes = format!(
        "PREFIX geo: <{GEO}> PREFIX geof: <http://www.opengis.net/def/function/geosparql/>"
    );

    let parse = |text: &str| {
        spargebra::SparqlParser::new()
            .parse_query(text)
            .expect("parse")
    };

    let filter_form = parse(&format!(
        "{prefixes} SELECT ?f WHERE {{ ?f geo:asWKT ?g .          FILTER(geof:sfWithin(?g, \"{WINDOW}\"^^geo:wktLiteral)) }}"
    ));
    assert!(
        holos_engine::bindjoin::plan(&topology::rewrite(&filter_form, None)).is_some(),
        "the explicit FILTER form should be inside the fragment"
    );

    let property_form = parse(&format!(
        "{prefixes} SELECT ?f WHERE {{ ?f geo:sfWithin \"{WINDOW}\"^^geo:wktLiteral }}"
    ));
    assert!(
        holos_engine::bindjoin::plan(&topology::rewrite(&property_form, None)).is_none(),
        "the property form rewrites to a join over a union, which this fragment refuses"
    );

    // And both still answer correctly, which is what makes refusing one of them acceptable.
    let mut engine = Engine::new();
    engine
        .bulk_load(
            format!(
                "@prefix geo: <{GEO}> .\n                 <http://example.com/inside>  geo:asWKT \"POINT(5 5)\"^^geo:wktLiteral .\n                 <http://example.com/outside> geo:asWKT \"POINT(50 50)\"^^geo:wktLiteral .\n"
            )
            .as_bytes(),
            RdfFormat::Turtle,
            None,
        )
        .expect("load");

    for query in [
        format!(
            "{prefixes} SELECT ?f WHERE {{ ?f geo:asWKT ?g .              FILTER(geof:sfWithin(?g, \"{WINDOW}\"^^geo:wktLiteral)) }}"
        ),
        format!("{prefixes} SELECT ?f WHERE {{ ?f geo:sfWithin \"{WINDOW}\"^^geo:wktLiteral }}"),
    ] {
        let (fast, slow) = both(&engine, &query);
        assert_eq!(fast, slow, "disagreement on {query}");
        assert_eq!(
            fast,
            vec!["f=<http://example.com/inside>".to_owned()],
            "only the point inside the window is within it: {query}"
        );
    }
}

#[test]
fn policy_is_enforced_on_the_fast_path() {
    // The property that must not be lost: the fast path scans through the same
    // policy-filtered view, so a denied predicate is invisible to it exactly as it is to the
    // evaluator. A fast path that read the store directly would be a way around §14.
    use holos_security::{Modes, Policy, Principal, PrincipalMatch, Rule, Scope};
    use oxrdf::NamedNode;

    let engine = engine();
    let policy = Policy::permit_all().with_rule(Rule::deny(
        Modes::READ,
        Scope::Predicate(NamedNode::new_unchecked(format!("{EX}badge"))),
        PrincipalMatch::Everyone,
    ));
    let session = Session::open(engine.store(), Principal::anonymous(), policy).expect("session");
    let view = engine.view(&session);
    let stats =
        Arc::new(Statistics::build(engine.store(), GraphFilter::Default).expect("statistics"));

    let query = format!("{P} SELECT ?b WHERE {{ ?s ex:badge ?b }}");
    let (results, _) =
        Engine::query_with(&view, &query, &QueryOptions::new().reordering(stats)).expect("query");
    match results {
        QueryResults::Solutions(iter) => assert_eq!(
            iter.count(),
            0,
            "the fast path returned quads the policy denies"
        ),
        _ => panic!("expected solutions"),
    }
}
