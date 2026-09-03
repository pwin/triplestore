//! `holos` — the command line.
//!
//! Deliberately small. It exists so the layers below can be exercised end to end,
//! including the access policy, which is the part most worth being able to try by hand.

use anyhow::{bail, Context, Result};
use holos_engine::Engine;
use holos_security::{
    CollectingSink, Label, Modes, Policy, Principal, PrincipalMatch, Rule, Scope, Semantics,
    Session,
};
use holos_shacl::{CompiledShapes, Options as ShaclOptions};
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
    holos backup   --store <DIR> --to <DIR>
    holos compact  --store <DIR> --to <DIR>
    holos entail   --store <DIR> [--entail-graph <IRI>] [--entail-budget <N>]

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

MAINTENANCE
    --entail-graph <IRI>     Where `entail` writes. Its own graph so it can be dropped again
                             and told apart from what was asserted. Default
                             <https://holos.dev/ns#entailed> -- an absolute IRI, because
                             `DROP GRAPH` needs the name the store actually holds.
    --entail-budget <N>      Refuse a closure larger than N new triples. Default 10,000,000.
    --force                  Run `compact` even when the headroom estimate says it will
                             not fit. The estimate is the source store's size on disk, which
                             is generous on purpose: a compaction that reclaims most of a
                             store needs far less. Use it when you know that is the case.
    --to <DIR>               Destination for `backup` and `compact`. Must not exist.
                             `backup` writes a RocksDB checkpoint: near-instant, hard-linked,
                             and it preserves the store exactly — including the dictionary
                             entries left behind by deleted quads.
                             `compact` writes a fresh store containing only the live data,
                             which is the only thing that reclaims those. It reads the store
                             directly rather than through a policy, so it copies everything;
                             it needs room for both stores; and it does not copy writes that
                             arrive while it runs.

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
    --engine <NAME>          native (default) or adapted. 'native' reads the live store and
                             revalidates a delta; 'adapted' bridges the store into the
                             adapted SHACL_Engine, which covers far more of SHACL.
                             'native' refuses a shapes graph using anything it cannot check
                             -- sh:sparql, SHACL-AF rules, node expressions -- rather than
                             dropping the constraint and reporting conformance. Use
                             'adapted' for those.

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
        opts.begin_bulk(&mut engine)?;
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
        "stats" => stats(&engine, &opts),
        "dump" => dump(&engine, &opts),
        "update" => update_command(&mut engine, &opts),
        "validate" => validate(&mut engine, &opts),
        "backup" => backup(&engine, &opts),
        "compact" => compact(&engine, &opts),
        "entail" => entail(&mut engine, &opts),
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

/// Renders a byte count the way an operator reading a refusal wants to see it.
fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Free space on the filesystem that will hold `destination`.
///
/// Asked of the nearest existing ancestor, because the destination itself must not exist
/// yet — that is the precondition both maintenance commands check first.
fn available_at(destination: &Path) -> Option<u64> {
    let mut probe = destination;
    loop {
        if probe.exists() {
            return fs4::available_space(probe).ok();
        }
        probe = probe.parent()?;
        if probe.as_os_str().is_empty() {
            return fs4::available_space(Path::new(".")).ok();
        }
    }
}

/// Refuses a maintenance command that cannot fit, before it spends an hour finding out.
///
/// The estimate is the source store's size on disk, which is deliberately generous for
/// `compact` — the whole point of compacting is that the result is smaller — and about right
/// for a `backup` that has to copy rather than link. Erring high is the safe direction for a
/// check whose failure mode is a full disk.
///
/// `--force` exists because the estimate cannot know the answer: a compaction that reclaims
/// nine tenths of a store genuinely needs a tenth of this, and an operator who knows that
/// should not be blocked by arithmetic.
fn check_headroom(
    engine: &Engine,
    source: Option<&str>,
    destination: &Path,
    force: bool,
) -> Result<()> {
    let (Some(source), Some(needed)) = (source, engine.store().on_disk_bytes()) else {
        return Ok(());
    };
    let Some(available) = available_at(destination) else {
        // Not knowing is not a reason to refuse. Say so, so an operator on a filesystem this
        // cannot read does not think the check passed.
        eprintln!(
            "note: could not read free space for {}",
            destination.display()
        );
        return Ok(());
    };
    if let Some(warning) = headroom_verdict(source, destination, needed, available, force)? {
        eprintln!("{warning}");
    }
    Ok(())
}

/// The decision, separated from the filesystem so it can be tested.
///
/// `Ok(None)` fits, `Ok(Some(_))` does not fit but `--force` was given and here is what to
/// say about it, `Err` refuses. Splitting it out is not ceremony: the interesting behaviour
/// is entirely in the comparison, and reaching it through the real filesystem would mean a
/// test that can only run on a nearly-full disk.
fn headroom_verdict(
    source: &str,
    destination: &Path,
    needed: u64,
    available: u64,
    force: bool,
) -> Result<Option<String>> {
    // A source of nothing is a store that has not been written to, or a path this could not
    // read. Either way there is nothing to compare against.
    if needed == 0 || available >= needed {
        return Ok(None);
    }
    anyhow::ensure!(
        force,
        "not enough room: {} holds {}, and {} has {} free.\n\n\
         This is an estimate from the source's size on disk, and it is deliberately \
         generous — a compaction that reclaims most of a store needs far less. If you know \
         it will fit, pass --force.\n\n\
         Otherwise: free space, choose a --to on another filesystem, or run `holos compact` \
         first if this is a backup of a store with a lot of deleted data in it.",
        source,
        human(needed),
        destination.display(),
        human(available)
    );
    Ok(Some(format!(
        "warning: {} holds {} and {} has {} free; continuing because --force was given",
        source,
        human(needed),
        destination.display(),
        human(available)
    )))
}

/// Writes a consistent snapshot of a persistent store to a new directory.
///
/// Works on a store another process has open and is writing to, which is what makes it a
/// backup rather than a maintenance window. RocksDB flushes its log and hard-links the SST
/// files, so it is near-instant and initially costs almost no disk.
///
/// Two things worth knowing, both printed rather than buried:
///
/// * Hard links need the **same filesystem**. To another mount RocksDB copies instead —
///   still correct, no longer instant. Back up locally, then move or replicate the result.
/// * A checkpoint **pins the files it links**, so it cannot be deleted by compaction. Disk
///   use climbs as the snapshot and the live store diverge; old checkpoints need removing.
fn backup(engine: &Engine, opts: &Options) -> Result<()> {
    let destination = opts
        .to
        .as_deref()
        .context("--to <DIR> says where to write the snapshot")?;
    let destination = Path::new(destination);
    anyhow::ensure!(
        !destination.exists(),
        "{} already exists; a checkpoint needs a directory that does not, which is why          timestamped names are the usual pattern",
        destination.display()
    );
    if let Some(parent) = destination.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
    }

    // A warning rather than a refusal. A checkpoint onto the same filesystem hard-links and
    // costs almost nothing up front, so demanding the full size would block the ordinary
    // case; but the links pin blocks that compaction can then never reclaim, so a store this
    // size *will* eventually be needed. Onto another filesystem it is needed immediately.
    if let (Some(needed), Some(available)) =
        (engine.store().on_disk_bytes(), available_at(destination))
    {
        if needed > 0 && available < needed {
            eprintln!(
                "warning: {} has {} free and the store holds {}.",
                destination.display(),
                human(available),
                human(needed)
            );
            eprintln!(
                "         A checkpoint onto the same filesystem hard-links and will fit, but \
                 the links pin"
            );
            eprintln!(
                "         files compaction can no longer reclaim, so the space is owed \
                 either way. Onto a"
            );
            eprintln!("         different filesystem this will copy, and will not fit.");
        }
    }

    let quads = engine.store().len();
    engine
        .store()
        .checkpoint(destination)
        .with_context(|| format!("checkpointing to {}", destination.display()))?;

    println!(
        "wrote a checkpoint of {quads} quads to {}",
        destination.display()
    );
    println!();
    println!("restore by pointing --store at it, or by copying it back over the original");
    println!("note: it hard-links the live store's files where it can, so it is not an");
    println!("      off-machine backup until it is copied somewhere else");
    Ok(())
}

/// Rewrites the store into a fresh one, reclaiming everything the dictionary is holding on
/// to.
///
/// # What accumulates, and why nothing else clears it
///
/// The term dictionary is **append-only** — §5 depends on that, and so does everything
/// derived from it. Deleting quads therefore reclaims their index entries and nothing else:
/// the terms they used stay interned for ever. A store that has churned carries a dictionary
/// sized by every term it has *ever* seen.
///
/// Neither backup nor restart helps. A backup is a RocksDB checkpoint — the SST files are
/// hard-linked, so a restore hands back the same dictionary, dead entries included.
///
/// # Why this is a copy and not an edit
///
/// Reclaiming a dictionary slot in place means proving nothing refers to it, and that is
/// harder than it looks. An RDF 1.2 triple term holds its components by id, so a term can be
/// referenced while **no quad mentions it at all**:
///
/// ```text
/// <claim> <says> <<( <a> <p> "v" )>> .
///
/// <a> and <p> are interned IRIs that appear in no quad.
/// ```
///
/// A reference check that looked only at quads would free them and leave the triple term
/// pointing at nothing. Copying has no such failure mode: it writes only terms it has just
/// read, so anything reachable comes with its referents, and anything unreachable is left
/// behind by construction rather than by analysis.
///
/// # This is not a dump
///
/// [`dump`] goes through the policy-filtered view and writes what the *principal* may see.
/// Compaction reads the store directly and copies **everything**, because a maintenance
/// operation that silently dropped the quads the operator happens not to be cleared for
/// would be a data-loss bug wearing a security feature's clothes.
///
/// # Cost
///
/// Time and disk proportional to the live data, and the result is a second store — so the
/// machine needs room for both. It is an offline operation: writes to the source while this
/// runs are not copied.
fn compact(engine: &Engine, opts: &Options) -> Result<()> {
    let destination = opts
        .to
        .as_deref()
        .context("--to <DIR> says where to write the compacted store")?;
    let destination = Path::new(destination);
    anyhow::ensure!(
        !destination.exists(),
        "{} already exists; compaction writes a new store rather than editing one in place,          so that a failure leaves the original untouched",
        destination.display()
    );

    // Before anything is written. A compaction of a large store takes a long time, and
    // discovering at the end that the disk is full wastes all of it and leaves a partial
    // store behind to clean up.
    check_headroom(engine, opts.store.as_deref(), destination, opts.force)?;

    let source = engine.store();
    let quads_before = source.len();
    let terms_before = source.dictionary_len();
    let graphs_before = source.named_graphs()?.len();

    let mut fresh = opts.open_store_at(destination)?;
    fresh.begin_bulk_load()?;

    // Empty named graphs first: a graph with no quads exists in the store and would
    // otherwise vanish, which the Graph Store Protocol can tell the difference between.
    for id in source.named_graphs()? {
        let Some(term) = source.decode_term(id)? else {
            continue;
        };
        let name = match term {
            oxrdf::Term::NamedNode(n) => oxrdf::GraphName::NamedNode(n),
            oxrdf::Term::BlankNode(b) => oxrdf::GraphName::BlankNode(b),
            _ => continue,
        };
        fresh.insert_named_graph(&name)?;
    }

    let mut copied = 0usize;
    for quad in source.iter() {
        fresh.insert(quad?.as_ref())?;
        copied += 1;
    }

    fresh.end_bulk_load()?;
    fresh.flush()?;

    let quads_after = fresh.len();
    let terms_after = fresh.dictionary_len();
    let graphs_after = fresh.named_graphs()?.len();

    // Checked rather than reported. A compaction that quietly lost quads would be the worst
    // possible outcome of a maintenance command, so it fails loudly instead of printing a
    // reassuring summary.
    anyhow::ensure!(
        copied == quads_before && quads_after == quads_before,
        "compaction did not preserve the data: {quads_before} quads in, {copied} copied,          {quads_after} in the result. {} has been left in place for inspection.",
        destination.display()
    );
    anyhow::ensure!(
        graphs_after == graphs_before,
        "compaction lost named graphs: {graphs_before} before, {graphs_after} after"
    );

    let reclaimed = terms_before.saturating_sub(terms_after);
    println!(
        "compacted {quads_before} quads into {}",
        destination.display()
    );
    println!("  dictionary  {terms_before} -> {terms_after}  ({reclaimed} reclaimed)");
    println!("  graphs      {graphs_before}");
    println!();
    println!("the source is untouched. swap by stopping the server, moving the old store");
    println!("aside, and pointing --store at the new one; keep the old until a query or two");
    println!("has confirmed the new one, since this is the operation you least want to");
    println!("discover was wrong a week later");
    Ok(())
}

/// Materialises the RDFS closure of the store.
///
/// # What it is for
///
/// A store answers what is *in* it. `ex:father rdfs:subPropertyOf ex:parent` says that every
/// use of the first is a use of the second, and no query acts on that — so a query for
/// `ex:parent` misses everyone who only has an `ex:father`. The most concrete instance is
/// GeoSPARQL: the OGC example attaches geometries with a sub-property of `geo:hasGeometry`,
/// and §17's rewrite looks for `geo:hasGeometry`, so a feature-level query returns the
/// geometries rather than the features they belong to.
///
/// # It writes to its own graph
///
/// Not into the data. An inference can then be dropped again with `DROP GRAPH`, a reader can
/// tell it from something somebody asserted, and access policy has something to name. The
/// cost is that queries see it only under the union default graph or by naming it, which is
/// the trade rather than an oversight.
///
/// Running it twice adds nothing the second time.
fn entail(engine: &mut Engine, opts: &Options) -> Result<()> {
    let iri = opts
        .entail_graph
        .as_deref()
        .unwrap_or(holos_engine::entailment::DEFAULT_GRAPH_IRI);
    let budget = if opts.entail_budget == 0 {
        holos_engine::entailment::DEFAULT_BUDGET
    } else {
        opts.entail_budget
    };

    // The graph name has to be a term id, and interning it is the only way to get one.
    let graph = engine
        .store_mut()
        .encode_quad(
            oxrdf::Quad {
                subject: oxrdf::NamedNode::new(iri)?.into(),
                predicate: oxrdf::NamedNode::new_unchecked(
                    "https://holos.dev/ns#entailmentGraphMarker",
                ),
                object: oxrdf::Term::NamedNode(oxrdf::NamedNode::new(iri)?),
                graph_name: oxrdf::GraphName::DefaultGraph,
            }
            .as_ref(),
        )?
        .object;

    let mut session = opts.session(engine)?;
    let before = engine.store().len();
    let report = holos_engine::entailment::materialise(engine, &mut session, Some(graph), budget)?;
    engine.store_mut().flush()?;

    println!("entailed {} triple(s) into <{iri}>", report.added);
    println!("  rounds  {}", report.rounds);
    println!("  store   {before} -> {} quads", engine.store().len());
    println!();
    println!("they are in a graph of their own, so a query sees them only with");
    println!("--default-graph <{iri}> alongside the default one, and");
    println!("`DROP GRAPH <{iri}>` undoes this exactly");
    Ok(())
}

fn stats(engine: &Engine, opts: &Options) -> Result<()> {
    let store = engine.store();
    println!("quads            {}", store.len());
    println!("dictionary terms {}", store.dictionary_len());
    println!("named graphs     {}", store.named_graphs()?.len());

    // Disk, for a persistent store. Reported here so an operator can size headroom without
    // starting a maintenance command to find out whether it will fit — which is how the
    // question usually gets asked, and the worst time to ask it.
    if let Some(path) = opts.store.as_deref() {
        let used = store.on_disk_bytes().unwrap_or(0);
        println!("on disk          {}", human(used));
        if let Some(free) = available_at(Path::new(path)) {
            println!("free here        {}", human(free));
            // `compact` writes a second store beside the first, so that is the number that
            // decides whether maintenance is possible at all.
            let verdict = if free >= used { "yes" } else { "NO" };
            println!("room to compact  {verdict} (needs up to {})", human(used));
        }
    }
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
        let (Some(s), Some(p), Some(o)) = (solution.get("s"), solution.get("p"), solution.get("o"))
        else {
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

    // "vendored" was the earlier spelling; still accepted so a script written against it
    // does not break on a wording change.
    if matches!(opts.engine.as_deref(), Some("adapted" | "vendored")) {
        let started = std::time::Instant::now();
        let mut run = holos_shacl::engine::EngineRun::prepare(
            engine.store(),
            ShaclOptions {
                data_graph: GraphFilter::Default,
                shapes_graph,
            },
        )?;
        let prepared = started.elapsed();
        let started = std::time::Instant::now();
        let report = run.validate()?;
        let validated = started.elapsed();
        println!(
            "bridged+compiled  {} triples, {} shapes in {:.3}s",
            run.triples(),
            run.shapes(),
            prepared.as_secs_f64()
        );
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

    println!(
        "shapes compiled   {} shapes in {:.3}s",
        shapes.shapes().len(),
        compiled.as_secs_f64()
    );
    println!("validated         {:.3}s", validated.as_secs_f64());
    println!("conforms          {}", report.conforms);
    println!("results           {}", report.results.len());

    if opts.report {
        let quads = shapes.report_to_quads(engine.store(), &report)?;
        let mut out = stdout().lock();
        for quad in quads {
            writeln!(
                out,
                "{} .",
                oxrdf::Triple {
                    subject: quad.subject,
                    predicate: quad.predicate,
                    object: quad.object,
                }
            )?;
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
    /// Destination for `holos backup`.
    to: Option<String>,
    /// Proceed with a maintenance command whose disk-headroom estimate says it will
    /// not fit. The estimate is the source's size and is deliberately generous.
    force: bool,
    /// Where `holos entail` writes its conclusions.
    entail_graph: Option<String>,
    /// How many new triples `holos entail` may add before it refuses.
    entail_budget: usize,
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
                "--to" => o.to = Some(value(&mut i)?),
                "--force" => o.force = true,
                "--entail-graph" => o.entail_graph = Some(value(&mut i)?),
                "--entail-budget" => o.entail_budget = value(&mut i)?.parse()?,
                "--bulk" => o.bulk = true,
                "--shapes" => o.shapes = Some(value(&mut i)?),
                "--report" => o.report = true,
                "--engine" => o.engine = Some(value(&mut i)?),
                "--fail-closed" => o.fail_closed = true,
                "--audit" => o.audit = true,
                other => bail!(
                    "unknown flag `{other}`

{USAGE}"
                ),
            }
            i += 1;
        }
        Ok(o)
    }

    /// Opens a second, empty store at `path`, on the same backend as `--store`.
    fn open_store_at(&self, path: &Path) -> Result<holos_store::Store> {
        #[cfg(feature = "rocksdb")]
        {
            let storage = holos_store::RocksStorage::open(path)
                .with_context(|| format!("creating a store at {}", path.display()))?;
            Ok(holos_store::Store::with_storage(storage))
        }
        #[cfg(not(feature = "rocksdb"))]
        {
            let _ = path;
            bail!("this build has no persistent backend: rebuild with --features rocksdb")
        }
    }

    /// Opens the engine over the requested backend.
    fn open_engine(&self) -> Result<Engine> {
        match &self.store {
            None => Ok(Engine::new()),
            #[cfg(feature = "rocksdb")]
            Some(path) => {
                let storage = holos_store::RocksStorage::open(path)
                    .with_context(|| format!("opening the store at {path}"))?;
                Ok(Engine::with_store(holos_store::Store::with_storage(
                    storage,
                )))
            }
            #[cfg(not(feature = "rocksdb"))]
            Some(_) => {
                bail!("this build has no persistent backend: rebuild with --features rocksdb")
            }
        }
    }

    fn begin_bulk(&self, engine: &mut Engine) -> Result<()> {
        if self.bulk {
            engine.store_mut().begin_bulk_load()?;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn verdict(needed: u64, available: u64, force: bool) -> Result<Option<String>> {
        headroom_verdict("/store", Path::new("/backup"), needed, available, force)
    }

    #[test]
    fn room_to_spare_is_no_comment() {
        assert!(verdict(100, 200, false).expect("fits").is_none());
        // Exactly enough is enough. A `>` here would refuse a store that fits precisely.
        assert!(verdict(100, 100, false).expect("fits exactly").is_none());
    }

    #[test]
    fn not_enough_room_is_refused() {
        let refusal = verdict(200, 100, false)
            .expect_err("should refuse")
            .to_string();
        // The message has to carry both numbers, because "not enough room" without them
        // leaves the operator to go and measure the thing the command just measured.
        assert!(refusal.contains("200 B"), "{refusal}");
        assert!(refusal.contains("100 B"), "{refusal}");
        assert!(refusal.contains("--force"), "{refusal}");
    }

    #[test]
    fn force_downgrades_the_refusal_to_a_warning() {
        let warning = verdict(200, 100, true)
            .expect("force should not refuse")
            .expect("and should still say something");
        assert!(warning.starts_with("warning:"), "{warning}");
    }

    /// A source that measured as nothing is a store that was never written, or a path that
    /// could not be read. Refusing on that would block the command for a reason that has
    /// nothing to do with disk space.
    #[test]
    fn an_unmeasurable_source_does_not_refuse() {
        assert!(verdict(0, 0, false).expect("no estimate").is_none());
    }

    #[test]
    fn sizes_are_rendered_for_a_person() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(1024), "1.0 KiB");
        assert_eq!(human(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(human(1024 * 1024 * 1024), "1.0 GiB");
    }
}
