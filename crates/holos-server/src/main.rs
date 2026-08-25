//! `holos-server` — SPARQL 1.2 Protocol over HTTP, with a YASGUI console.
//!
//! HOLOS L6 (`DESIGN.md` §10). Four things worth knowing about how this is built:
//!
//! **It authenticates nobody.** `DESIGN.md` §14.5 puts token verification, Kerberos, mTLS
//! and SAML at the edge, and this server is behind that edge. It reads *already-verified*
//! claims from headers a front door is expected to set — and refuses to do so at all
//! unless started with `--trust-forwarded-identity`, because trusting those headers on an
//! open port would let any client name its own roles.
//!
//! **Every request opens a session.** There is no path from a request to the data that
//! does not go through a [`Session`], so the policy chokepoint of §14 covers HTTP for free.
//!
//! **Reads and writes are separated by an `RwLock`.** The store is single-writer and
//! many-readers, so a thread-per-request model matches it exactly.
//!
//! **SPARQL Update takes the write lock.** `POST /update` is the only endpoint that does.
//! A writer excludes readers for the update's duration, which is what makes the update's
//! failure-atomicity also behave as isolation in this deployment.

mod http;
mod ui;

use anyhow::{Context, Result};
use holos_engine::{update as sparql_update, Engine, QueryOptions};
use holos_security::{Label, Modes, Policy, Principal, PrincipalMatch, Rule, Scope, Session};
use oxrdf::NamedNode;
use oxrdfio::{RdfFormat, RdfSerializer};
use sparesults::QueryResultsSerializer;
use spareval::QueryResults;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tiny_http::{Header, Request, Response, Server};

const USAGE: &str = "\
holos-server — SPARQL 1.2 over HTTP, with a YASGUI console

USAGE
    holos-server [--listen <ADDR>] [--data <FILE>...] [--store <DIR>] [OPTIONS]

ENDPOINTS
    GET  /                   YASGUI console
    GET  /query?query=...    SPARQL 1.2 Protocol
    POST /query              SPARQL 1.2 Protocol (form-encoded or application/sparql-query)
    POST /update             SPARQL 1.1 Update (form-encoded or application/sparql-update)
    GET  /stats              Store statistics, as JSON
    GET  /health             Liveness

SERVER
    --listen <ADDR>          Default 127.0.0.1:7878
    --data <FILE>            Load a file at start-up. Repeatable. Also reads .gz, streamed.
    --store <DIR>            Use a persistent RocksDB store at DIR.
    --threads <N>            Worker threads. Default 8.
    --timeout <SECONDS>      Abandon a query after this long. Default 0 (no limit).
                             Enforced while a query reads or streams rows; a query blocked
                             inside one in-memory step is not interruptible. See
                             OPERATIONS.md.
    --read-only              Answer 403 to /update. The store is still opened writable, so
                             a loader can use it; this refuses updates over HTTP only.
    --reorder                Order each basic graph pattern by estimated cardinality before
                             evaluating, making query cost independent of how the query was
                             written. Statistics are built once at start-up and rebuilt after
                             an update. Measured 14x on a badly ordered join (BENCHMARKS.md).
    --no-ui                  Do not serve the console. The endpoints still work, and the
                             server then needs no network access of any kind.

IDENTITY  (see DESIGN.md §14.5)
    --trust-forwarded-identity
                             Read the principal from X-Holos-Principal, X-Holos-Roles and
                             X-Holos-Clearance. Only safe behind a front door that sets
                             them and strips whatever a client sent. Off by default.
    --role <NAME>            Give every request this role. Repeatable. For development.
    --clearance <N>          Give every request this clearance level.

POLICY
    --deny-all               Deny by default instead of permit-all.
    --allow-graph <IRI>      Grant read on a named graph. Repeatable.
    --deny-predicate <IRI>   Refuse read on a predicate. Repeatable.
    --label-graph <IRI>=<N>  Classify a graph at level N. Repeatable.
    --fail-closed            Error on refusal rather than filtering silently.
";

struct Config {
    listen: String,
    timeout: Option<Duration>,
    read_only: bool,
    reorder: bool,
    data: Vec<String>,
    store: Option<String>,
    threads: usize,
    ui: bool,
    trust_forwarded: bool,
    roles: Vec<String>,
    clearance: Option<u16>,
    deny_all: bool,
    allow_graphs: Vec<String>,
    deny_predicates: Vec<String>,
    graph_labels: Vec<(String, u16)>,
    fail_closed: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:7878".to_owned(),
            timeout: None,
            read_only: false,
            reorder: false,
            data: Vec::new(),
            store: None,
            threads: 8,
            ui: true,
            trust_forwarded: false,
            roles: Vec::new(),
            clearance: None,
            deny_all: false,
            allow_graphs: Vec::new(),
            deny_predicates: Vec::new(),
            graph_labels: Vec::new(),
            fail_closed: false,
        }
    }
}

struct State {
    engine: RwLock<Engine>,
    policy: Policy,
    config: Config,
    /// Cardinality statistics, when `--reorder` is on.
    ///
    /// A snapshot, replaced after an update. Reading a stale snapshot makes a plan worse,
    /// never wrong — reordering a basic graph pattern cannot change its answer — so a
    /// reader never has to wait for a rebuild.
    statistics: RwLock<Option<Arc<holos_stats::Statistics>>>,
}

impl State {
    /// Rebuilds the statistics snapshot, if reordering is on.
    fn refresh_statistics(&self) {
        if !self.config.reorder {
            return;
        }
        let built = {
            let Ok(engine) = self.engine.read() else {
                return;
            };
            holos_stats::Statistics::build(engine.store(), holos_store::GraphFilter::Default)
        };
        match built {
            Ok(stats) => {
                if let Ok(mut slot) = self.statistics.write() {
                    *slot = Some(Arc::new(stats));
                }
            }
            // Losing the statistics costs speed, not correctness, so a failure here is
            // reported and the server carries on without them.
            Err(e) => eprintln!("statistics could not be rebuilt: {e}"),
        }
    }

    fn statistics(&self) -> Option<Arc<holos_stats::Statistics>> {
        self.statistics.read().ok().and_then(|s| s.clone())
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    let config = parse_args(&args)?;

    let mut engine = open_engine(&config)?;
    for path in &config.data {
        let format = format_for(path)?;
        let reader = holos_engine::source::reader(std::path::Path::new(path))?;
        let n = engine.bulk_load(reader, format, None)?;
        eprintln!("loaded {n} quads from {path}");
    }

    let policy = build_policy(&config)?;
    let listen = config.listen.clone();
    let threads = config.threads;
    let ui_enabled = config.ui;
    let state = Arc::new(State {
        engine: RwLock::new(engine),
        policy,
        config,
        statistics: RwLock::new(None),
    });
    if state.config.reorder {
        let started = std::time::Instant::now();
        state.refresh_statistics();
        eprintln!(
            "  reorder  statistics built in {:.2}s",
            started.elapsed().as_secs_f64()
        );
    }

    let server = Arc::new(Server::http(&listen).map_err(|e| anyhow::anyhow!("{e}"))?);
    eprintln!("holos-server listening on http://{listen}");
    if ui_enabled {
        eprintln!("  console  http://{listen}/");
    }
    eprintln!("  query    http://{listen}/query");
    if !state.config.trust_forwarded {
        eprintln!(
            "  identity forwarded headers are NOT trusted; every request is anonymous \
             (--trust-forwarded-identity to change)"
        );
    }

    let mut workers = Vec::with_capacity(threads);
    for _ in 0..threads {
        let server = Arc::clone(&server);
        let state = Arc::clone(&state);
        workers.push(std::thread::spawn(move || {
            // A worker that dies takes its thread with it, not the server; a panic while
            // answering one request must not close the port for everyone else.
            while let Ok(request) = server.recv() {
                if let Err(e) = dispatch(&state, request) {
                    eprintln!("request failed: {e:#}");
                }
            }
        }));
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

fn dispatch(state: &State, mut request: Request) -> Result<()> {
    let (path, params) = http::split_url(request.url());
    let accept = header(&request, "accept");
    let content_type = header(&request, "content-type");

    match (request.method().as_str(), path.as_str()) {
        ("GET", "/health") => respond(request, 200, "text/plain", b"ok".to_vec()),
        ("GET", "/") if state.config.ui => {
            let page = ui::console("/query", "HOLOS");
            respond(request, 200, "text/html; charset=utf-8", page.into_bytes())
        }
        ("GET", "/stats") => {
            let body = stats(state);
            respond(request, 200, "application/json", body.into_bytes())
        }
        ("GET", "/query") => {
            let Some(query) = params.get("query") else {
                return respond(
                    request,
                    400,
                    "text/plain",
                    b"missing the 'query' parameter".to_vec(),
                );
            };
            answer(state, request, &query.clone(), accept.as_deref(), &params)
        }
        ("POST", "/query") => {
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            // Two POST encodings are allowed: the query directly, or form-encoded.
            let query = if content_type
                .as_deref()
                .is_some_and(|c| c.starts_with("application/sparql-query"))
            {
                body
            } else {
                match http::parse_form(&body).get("query") {
                    Some(q) => q.clone(),
                    None => {
                        return respond(
                            request,
                            400,
                            "text/plain",
                            b"missing the 'query' parameter".to_vec(),
                        )
                    }
                }
            };
            answer(state, request, &query, accept.as_deref(), &params)
        }
        ("POST", "/update") => {
            if state.config.read_only {
                return respond(
                    request,
                    403,
                    "text/plain",
                    b"this endpoint is read-only".to_vec(),
                );
            }
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            let (update, form) = if content_type
                .as_deref()
                .is_some_and(|c| c.starts_with("application/sparql-update"))
            {
                (body, std::collections::HashMap::new())
            } else {
                let form = http::parse_form(&body);
                match form.get("update") {
                    Some(u) => (u.clone(), form),
                    None => {
                        return respond(
                            request,
                            400,
                            "text/plain",
                            b"missing the 'update' parameter".to_vec(),
                        )
                    }
                }
            };
            // The protocol names the update's dataset with `using-graph-uri`, in the query
            // string or the form body.
            let mut merged = params;
            for (k, v) in form {
                merged.entry(k).or_insert(v);
            }
            apply_update(state, request, &update, &merged)
        }
        ("OPTIONS", _) => respond(request, 204, "text/plain", Vec::new()),
        _ => respond(request, 404, "text/plain", b"not found".to_vec()),
    }
}

/// Runs one query under a session built from the request.
fn answer(
    state: &State,
    request: Request,
    query: &str,
    accept: Option<&str>,
    params: &std::collections::HashMap<String, String>,
) -> Result<()> {
    let principal = principal_for(state, &request);
    let guard = state.engine.read().map_err(|_| anyhow::anyhow!("poisoned"))?;
    let session = match Session::open(guard.store(), principal, state.policy.clone()) {
        Ok(s) => s,
        Err(e) => {
            return respond(
                request,
                500,
                "text/plain",
                format!("opening a session: {e}").into_bytes(),
            )
        }
    };
    let view = guard.view(&session);

    let options = match query_options(state, params) {
        Ok(options) => options,
        Err(e) => return respond(request, 400, "text/plain", e.into_bytes()),
    };
    let explain = options.explain;

    let (results, explanation) = match Engine::query_with(&view, query, &options) {
        Ok(pair) => pair,
        Err(e) => {
            let status = if matches!(e, holos_engine::EngineError::Syntax(_)) {
                400
            } else {
                500
            };
            return respond(request, status, "text/plain", e.to_string().into_bytes());
        }
    };

    // `?explain` returns the plan instead of the answers. The results still have to be
    // drained, because the per-operator statistics are only filled in as rows flow.
    if explain {
        if let Some(explanation) = explanation {
            drain(results);
            let mut json = Vec::new();
            explanation.write_in_json(&mut json)?;
            return respond(request, 200, "application/json", json);
        }
    }

    serialise(request, results, accept)
}

/// Consumes a result stream for its side effect on the explanation's statistics.
fn drain(results: QueryResults<'_>) {
    match results {
        QueryResults::Solutions(iter) => {
            for solution in iter {
                if solution.is_err() {
                    return;
                }
            }
        }
        QueryResults::Graph(iter) => {
            for triple in iter {
                if triple.is_err() {
                    return;
                }
            }
        }
        QueryResults::Boolean(_) => {}
    }
}

/// Builds the query options the SPARQL Protocol's parameters ask for.
///
/// `default-graph-uri` and `named-graph-uri` were previously parsed and then ignored, so a
/// client asking for one dataset silently got answers over another. They are repeatable in
/// the protocol; `http::parse_form` keeps the first of a repeated key, so multiples arrive
/// space-separated from the query string helper instead.
fn query_options(
    state: &State,
    params: &std::collections::HashMap<String, String>,
) -> Result<QueryOptions, String> {
    let mut options = QueryOptions::new();
    if let Some(timeout) = state.config.timeout {
        options = options.with_timeout(timeout);
    }
    if let Some(stats) = state.statistics() {
        options = options.reordering(stats);
    }
    for iri in http::values(params, "default-graph-uri") {
        let node = NamedNode::new(&iri).map_err(|e| format!("default-graph-uri `{iri}`: {e}"))?;
        options = options.with_default_graph(node.into());
    }
    for iri in http::values(params, "named-graph-uri") {
        let node = NamedNode::new(&iri).map_err(|e| format!("named-graph-uri `{iri}`: {e}"))?;
        options = options.with_named_graph(node.into());
    }
    if params.contains_key("explain") {
        options = options.explaining();
    }
    Ok(options)
}

/// Applies a SPARQL update, holding the write lock for its duration.
fn apply_update(
    state: &State,
    request: Request,
    update: &str,
    params: &std::collections::HashMap<String, String>,
) -> Result<()> {
    let principal = principal_for(state, &request);
    let mut guard = state
        .engine
        .write()
        .map_err(|_| anyhow::anyhow!("poisoned"))?;
    let mut session = match Session::open(guard.store(), principal, state.policy.clone()) {
        Ok(s) => s,
        Err(e) => {
            return respond(
                request,
                500,
                "text/plain",
                format!("opening a session: {e}").into_bytes(),
            )
        }
    };
    // `using-graph-uri` is accepted and reported rather than silently dropped: honouring it
    // needs the USING clause plumbed through spargebra's operation, and answering "not
    // supported" beats answering over the wrong dataset.
    if params.contains_key("using-graph-uri") || params.contains_key("using-named-graph-uri") {
        return respond(
            request,
            400,
            "text/plain",
            b"using-graph-uri is not supported; put USING in the update text".to_vec(),
        );
    }

    let outcome = sparql_update::update(&mut guard, &mut session, update, None);
    // Release the write lock before rebuilding: refresh_statistics takes a read lock, and
    // holding the write lock across it would deadlock.
    drop(guard);
    if outcome.is_ok() {
        state.refresh_statistics();
    }

    match outcome {
        Ok(outcome) => {
            let body = format!(
                r#"{{"inserted":{},"deleted":{},"graphsCreated":{},"graphsDropped":{}}}"#,
                outcome.inserted, outcome.deleted, outcome.graphs_created, outcome.graphs_dropped
            );
            respond(request, 200, "application/json", body.into_bytes())
        }
        Err(e) => {
            let status = match &e {
                holos_engine::EngineError::Syntax(_) => 400,
                holos_engine::EngineError::AccessDenied => 403,
                _ => 500,
            };
            respond(request, status, "text/plain", e.to_string().into_bytes())
        }
    }
}

/// Writes results in the negotiated format.
fn serialise(request: Request, results: QueryResults<'_>, accept: Option<&str>) -> Result<()> {
    match results {
        QueryResults::Graph(triples) => {
            let format = http::negotiate_rdf(accept);
            let mut writer = RdfSerializer::from_format(format).for_writer(Vec::new());
            for triple in triples {
                writer.serialize_triple(triple?.as_ref())?;
            }
            let body = writer.finish()?;
            respond(request, 200, format.media_type(), body)
        }
        QueryResults::Boolean(value) => {
            let format = http::negotiate_results(accept);
            let body = QueryResultsSerializer::from_format(format)
                .serialize_boolean_to_writer(Vec::new(), value)?;
            respond(request, 200, http::results_media_type(format), body)
        }
        QueryResults::Solutions(solutions) => {
            let format = http::negotiate_results(accept);
            let variables = solutions.variables().to_vec();
            let mut writer = QueryResultsSerializer::from_format(format)
                .serialize_solutions_to_writer(Vec::new(), variables)?;
            for solution in solutions {
                writer.serialize(&solution?)?;
            }
            let body = writer.finish()?;
            respond(request, 200, http::results_media_type(format), body)
        }
    }
}

/// Builds the principal for a request.
///
/// Forwarded identity headers are read **only** when the operator turned them on. Reading
/// them by default would let any client on the port name its own roles and clearance,
/// which is the opposite of what §14 is for.
fn principal_for(state: &State, request: &Request) -> Principal {
    let mut principal = if state.config.trust_forwarded {
        match header(request, "x-holos-principal") {
            Some(id) => Principal::new(NamedNode::new_unchecked(format!(
                "urn:holos:principal:forwarded/{}",
                id.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_', "_")
            ))),
            None => Principal::anonymous(),
        }
    } else {
        Principal::anonymous()
    };

    if state.config.trust_forwarded {
        if let Some(roles) = header(request, "x-holos-roles") {
            for role in roles.split(',').map(str::trim).filter(|r| !r.is_empty()) {
                principal = principal.with_role(role);
            }
        }
        if let Some(level) = header(request, "x-holos-clearance").and_then(|c| c.trim().parse().ok())
        {
            principal = principal.with_clearance(Label::level(level));
        }
    }
    for role in &state.config.roles {
        principal = principal.with_role(role);
    }
    if let Some(level) = state.config.clearance {
        principal = principal.with_clearance(Label::level(level));
    }
    principal
}

fn stats(state: &State) -> String {
    let Ok(guard) = state.engine.read() else {
        return r#"{"error":"store lock poisoned"}"#.to_owned();
    };
    let store = guard.store();
    let graphs = store.named_graphs().map(|g| g.len()).unwrap_or(0);
    format!(
        r#"{{"quads":{},"dictionaryTerms":{},"namedGraphs":{}}}"#,
        store.len(),
        store.dictionary_len(),
        graphs
    )
}

fn respond(request: Request, status: u16, content_type: &str, body: Vec<u8>) -> Result<()> {
    let mut response = Response::from_data(body).with_status_code(status);
    for (name, value) in [
        ("Content-Type", content_type),
        // The console is served from this same origin, but a SPARQL endpoint is routinely
        // queried from a page somewhere else, and refusing that by default makes the
        // endpoint useless for its most common job.
        ("Access-Control-Allow-Origin", "*"),
        ("Access-Control-Allow-Headers", "content-type, accept"),
        ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
    ] {
        if let Ok(header) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            response.add_header(header);
        }
    }
    request.respond(response)?;
    Ok(())
}

fn header(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_owned())
}

fn open_engine(config: &Config) -> Result<Engine> {
    match &config.store {
        None => Ok(Engine::new()),
        #[cfg(feature = "rocksdb")]
        Some(path) => {
            let storage = holos_store::RocksStorage::open(path)
                .with_context(|| format!("opening the store at {path}"))?;
            Ok(Engine::with_store(holos_store::Store::with_storage(storage)))
        }
        #[cfg(not(feature = "rocksdb"))]
        Some(_) => anyhow::bail!("this build has no persistent backend"),
    }
}

fn build_policy(config: &Config) -> Result<Policy> {
    let mut policy = if config.deny_all {
        Policy::default()
    } else {
        Policy::permit_all()
    };
    if config.fail_closed {
        policy = policy.with_semantics(holos_security::Semantics::Fail);
    }
    for iri in &config.allow_graphs {
        policy = policy.with_rule(Rule::allow(
            Modes::READ,
            Scope::Graph(iri_arg(iri)?),
            PrincipalMatch::Everyone,
        ));
    }
    for iri in &config.deny_predicates {
        policy = policy.with_rule(Rule::deny(
            Modes::READ,
            Scope::Predicate(iri_arg(iri)?),
            PrincipalMatch::Everyone,
        ));
    }
    for (iri, level) in &config.graph_labels {
        policy = policy.with_graph_label(iri_arg(iri)?, Label::level(*level));
    }
    Ok(policy)
}

fn iri_arg(s: &str) -> Result<NamedNode> {
    NamedNode::new(s).with_context(|| format!("`{s}` is not a valid IRI"))
}

/// The RDF format a file name implies, seeing through a `.gz` suffix.
fn format_for(path: &str) -> Result<RdfFormat> {
    holos_engine::source::format_for_path(std::path::Path::new(path)).with_context(|| {
        format!(
            "cannot infer an RDF format from `{path}`; expected .ttl .nt .trig .nq .rdf .n3 \
             .jsonld, optionally with .gz"
        )
    })
}

#[allow(clippy::too_many_lines)]
fn parse_args(args: &[String]) -> Result<Config> {
    let mut c = Config::default();
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].clone();
        let value = |i: &mut usize| -> Result<String> {
            *i += 1;
            args.get(*i)
                .cloned()
                .with_context(|| format!("{flag} needs a value"))
        };
        match args[i].as_str() {
            "--listen" => c.listen = value(&mut i)?,
            "--data" => c.data.push(value(&mut i)?),
            "--store" => c.store = Some(value(&mut i)?),
            "--threads" => c.threads = value(&mut i)?.parse()?,
            "--timeout" => {
                let seconds: f64 = value(&mut i)?.parse()?;
                c.timeout = if seconds > 0.0 {
                    Some(Duration::from_secs_f64(seconds))
                } else {
                    None
                };
            }
            "--read-only" => c.read_only = true,
            "--reorder" => c.reorder = true,
            "--no-ui" => c.ui = false,
            "--trust-forwarded-identity" => c.trust_forwarded = true,
            "--role" => c.roles.push(value(&mut i)?),
            "--clearance" => c.clearance = Some(value(&mut i)?.parse()?),
            "--deny-all" => c.deny_all = true,
            "--allow-graph" => c.allow_graphs.push(value(&mut i)?),
            "--deny-predicate" => c.deny_predicates.push(value(&mut i)?),
            "--label-graph" => {
                let raw = value(&mut i)?;
                let (iri, level) = raw
                    .rsplit_once('=')
                    .with_context(|| format!("--label-graph wants <IRI>=<N>, got `{raw}`"))?;
                c.graph_labels.push((iri.to_owned(), level.parse()?));
            }
            "--fail-closed" => c.fail_closed = true,
            other => anyhow::bail!("unknown flag `{other}`\n\n{USAGE}"),
        }
        i += 1;
    }
    Ok(c)
}
