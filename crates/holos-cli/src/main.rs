//! `holos` — the command line.
//!
//! Deliberately small. It exists so the layers below can be exercised end to end,
//! including the access policy, which is the part most worth being able to try by hand.

use anyhow::{bail, Context, Result};
use holos_engine::Engine;
use holos_shacl::{CompiledShapes, Options as ShaclOptions};
use holos_security::{
    CollectingSink, Label, Modes, Policy, Principal, PrincipalMatch, Rule, Scope, Semantics,
    Session,
};
use holos_store::GraphFilter;
use oxrdf::{GraphName, NamedNode, Quad};
use oxrdfio::{RdfFormat, RdfSerializer};
use sparesults::{QueryResultsFormat, QueryResultsSerializer};
use spareval::QueryResults;
use std::fs::File;
use std::io::{stdout, Read, Write};
use std::path::Path;

const USAGE: &str = "\
holos — an RDF 1.2 store with SPARQL 1.2 and policy enforced at the scan

USAGE
    holos query    --data <FILE>... (--query <SPARQL> | --query-file <FILE>) [POLICY] [OPTIONS]
    holos update   --store <DIR> (--update <SPARQL> | --update-file <FILE>) [POLICY]
    holos stats    --data <FILE>...
    holos dump     --data <FILE>... [POLICY]
    holos validate --data <FILE>... [--shapes <FILE>]

DATA
    --data <FILE>            Load a file. Repeatable. Format is taken from the extension:
                             .ttl .nt .trig .nq .rdf .jsonld .n3 — each also with .gz,
                             which is how large RDF dumps are normally distributed.
                             Decompression is streamed, so a 60 GB dump costs no more
                             memory than a small one.
    --base <IRI>             Base IRI for parsing.
    --store <DIR>            Use a persistent RocksDB store at DIR. Without this the store
                             is in memory and is discarded on exit.
    --bulk                   Buffer writes and skip the write-ahead log while loading. Much
                             faster; a load interrupted part-way must be discarded.

QUERY
    --query <SPARQL>         Query text.
    --query-file <FILE>      Read the query from a file.
    --results <FORMAT>       json (default), xml, csv, tsv.
    --default-graph <IRI>    Add a graph to the query's default graph. Repeatable.
    --named-graph <IRI>      Let GRAPH ?g range over this graph. Repeatable.
    --union-default-graph    Query the union of the named graphs as the default graph.
    --timeout <SECONDS>      Give up after this long.
    --explain                Print the query plan as JSON instead of the results.
    --reorder                Order each basic graph pattern by estimated cardinality before
                             evaluating. Costs one pass over the store to build statistics,
                             and makes a badly ordered query as fast as a well ordered one:
                             measured 14x on a four-pattern join at 7.5M quads. See
                             BENCHMARKS.md.

UPDATE
    --update <SPARQL>        Update text.
    --update-file <FILE>     Read the update from a file.

                             An update is all-or-nothing: if any operation fails, the
                             store is left exactly as it was. Policy applies to every quad
                             written, and the WHERE clause is filtered by read policy, so
                             a principal cannot delete what it cannot see.

DUMP
    --format <FORMAT>        RDF serialisation for `dump`: nq (default), nt, trig, ttl,
                             rdf, jsonld. N-Quads is the default because it is the only
                             line-based format that carries the graph name, so a dump of a
                             quad store round-trips through it without loss.

POLICY  (omit all of these for unrestricted access)
    --deny-all               Start from deny-by-default instead of permit-all.
    --allow-graph <IRI>      Grant read on one named graph.
    --deny-predicate <IRI>   Refuse read on one predicate, for everyone without --role hr-style
                             exemption. Repeatable.
    --allow-predicate <IRI>  Grant read on one predicate. Repeatable.
    --label-graph <IRI>=<N>  Classify a graph at level N. Repeatable.
    --clearance <N>          Give the principal clearance level N.
    --role <NAME>            Give the principal a role. Repeatable.
    --except-role <NAME>     Make --deny-predicate not apply to this role.
    --fail-closed            Error on refusal instead of filtering silently.
    --audit                  Print the audit record to stderr after the query.

VALIDATE
    --shapes <FILE>          Read shapes from a separate file, kept in its own graph.
                             Without it the shapes are expected in the data itself.
    --report                 Print the validation report as N-Triples.
    --engine <NAME>          native (default) or vendored. 'native' reads the live store and
                             supports incremental revalidation; 'vendored' bridges the store
                             into the adapted SHACL_Engine, which covers far more of SHACL.

OPTIONS
    -h, --help               This text.
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    let (command, rest) = args.split_first().expect("checked non-empty");
    let opts = Options::parse(rest)?;

    let mut engine = opts.open_engine()?;
    if !opts.data.is_empty() {
        let started = std::time::Instant::now();
        let mut total = 0;
        opts.begin_bulk(&mut engine);
        for path in &opts.data {
            let format = format_for(path)?;
            total += engine
                .bulk_load(open_data(path)?, format, opts.base.as_deref())
                .with_context(|| format!("loading {path}"))?;
        }
        engine.store_mut().end_bulk_load()?;
        let elapsed = started.elapsed();
        let rate = (total as f64 / elapsed.as_secs_f64()) as u64;
        eprintln!(
            "loaded {total} quads in {:.2}s ({rate} quads/s)",
            elapsed.as_secs_f64()
        );
    }

    match command.as_str() {
        "query" => query(&engine, &opts),
        "stats" => stats(&engine),
        "dump" => dump(&engine, &opts),
        "update" => update_command(&mut engine, &opts),
        "validate" => validate(&mut engine, &opts),
        other => bail!("unknown command `{other}`\n\n{USAGE}"),
    }
}

fn query(engine: &Engine, opts: &Options) -> Result<()> {
    let query = match (&opts.query, &opts.query_file) {
        (Some(q), _) => q.clone(),
        (None, Some(path)) => {
            let mut s = String::new();
            File::open(path)
                .with_context(|| format!("opening {path}"))?
                .read_to_string(&mut s)?;
            s
        }
        (None, None) => bail!("query needs --query or --query-file\n\n{USAGE}"),
    };

    let session = opts.session(engine)?;
    let view = engine.view(&session);
    let audit = CollectingSink::new();
    let mut query_options = opts.query_options()?;
    if opts.reorder {
        // One pass over the store. Worth it whenever the saving beats the build, which a
        // single badly ordered join usually does.
        let stats = holos_stats::Statistics::build(engine.store(), GraphFilter::Default)?;
        query_options = query_options.reordering(std::sync::Arc::new(stats));
    }

    let results = if opts.audit {
        Engine::query_audited(&view, session.principal(), &query, &audit)?
    } else {
        let (results, explanation) = Engine::query_with(&view, &query, &query_options)?;
        if let Some(explanation) = explanation {
            // Drain first: the per-operator statistics are filled in as rows flow, so a
            // plan printed before consuming the results would show zeroes everywhere.
            drain(results);
            explanation.write_in_json(stdout().lock())?;
            println!();
            return Ok(());
        }
        results
    };

    write_results(results, opts.results.0)?;

    if opts.audit {
        for event in audit.events() {
            eprintln!(
                "audit: principal={} outcome={:?} withheld={}",
                event.principal, event.outcome, event.filtered_quads
            );
        }
    } else if view.filtered_count() > 0 {
        // Operator-facing only, and only on stderr: the count says hidden data exists.
        eprintln!("note: {} quads withheld by policy", view.filtered_count());
    }
    Ok(())
}

fn write_results(results: QueryResults<'_>, format: QueryResultsFormat) -> Result<()> {
    let serializer = QueryResultsSerializer::from_format(format);
    let out = stdout().lock();
    match results {
        QueryResults::Boolean(value) => {
            let _ = serializer.serialize_boolean_to_writer(out, value)?;
        }
        QueryResults::Solutions(solutions) => {
            let variables = solutions.variables().to_vec();
            let mut writer = serializer.serialize_solutions_to_writer(out, variables)?;
            for solution in solutions {
                writer.serialize(&solution?)?;
            }
            let _ = writer.finish()?;
        }
        QueryResults::Graph(triples) => {
            // CONSTRUCT and DESCRIBE produce triples, not a result set; N-Triples is the
            // lowest-common-denominator serialisation and needs no prefix bookkeeping.
            let mut out = out;
            for triple in triples {
                writeln!(out, "{} .", triple?)?;
            }
        }
    }
    println!();
    Ok(())
}

fn stats(engine: &Engine) -> Result<()> {
    let store = engine.store();
    println!("quads            {}", store.len());
    println!("dictionary terms {}", store.dictionary_len());
    println!("named graphs     {}", store.named_graphs()?.len());
    println!();
    println!("predicates by frequency");
    for (id, n) in store.predicate_histogram().iter().take(25) {
        let name = store
            .decode_term(*id)?
            .map_or_else(|| format!("{id:?}"), |t| t.to_string());
        println!("  {n:>10}  {name}");
    }
    Ok(())
}

/// Writes every quad the principal may see, as RDF.
///
/// The query below is the whole point: dumping goes through the same policy-filtered view
/// as any other read, so a dump can never disclose more than a query would. It is not a
/// backup — a backup is a copy of the store directory (see `deploy/backup.sh`); this is
/// what the *principal* is allowed to see, which is usually a smaller thing.
///
/// N-Quads is the default because it is the only line-based serialisation that carries the
/// graph name. Dumping a quad store as N-Triples silently flattens named graphs into one,
/// so the format choice here is a correctness question rather than a preference.
fn dump(engine: &Engine, opts: &Options) -> Result<()> {
    let session = opts.session(engine)?;
    let view = engine.view(&session);
    let results = Engine::query(
        &view,
        "SELECT * WHERE { { ?s ?p ?o } UNION { GRAPH ?g { ?s ?p ?o } } }",
        None,
    )?;

    let format = opts.format.unwrap_or(RdfFormat::NQuads);
    let QueryResults::Solutions(solutions) = results else {
        bail!("the dump query did not return solutions");
    };

    let mut writer = RdfSerializer::from_format(format).for_writer(stdout().lock());
    let mut written = 0_u64;
    for solution in solutions {
        let solution = solution?;
        let (Some(s), Some(p), Some(o)) = (
            solution.get("s"),
            solution.get("p"),
            solution.get("o"),
        ) else {
            continue;
        };
        let subject = match s {
            oxrdf::Term::NamedNode(n) => oxrdf::NamedOrBlankNode::NamedNode(n.clone()),
            oxrdf::Term::BlankNode(b) => oxrdf::NamedOrBlankNode::BlankNode(b.clone()),
            // A literal or a triple term in subject position cannot be serialised as a
            // quad. Nothing in the store can produce one, so this is unreachable rather
            // than a silent drop, but skipping is the safe reading either way.
            _ => continue,
        };
        let oxrdf::Term::NamedNode(predicate) = p else {
            continue;
        };
        let graph_name = match solution.get("g") {
            Some(oxrdf::Term::NamedNode(n)) => GraphName::NamedNode(n.clone()),
            Some(oxrdf::Term::BlankNode(b)) => GraphName::BlankNode(b.clone()),
            _ => GraphName::DefaultGraph,
        };
        writer.serialize_quad(
            Quad {
                subject,
                predicate: predicate.clone(),
                object: o.clone(),
                graph_name,
            }
            .as_ref(),
        )?;
        written += 1;
    }
    let _ = writer.finish()?;
    eprintln!("{written} quads");
    Ok(())
}

/// Consumes a result stream so an explanation's statistics are populated.
fn drain(results: QueryResults<'_>) {
    match results {
        QueryResults::Solutions(iter) => iter.for_each(|_| {}),
        QueryResults::Graph(iter) => iter.for_each(|_| {}),
        QueryResults::Boolean(_) => {}
    }
}

/// Applies a SPARQL 1.1 update.
///
/// Prints what changed to stderr, so stdout stays free for anything piped after it.
fn update_command(engine: &mut Engine, opts: &Options) -> Result<()> {
    let text = match (&opts.update, &opts.update_file) {
        (Some(text), _) => text.clone(),
        (None, Some(path)) => {
            std::fs::read_to_string(path).with_context(|| format!("reading {path}"))?
        }
        (None, None) => bail!("update needs --update or --update-file"),
    };

    let mut session = opts.session(engine)?;
    let started = std::time::Instant::now();
    let outcome = holos_engine::update::update(engine, &mut session, &text, opts.base.as_deref())?;
    engine.store_mut().flush()?;

    eprintln!(
        "inserted {} deleted {} graphs +{} -{} in {:.3}s",
        outcome.inserted,
        outcome.deleted,
        outcome.graphs_created,
        outcome.graphs_dropped,
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// Validates the loaded data against SHACL shapes, reporting where the time went.
///
/// The timing split is the point. SHACL_Engine's own benchmarks have loading exceeding
/// validation roughly threefold at 100k instances, because a validator that is a library
/// has to build its own copy of the graph first. A validator that reads the store's
/// indexes does not load at all — so the question this command answers is whether that
/// actually shows up (DESIGN.md §8).
fn validate(engine: &mut Engine, opts: &Options) -> Result<()> {
    let shapes_graph = match &opts.shapes {
        None => GraphFilter::Default,
        Some(path) => {
            let format = format_for(path)?;
            let name = NamedNode::new_unchecked("urn:holos:shapes");
            let n = engine.bulk_load_into_graph(
                open_data(path)?,
                format,
                opts.base.as_deref(),
                &name.clone().into(),
            )?;
            eprintln!("loaded {n} shape quads from {path}");
            let id = engine
                .store()
                .lookup_term(name.as_ref().into())?
                .context("shapes graph did not intern")?;
            GraphFilter::Named(id)
        }
    };

    if opts.engine.as_deref() == Some("vendored") {
        let started = std::time::Instant::now();
        let mut run = holos_shacl::engine::EngineRun::prepare(
            engine.store(),
            ShaclOptions { data_graph: GraphFilter::Default, shapes_graph },
        )?;
        let prepared = started.elapsed();
        let started = std::time::Instant::now();
        let report = run.validate()?;
        let validated = started.elapsed();
        println!("bridged+compiled  {} triples, {} shapes in {:.3}s", run.triples(), run.shapes(), prepared.as_secs_f64());
        println!("validated         {:.3}s", validated.as_secs_f64());
        println!("conforms          {}", run.conforms(&report));
        println!("results           {}", report.results.len());
        if opts.report {
            let graph = run.report_to_oxrdf(&report);
            let mut out = stdout().lock();
            for triple in graph.iter() {
                writeln!(out, "{triple} .")?;
            }
        }
        return Ok(());
    }

    let started = std::time::Instant::now();
    let shapes = CompiledShapes::compile(
        engine.store(),
        ShaclOptions {
            data_graph: GraphFilter::Default,
            shapes_graph,
        },
    )?;
    let compiled = started.elapsed();

    let started = std::time::Instant::now();
    let report = shapes.validate(engine.store())?;
    let validated = started.elapsed();

    println!("shapes compiled   {} shapes in {:.3}s", shapes.shapes().len(), compiled.as_secs_f64());
    println!("validated         {:.3}s", validated.as_secs_f64());
    println!("conforms          {}", report.conforms);
    println!("results           {}", report.results.len());

    if opts.report {
        let quads = shapes.report_to_quads(engine.store(), &report)?;
        let mut out = stdout().lock();
        for quad in quads {
            writeln!(out, "{} .", oxrdf::Triple {
                subject: quad.subject,
                predicate: quad.predicate,
                object: quad.object,
            })?;
        }
    }
    Ok(())
}

/// The RDF format a file name implies, seeing through a `.gz` suffix.
fn format_for(path: &str) -> Result<RdfFormat> {
    holos_engine::source::format_for_path(Path::new(path)).with_context(|| {
        format!("cannot infer an RDF format from `{path}`; expected .ttl .nt .trig .nq .rdf .n3 .jsonld, optionally with .gz")
    })
}

/// Opens a data file, decompressing it when the name says to.
fn open_data(path: &str) -> Result<Box<dyn std::io::BufRead + Send>> {
    Ok(holos_engine::source::reader(Path::new(path))?)
}

#[derive(Debug, Default)]
struct Options {
    data: Vec<String>,
    base: Option<String>,
    query: Option<String>,
    query_file: Option<String>,
    update: Option<String>,
    update_file: Option<String>,
    default_graphs: Vec<String>,
    named_graphs: Vec<String>,
    union_default_graph: bool,
    timeout: Option<f64>,
    explain: bool,
    reorder: bool,
    results: QueryResultsFormatOpt,
    format: Option<RdfFormat>,
    deny_all: bool,
    allow_graphs: Vec<String>,
    deny_predicates: Vec<String>,
    allow_predicates: Vec<String>,
    graph_labels: Vec<(String, u16)>,
    clearance: Option<u16>,
    roles: Vec<String>,
    except_role: Option<String>,
    fail_closed: bool,
    audit: bool,
    store: Option<String>,
    bulk: bool,
    shapes: Option<String>,
    report: bool,
    engine: Option<String>,
}

/// Newtype so `Options` can derive `Default` — `QueryResultsFormat` has no default.
#[derive(Debug, Clone, Copy)]
struct QueryResultsFormatOpt(QueryResultsFormat);

impl Default for QueryResultsFormatOpt {
    fn default() -> Self {
        Self(QueryResultsFormat::Json)
    }
}

impl Options {
    /// Builds the evaluation options from the command line flags.
    fn query_options(&self) -> Result<holos_engine::QueryOptions> {
        let mut options = holos_engine::QueryOptions::new();
        if let Some(base) = &self.base {
            options = options.with_base_iri(base.clone());
        }
        for iri in &self.default_graphs {
            options = options.with_default_graph(iri_arg(iri)?.into());
        }
        for iri in &self.named_graphs {
            options = options.with_named_graph(iri_arg(iri)?.into());
        }
        options.union_default_graph = self.union_default_graph;
        if let Some(seconds) = self.timeout {
            if seconds > 0.0 {
                options = options.with_timeout(std::time::Duration::from_secs_f64(seconds));
            }
        }
        if self.explain {
            options = options.explaining();
        }
        Ok(options)
    }

    fn parse(args: &[String]) -> Result<Self> {
        let mut o = Self::default();
        let mut i = 0;
        while i < args.len() {
            let flag = args[i].clone();
            // Pulls the next argument as this flag's value.
            let value = |i: &mut usize| -> Result<String> {
                *i += 1;
                args.get(*i)
                    .cloned()
                    .with_context(|| format!("{flag} needs a value"))
            };
            match args[i].as_str() {
                "--data" => o.data.push(value(&mut i)?),
                "--base" => o.base = Some(value(&mut i)?),
                "--query" => o.query = Some(value(&mut i)?),
                "--update" => o.update = Some(value(&mut i)?),
                "--update-file" => o.update_file = Some(value(&mut i)?),
                "--default-graph" => o.default_graphs.push(value(&mut i)?),
                "--named-graph" => o.named_graphs.push(value(&mut i)?),
                "--union-default-graph" => o.union_default_graph = true,
                "--timeout" => o.timeout = Some(value(&mut i)?.parse()?),
                "--explain" => o.explain = true,
                "--reorder" => o.reorder = true,
                "--query-file" => o.query_file = Some(value(&mut i)?),
                "--format" => {
                    let raw = value(&mut i)?;
                    o.format = Some(match raw.as_str() {
                        "nq" | "nquads" | "n-quads" => RdfFormat::NQuads,
                        "nt" | "ntriples" | "n-triples" => RdfFormat::NTriples,
                        "trig" => RdfFormat::TriG,
                        "ttl" | "turtle" => RdfFormat::Turtle,
                        "rdf" | "rdfxml" | "rdf-xml" => RdfFormat::RdfXml,
                        "jsonld" | "json-ld" => RdfFormat::JsonLd {
                            profile: oxrdfio::JsonLdProfileSet::empty(),
                        },
                        other => bail!(
                            "unknown --format `{other}`; expected nq, nt, trig, ttl, rdf or jsonld"
                        ),
                    });
                }
                "--results" => {
                    o.results = QueryResultsFormatOpt(match value(&mut i)?.as_str() {
                        "json" => QueryResultsFormat::Json,
                        "xml" => QueryResultsFormat::Xml,
                        "csv" => QueryResultsFormat::Csv,
                        "tsv" => QueryResultsFormat::Tsv,
                        other => bail!("unknown result format `{other}`"),
                    });
                }
                "--deny-all" => o.deny_all = true,
                "--allow-graph" => o.allow_graphs.push(value(&mut i)?),
                "--deny-predicate" => o.deny_predicates.push(value(&mut i)?),
                "--allow-predicate" => o.allow_predicates.push(value(&mut i)?),
                "--label-graph" => {
                    let raw = value(&mut i)?;
                    let (iri, level) = raw
                        .rsplit_once('=')
                        .with_context(|| format!("--label-graph wants <IRI>=<N>, got `{raw}`"))?;
                    o.graph_labels.push((iri.to_owned(), level.parse()?));
                }
                "--clearance" => o.clearance = Some(value(&mut i)?.parse()?),
                "--role" => o.roles.push(value(&mut i)?),
                "--except-role" => o.except_role = Some(value(&mut i)?),
                "--store" => o.store = Some(value(&mut i)?),
                "--bulk" => o.bulk = true,
                "--shapes" => o.shapes = Some(value(&mut i)?),
                "--report" => o.report = true,
                "--engine" => o.engine = Some(value(&mut i)?),
                "--fail-closed" => o.fail_closed = true,
                "--audit" => o.audit = true,
                other => bail!("unknown flag `{other}`

{USAGE}"),
            }
            i += 1;
        }
        Ok(o)
    }

    /// Opens the engine over the requested backend.
    fn open_engine(&self) -> Result<Engine> {
        match &self.store {
            None => Ok(Engine::new()),
            #[cfg(feature = "rocksdb")]
            Some(path) => {
                let storage = holos_store::RocksStorage::open(path)
                    .with_context(|| format!("opening the store at {path}"))?;
                Ok(Engine::with_store(holos_store::Store::with_storage(storage)))
            }
            #[cfg(not(feature = "rocksdb"))]
            Some(_) => bail!("this build has no persistent backend: rebuild with --features rocksdb"),
        }
    }

    fn begin_bulk(&self, engine: &mut Engine) {
        if self.bulk {
            engine.store_mut().begin_bulk_load();
        }
    }

    fn has_policy(&self) -> bool {
        self.deny_all
            || !self.allow_graphs.is_empty()
            || !self.deny_predicates.is_empty()
            || !self.allow_predicates.is_empty()
            || !self.graph_labels.is_empty()
            || self.clearance.is_some()
            || self.fail_closed
    }

    fn session(&self, engine: &Engine) -> Result<Session> {
        let mut principal = Principal::anonymous();
        for role in &self.roles {
            principal = principal.with_role(role);
        }
        if let Some(level) = self.clearance {
            principal = principal.with_clearance(Label::level(level));
        }
        if !self.has_policy() {
            return Ok(Session::open(
                engine.store(),
                principal,
                Policy::permit_all(),
            )?);
        }

        let mut policy = if self.deny_all {
            Policy::default()
        } else {
            Policy::permit_all()
        };
        if self.fail_closed {
            policy = policy.with_semantics(Semantics::Fail);
        }
        // An exemption belongs in the principal match, not in a competing allow: two
        // rules at the same scope resolve deny-first.
        let denied_to = match &self.except_role {
            Some(role) => PrincipalMatch::Not(Box::new(PrincipalMatch::Role(role.clone()))),
            None => PrincipalMatch::Everyone,
        };
        for iri in &self.allow_graphs {
            policy = policy.with_rule(Rule::allow(
                Modes::READ,
                Scope::Graph(iri_arg(iri)?),
                PrincipalMatch::Everyone,
            ));
        }
        for iri in &self.allow_predicates {
            policy = policy.with_rule(Rule::allow(
                Modes::READ,
                Scope::Predicate(iri_arg(iri)?),
                PrincipalMatch::Everyone,
            ));
        }
        for iri in &self.deny_predicates {
            policy = policy.with_rule(Rule::deny(
                Modes::READ,
                Scope::Predicate(iri_arg(iri)?),
                denied_to.clone(),
            ));
        }
        for (iri, level) in &self.graph_labels {
            policy = policy.with_graph_label(iri_arg(iri)?, Label::level(*level));
        }
        Ok(Session::open(engine.store(), principal, policy)?)
    }
}

fn iri_arg(s: &str) -> Result<NamedNode> {
    NamedNode::new(s).with_context(|| format!("`{s}` is not a valid IRI"))
}
