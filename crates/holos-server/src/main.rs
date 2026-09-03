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

mod gsp;
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

GRAPH STORE PROTOCOL      https://www.w3.org/TR/sparql11-http-rdf-update/
    GET    /graph?graph=<IRI>    Serialise a graph (or ?default)
    HEAD   /graph?graph=<IRI>    As GET, without the body
    PUT    /graph?graph=<IRI>    Replace a graph with the request body
    POST   /graph?graph=<IRI>    Merge the request body into a graph
    DELETE /graph?graph=<IRI>    Remove a graph
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
    --read-only              Answer 403 to /update and to every writing Graph Store
                             Protocol verb. The store is still opened writable, so a loader
                             can use it; this refuses writes over HTTP only.
    --gsp-path <PATH>        Where the Graph Store Protocol is served. Default /graph.
    --gsp-base <IRI>         Enable *direct* graph identification, treating the request path
                             under this base as the graph name. Off by default: a server
                             behind a proxy does not know its own external base, and
                             guessing would mint names that do not match what clients ask
                             for later.
    --reorder                Order each basic graph pattern by estimated cardinality before
                             evaluating, making query cost independent of how the query was
                             written. Statistics are built once at start-up and rebuilt after
                             an update. Measured 14x on a badly ordered join (BENCHMARKS.md).
    --no-ui                  Do not serve the console. The endpoints still work, and the
                             server then needs no network access of any kind.

MAINTENANCE
    --backup-dir <DIR>       Parent directory for `POST /backup` checkpoints. The endpoint
                             does not exist unless this and --backup-role are both set: a
                             client never names a path, so it cannot be aimed at one.
    --backup-role <ROLE>     Role a principal must hold to trigger a backup.
    --purge-role <ROLE>      Role a principal must hold to call POST /maintenance/purge,
                             which reclaims spatial index entries for geometries no longer
                             referenced by any quad. There is no timer: schedule it with
                             cron or a systemd timer, as with backups.

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
    gsp_base: Option<String>,
    gsp_path: String,
    data: Vec<String>,
    store: Option<String>,
    /// Where `POST /backup` writes checkpoints. `None` disables the endpoint entirely.
    backup_dir: Option<String>,
    /// The role a principal must hold to trigger a backup. `None` disables the endpoint.
    backup_role: Option<String>,
    /// The role a principal must hold to purge the spatial index. `None` disables the
    /// endpoint, on the same reasoning as `backup_role`.
    purge_role: Option<String>,
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
            gsp_base: None,
            gsp_path: "/graph".to_owned(),
            data: Vec::new(),
            store: None,
            backup_dir: None,
            backup_role: None,
            purge_role: None,
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
    /// The spatial index, rebuilt on every write alongside the statistics.
    ///
    /// Rebuilding rather than maintaining incrementally, for now: it is the same shape as
    /// the statistics beside it, and it is what keeps the index *current*, which is the
    /// condition the query path checks before it will use one at all.
    spatial: RwLock<Option<Arc<holos_engine::spatial::SpatialIndex>>>,
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

    /// Brings the spatial index up to date with the store.
    ///
    /// Called wherever the statistics are refreshed, and for the same reason: a write has
    /// happened, so anything derived from the store is out of date. An index that is not
    /// refreshed is not *wrong* — the query path notices it no longer describes the store and
    /// does the full scan instead — but it stops narrowing anything, so refreshing is what
    /// makes it worth having.
    ///
    /// Updates the existing index rather than replacing it. A rebuild re-decodes and
    /// re-parses every geometry in the store, which is 57% of its cost and all of it wasted
    /// on geometries that have not changed; a refresh pays that only for terms it has not
    /// seen before. The first call, when there is no index yet, still builds one.
    fn refresh_spatial(&self) {
        let Ok(engine) = self.engine.read() else {
            return;
        };
        let existing = self.spatial.read().ok().and_then(|slot| slot.clone());
        let outcome = match existing {
            Some(index) => index.refresh(engine.store()),
            None => holos_engine::spatial::SpatialIndex::build(engine.store()).map(|built| {
                if let Ok(mut slot) = self.spatial.write() {
                    *slot = Some(Arc::new(built));
                }
            }),
        };
        // Losing the index costs speed, not correctness: without one, topology relations are
        // evaluated by scanning, which is what they did before it existed.
        if let Err(e) = outcome {
            eprintln!("the spatial index could not be brought up to date: {e}");
        }
    }

    fn spatial(&self) -> Option<Arc<holos_engine::spatial::SpatialIndex>> {
        self.spatial.read().ok().and_then(|s| s.clone())
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
        spatial: RwLock::new(None),
    });
    // Built unconditionally, and deliberately not inside the `--reorder` block below. The
    // two were coupled, which meant a server started without `--reorder` — the default — had
    // no spatial index at all until its first write, and every GeoSPARQL query until then
    // did a full scan. Reordering and spatial routing are unrelated features that happened
    // to be refreshed together.
    state.refresh_spatial();
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
    // Kept in its encoded form as well: a duplicate parameter has to be spotted before the
    // parser collapses it, and the protocol makes a duplicated `query` a client error.
    let query_string = request
        .url()
        .split_once('?')
        .map_or_else(String::new, |(_, q)| q.to_owned());
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
            if let Some(why) = repeated(&query_string, "query") {
                return respond(request, 400, "text/plain", why.into_bytes());
            }
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
            if let Some(why) = repeated(&query_string, "query") {
                return respond(request, 400, "text/plain", why.into_bytes());
            }
            if let Some(why) = unusable_body(content_type.as_deref()) {
                return respond(request, 400, "text/plain", why.into_bytes());
            }
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            // Two POST encodings are allowed: the query directly, or form-encoded.
            let query = if content_type
                .as_deref()
                .is_some_and(|c| c.starts_with("application/sparql-query"))
            {
                body
            } else {
                if let Some(why) = repeated(&body, "query") {
                    return respond(request, 400, "text/plain", why.into_bytes());
                }
                let form = http::parse_form(&body);
                match form.get("query") {
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
            if let Some(why) = repeated(&query_string, "update") {
                return respond(request, 400, "text/plain", why.into_bytes());
            }
            if let Some(why) = unusable_body(content_type.as_deref()) {
                return respond(request, 400, "text/plain", why.into_bytes());
            }
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body)?;
            let (update, form) = if content_type
                .as_deref()
                .is_some_and(|c| c.starts_with("application/sparql-update"))
            {
                (body, std::collections::HashMap::new())
            } else {
                if let Some(why) = repeated(&body, "update") {
                    return respond(request, 400, "text/plain", why.into_bytes());
                }
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
        ("POST", "/backup") => backup(state, request),
        ("POST", "/maintenance/purge") => purge(state, request),
        ("OPTIONS", _) => respond(request, 204, "text/plain", Vec::new()),
        (method, p)
            if matches!(method, "GET" | "HEAD" | "PUT" | "POST" | "DELETE")
                && is_graph_store_path(state, p) =>
        {
            graph_store(state, request, &params)
        }
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
    let guard = state
        .engine
        .read()
        .map_err(|_| anyhow::anyhow!("poisoned"))?;
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
        // A query may hold relative IRIs, which resolve against the endpoint.
        Ok(options) => options.with_base_iri(base_iri(&request, "/query")),
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

/// Whether a path addresses the Graph Store Protocol.
///
/// The configured path exactly, and — when direct graph identification is on — anything
/// beneath it. Direct identification names the graph *with the path itself*, so
/// `/gsp/person/1.ttl` is a request about a different graph from `/gsp/person/2.ttl`, and
/// matching only the exact prefix would 404 every one of them.
///
/// Without `--gsp-base` the sub-paths are not claimed, because nothing could resolve them
/// to a graph name and answering 400 where a 404 belongs would be worse than not matching.
fn is_graph_store_path(state: &State, path: &str) -> bool {
    if path == state.config.gsp_path {
        return true;
    }
    state.config.gsp_base.is_some()
        && path.starts_with(&format!("{}/", state.config.gsp_path.trim_end_matches('/')))
}

/// Takes a consistent snapshot of the store, for an administrator.
///
/// # Why the client cannot name the destination
///
/// It would be the obvious API — `POST /backup?to=/srv/backups/tonight` — and it would be an
/// arbitrary-write primitive: whatever the server process can write, a caller could ask it
/// to fill with a copy of the database. The server owns the parent directory (`--backup-dir`)
/// and mints a timestamped child; the caller chooses nothing but the moment.
///
/// # The guard
///
/// Three conditions, each of which fails closed:
///
/// 1. **The endpoint does not exist** unless both `--backup-dir` and `--backup-role` are set.
///    An unconfigured server answers 404, so the surface is absent rather than merely
///    defended.
/// 2. **The principal must hold the named role.** A backup copies the whole store, ignoring
///    the policy that governs every query — so this is the one operation where the ordinary
///    access controls do not apply, and it needs its own.
/// 3. **Identity has to be trustworthy.** Without `--trust-forwarded-identity` every request
///    is anonymous and holds no roles, so the endpoint answers 403 to everyone. That is the
///    correct default: an unauthenticated server should not be able to be told to copy
///    itself.
/// `POST /maintenance/purge` — reclaim spatial index entries for departed geometries.
///
/// # Why this is a maintenance step and not automatic
///
/// The spatial index tracks the **dictionary**, which never forgets, rather than the store.
/// That is what lets it catch up with a write in a fraction of a millisecond instead of
/// rebuilding — but it means deleting geometries leaves entries behind. Nothing is wrong
/// while they are there: the index is a superset filter, and a geometry with no quads fails
/// to join and contributes no row. It is memory, not correctness.
///
/// **Restarting does not clear them.** The index is rebuilt at startup from the dictionary,
/// so it comes back holding every geometry ever interned. Reclaiming really does need an
/// explicit step, which is why this exists.
///
/// # Scheduling
///
/// There is no timer inside the server. This is an endpoint so that whatever already
/// schedules things on the host — cron, a systemd timer, a Kubernetes CronJob — can call it,
/// the same way `deploy/backup.sh` calls `/backup`. A server that schedules its own
/// maintenance is a server that does something surprising at three in the morning.
///
/// # The guard
///
/// The same shape as [`backup`]: the endpoint does not exist unless `--purge-role` is set,
/// the principal must hold that role, and without `--trust-forwarded-identity` every request
/// is anonymous and holds no roles. Purging cannot destroy data — it only drops derived
/// entries — but it is a whole-store operation whose cost is proportional to the index, and
/// an unauthenticated server should not be able to be told to do work.
fn purge(state: &State, request: Request) -> Result<()> {
    let Some(role) = &state.config.purge_role else {
        // Not 403: a switched-off endpoint should be indistinguishable from one that was
        // never built, so probing cannot map the configuration.
        return respond(request, 404, "text/plain", b"not found".to_vec());
    };

    let principal = principal_for(state, &request);
    if !principal.has_role(role) {
        return respond(
            request,
            403,
            "text/plain",
            format!("a purge requires the `{role}` role").into_bytes(),
        );
    }

    let Some(index) = state.spatial() else {
        return respond(
            request,
            409,
            "text/plain",
            b"there is no spatial index to purge".to_vec(),
        );
    };

    // A read lock: purging reads the store to ask which geometries are still referenced, and
    // mutates only the index, which has its own lock.
    let guard = state
        .engine
        .read()
        .map_err(|_| anyhow::anyhow!("poisoned"))?;
    let outcome = index.purge(guard.store());
    drop(guard);

    match outcome {
        Ok(report) => {
            let body = format!(
                r#"{{"examined":{},"dropped":{},"retained":{}}}"#,
                report.examined, report.dropped, report.retained
            );
            respond(request, 200, "application/json", body.into_bytes())
        }
        Err(e) => respond(request, 500, "text/plain", e.to_string().into_bytes()),
    }
}

fn backup(state: &State, request: Request) -> Result<()> {
    let (Some(dir), Some(role)) = (&state.config.backup_dir, &state.config.backup_role) else {
        // Not 403: an endpoint that is switched off should be indistinguishable from one
        // that was never built, so probing cannot map the configuration.
        return respond(request, 404, "text/plain", b"not found".to_vec());
    };

    let principal = principal_for(state, &request);
    if !principal.has_role(role) {
        return respond(
            request,
            403,
            "text/plain",
            format!("a backup requires the `{role}` role").into_bytes(),
        );
    }

    let destination = std::path::Path::new(dir).join(format!(
        "holos-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    ));
    if let Err(e) = std::fs::create_dir_all(dir) {
        return respond(
            request,
            500,
            "text/plain",
            format!("could not prepare {dir}: {e}").into_bytes(),
        );
    }

    // A read lock: a checkpoint is a read of the store, and blocking writers for its
    // duration would defeat the point of having one.
    let guard = state
        .engine
        .read()
        .map_err(|_| anyhow::anyhow!("poisoned"))?;

    // Refused before it starts, if it cannot fit. This is the endpoint a scheduler calls
    // every night, so it is the one that fills a disk while nobody is watching — and the
    // hard links a checkpoint makes are what stop compaction reclaiming the space, so a
    // backup directory on the store's own filesystem grows until something says no.
    //
    // 507 rather than 500: the request was well formed and the server is healthy. It is the
    // storage that is insufficient, which is exactly what the code means and is a thing a
    // scheduler can alert on differently from a fault.
    if let (Some(needed), Some(available)) = (
        guard.store().on_disk_bytes(),
        fs4::available_space(dir).ok(),
    ) {
        if needed > 0 && available < needed {
            drop(guard);
            return respond(
                request,
                507,
                "text/plain",
                format!(
                    "not enough room for a checkpoint: the store holds {needed} bytes and \
                     {dir} has {available} free"
                )
                .into_bytes(),
            );
        }
    }

    let outcome = guard.store().checkpoint(&destination);
    let quads = guard.store().len();
    drop(guard);

    match outcome {
        Ok(()) => {
            let body = format!(
                r#"{{"path":{},"quads":{quads}}}"#,
                ui::json_string(&destination.display().to_string())
            );
            respond(request, 201, "application/json", body.into_bytes())
        }
        Err(holos_store::StorageError::Unsupported(why)) => {
            // 409 rather than 500: the request was well formed and the server is healthy;
            // this store simply cannot do it right now, or at all.
            respond(request, 409, "text/plain", why.into_bytes())
        }
        Err(e) => respond(request, 500, "text/plain", e.to_string().into_bytes()),
    }
}

/// The Graph Store Protocol.
///
/// One function for all five verbs because they share the target resolution, the policy
/// session and the existence check, and splitting them would mean five copies of the part
/// that has to agree.
fn graph_store(
    state: &State,
    mut request: Request,
    params: &std::collections::HashMap<String, String>,
) -> Result<()> {
    let method = request.method().as_str().to_owned();
    let writing = matches!(method.as_str(), "PUT" | "POST" | "DELETE");

    if writing && state.config.read_only {
        return respond(
            request,
            403,
            "text/plain",
            b"this endpoint is read-only".to_vec(),
        );
    }

    let path = request.url().split('?').next().unwrap_or("/").to_owned();
    // `POST` to the protocol endpoint itself, carrying neither `?graph` nor `?default`,
    // is the specification's "create a graph and tell me its name" (§5.5). It has to be
    // recognised before the ordinary rules run: with a base configured those would resolve
    // the bare endpoint path by direct identification and name the new graph after the
    // endpoint, which is not a graph.
    let minting = method == "POST"
        && path == state.config.gsp_path
        && !params.contains_key("graph")
        && !params.contains_key("default");

    // Set only for a minted graph, and only so the response can carry `Location`.
    let mut minted: Option<String> = None;

    let target = if minting {
        let Some(base) = state.config.gsp_base.as_deref() else {
            return respond(
                request,
                400,
                "text/plain",
                b"creating a graph by POST needs --gsp-base, so the server can name it".to_vec(),
            );
        };
        let name = gsp::mint_graph_name(base, &path);
        minted = Some(name.as_str().to_owned());
        gsp::Target::Named(name)
    } else {
        match gsp::target(params, &path, state.config.gsp_base.as_deref()) {
            Ok(target) => target,
            Err(e) => return respond(request, 400, "text/plain", e.to_string().into_bytes()),
        }
    };

    let accept = header(&request, "accept");
    let content_type = header(&request, "content-type");
    let mut body = Vec::new();
    if writing {
        request.as_reader().read_to_end(&mut body)?;
    }

    // What the body holds, worked out before any lock is taken so an unusable one costs
    // nothing. Ordinarily this is one document; a graph submitted as a file upload arrives
    // as `multipart/form-data` and carries one per part, each with its own prefixes.
    let mut documents: Vec<(Vec<u8>, RdfFormat)> = Vec::new();
    let mut unusable = false;
    match content_type.as_deref().and_then(gsp::multipart_boundary) {
        Some(boundary) => {
            for (part, media) in gsp::multipart_parts(&body, &boundary) {
                match rdf_format_of(Some(&media)) {
                    Some(format) => documents.push((part.to_vec(), format)),
                    // One part this store cannot read makes the whole upload a 415.
                    // Merging the rest would leave the client holding a graph that is not
                    // what it sent, under a status code saying it succeeded.
                    None => unusable = true,
                }
            }
        }
        None => match rdf_format_of(content_type.as_deref()) {
            Some(format) => documents.push((body, format)),
            None => unusable = true,
        },
    }
    let usable = !unusable && !documents.is_empty();

    let principal = principal_for(state, &request);

    // A read takes the read lock; a write takes the write lock. Sharing one path would
    // mean every GET excluding every other request for no reason.
    if writing {
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
                    format!("session: {e}").into_bytes(),
                )
            }
        };
        let existed = match gsp::exists(&guard, &session, &target) {
            Ok(existed) => existed,
            Err(e) => return respond(request, 500, "text/plain", e.to_string().into_bytes()),
        };

        // Decided before a scope is opened, because both answers are refusals rather than
        // writes and an early return past an open scope would leak it.
        if method == "DELETE" && !existed {
            // Not an error: the client learns the graph was not there.
            return respond(request, 404, "text/plain", b"no such graph".to_vec());
        }
        if matches!(method.as_str(), "PUT" | "POST") && !usable {
            return respond(
                request,
                415,
                "text/plain",
                b"unsupported media type: send RDF with a content-type this store parses".to_vec(),
            );
        }

        // One commit scope for the whole request. `PUT` is clear-then-merge, and without
        // this a body that fails to parse — or a quad the policy refuses — leaves the graph
        // cleared and not replaced, having told the client the request failed. Emptying
        // someone's graph is not an acceptable outcome of a 400.
        if let Err(e) = guard.store_mut().begin() {
            return respond(
                request,
                500,
                "text/plain",
                format!("begin: {e}").into_bytes(),
            );
        }

        let outcome = match method.as_str() {
            "DELETE" => {
                // DELETE removes the graph, not just its contents — otherwise a second
                // DELETE cannot answer 404.
                gsp::drop_graph(&mut guard, &mut session, &target).map(|_| 204)
            }
            // PUT replaces, POST merges — see `gsp::write`, which is where the two
            // operations a replace is made of are held together.
            "PUT" | "POST" => gsp::write(
                &mut guard,
                &mut session,
                &target,
                &documents,
                method == "PUT",
            )
            // 201 tells the client it created the graph; 204 that it changed one.
            .map(|_| if existed { 204 } else { 201 }),
            _ => Ok(400),
        };

        // A 400 is still a well-formed answer rather than a failure, so it commits — there
        // is nothing buffered under it. Anything that produced an error discards the batch,
        // which is the whole point.
        let outcome = match (&outcome, guard.store().in_scope()) {
            (Ok(_), true) => match guard.store_mut().commit() {
                Ok(()) => outcome,
                Err(e) => Err(anyhow::anyhow!("commit: {e}")),
            },
            (Err(_), true) => {
                guard.store_mut().rollback();
                outcome
            }
            (_, false) => outcome,
        };

        drop(guard);
        if matches!(method.as_str(), "PUT" | "POST" | "DELETE") {
            state.refresh_statistics();
            state.refresh_spatial();
        }

        return match outcome {
            // A graph the server named has to say the name, or the client has no way to
            // address what it just asked to have created.
            Ok(status) => match &minted {
                Some(name) => respond_with(
                    request,
                    status,
                    "text/plain",
                    Vec::new(),
                    &[("Location", name.as_str())],
                ),
                None => respond(request, status, "text/plain", Vec::new()),
            },
            Err(e) => {
                let denied = e
                    .downcast_ref::<holos_engine::EngineError>()
                    .is_some_and(|e| matches!(e, holos_engine::EngineError::AccessDenied));
                let status = if denied { 403 } else { 400 };
                respond(request, status, "text/plain", e.to_string().into_bytes())
            }
        };
    }

    let guard = state
        .engine
        .read()
        .map_err(|_| anyhow::anyhow!("poisoned"))?;
    let session = match Session::open(guard.store(), principal, state.policy.clone()) {
        Ok(s) => s,
        Err(e) => {
            return respond(
                request,
                500,
                "text/plain",
                format!("session: {e}").into_bytes(),
            )
        }
    };
    match gsp::exists(&guard, &session, &target) {
        Ok(false) => return respond(request, 404, "text/plain", b"no such graph".to_vec()),
        Err(e) => return respond(request, 500, "text/plain", e.to_string().into_bytes()),
        Ok(true) => {}
    }

    let format = http::negotiate_rdf(accept.as_deref());
    match gsp::read(&guard, &session, &target, format) {
        // HEAD is GET without the body, so the content-type still has to be right.
        Ok(_) if method == "HEAD" => respond(request, 200, format.media_type(), Vec::new()),
        Ok(body) => respond(request, 200, format.media_type(), body),
        Err(e) => respond(request, 500, "text/plain", e.to_string().into_bytes()),
    }
}

/// The RDF format a request body declares.
/// Whether a protocol parameter was given more than once, as a message saying so.
///
/// The protocol allows a request to carry exactly one `query` or `update`. Two is a client
/// error rather than a choice for the server to make: answering the first would run
/// something the client did not unambiguously ask for.
///
/// Takes the *encoded* parameter string — a query string or a form body — because the
/// parser keeps the first value of a non-repeatable key and drops the rest.
fn repeated(encoded: &str, key: &str) -> Option<String> {
    http::given_more_than_once(encoded, key)
        .then(|| format!("the request carries more than one '{key}' parameter"))
}

/// Whether a POST body cannot be used, given its declared content type.
///
/// Two refusals the protocol requires:
///
/// * **No `Content-Type` at all.** A body that is form-encoded looks identical to one that
///   is a query; guessing means running text the client never said was a query.
/// * **A charset that is not UTF-8.** The protocol fixes the encoding of both media types
///   at UTF-8, so a request declaring UTF-16 is asking for something the protocol does not
///   offer, and decoding it as UTF-8 anyway would answer over mojibake.
fn unusable_body(content_type: Option<&str>) -> Option<String> {
    let Some(value) = content_type else {
        return Some(
            "a POST body needs a Content-Type: application/sparql-query, \
             application/sparql-update, or application/x-www-form-urlencoded"
                .to_owned(),
        );
    };
    let charset = value.split(';').skip(1).find_map(|p| {
        let (name, v) = p.split_once('=')?;
        name.trim()
            .eq_ignore_ascii_case("charset")
            .then(|| v.trim().trim_matches('"').to_ascii_lowercase())
    })?;
    (charset != "utf-8" && charset != "utf8")
        .then(|| format!("the protocol requires UTF-8; this request declares `{charset}`"))
}

/// The graphs `using-graph-uri` and `using-named-graph-uri` name.
///
/// Both are repeatable, and arrive newline-joined from the query-string parser.
fn named_dataset(
    params: &std::collections::HashMap<String, String>,
) -> Result<(Vec<NamedNode>, Vec<NamedNode>), holos_engine::EngineError> {
    let read = |key: &str| -> Result<Vec<NamedNode>, holos_engine::EngineError> {
        http::values(params, key)
            .into_iter()
            .map(|iri| {
                NamedNode::new(&iri).map_err(|e| {
                    holos_engine::EngineError::BadRequest(format!("{key} `{iri}`: {e}"))
                })
            })
            .collect()
    };
    Ok((read("using-graph-uri")?, read("using-named-graph-uri")?))
}

/// The base IRI a relative IRI in a request resolves against.
///
/// The protocol says a service *may* define one, and names the endpoint itself as the
/// obvious candidate. Without it `CONSTRUCT { <s> <p> 1 }` cannot parse at all, so the
/// choice is between having one and refusing a class of legal requests.
///
/// Built from the `Host` header so it matches the URI the client actually used, which is
/// what makes the resolved IRIs meaningful to that client.
fn base_iri(request: &Request, path: &str) -> String {
    let host = header(request, "host").unwrap_or_else(|| "localhost".to_owned());
    format!("http://{host}{path}")
}

fn rdf_format_of(content_type: Option<&str>) -> Option<RdfFormat> {
    let value = content_type?;
    let media = value.split(';').next()?.trim().to_ascii_lowercase();
    match media.as_str() {
        "text/turtle" | "application/x-turtle" => Some(RdfFormat::Turtle),
        "application/n-triples" | "text/plain" => Some(RdfFormat::NTriples),
        "application/trig" => Some(RdfFormat::TriG),
        "application/n-quads" => Some(RdfFormat::NQuads),
        "application/rdf+xml" => Some(RdfFormat::RdfXml),
        "text/n3" => Some(RdfFormat::N3),
        "application/ld+json" => Some(RdfFormat::JsonLd {
            profile: oxrdfio::JsonLdProfileSet::empty(),
        }),
        _ => None,
    }
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
    if let Some(index) = state.spatial() {
        options = options.with_spatial(index);
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
    // Read before the request is consumed by a response, and before the lock is taken.
    let base = base_iri(&request, "/update");
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
    // `using-graph-uri` names the dataset the update's WHERE runs against. It is applied
    // to the parsed form rather than the text, so an update that names its own dataset can
    // be told apart from one that does not — the protocol makes carrying both an error.
    let outcome = named_dataset(params).and_then(|(default_graphs, named_graphs)| {
        let mut parsed = sparql_update::parse(update, Some(&base))?;
        sparql_update::with_protocol_dataset(&mut parsed, default_graphs, named_graphs)?;
        sparql_update::apply(&mut guard, &mut session, &parsed)
    });
    // Release the write lock before rebuilding: refresh_statistics takes a read lock, and
    // holding the write lock across it would deadlock.
    drop(guard);
    if outcome.is_ok() {
        state.refresh_statistics();
        state.refresh_spatial();
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
                // Both mean the client sent something unanswerable, which is a 400
                // whether the SPARQL failed to parse or the request contradicted itself.
                holos_engine::EngineError::Syntax(_) | holos_engine::EngineError::BadRequest(_) => {
                    400
                }
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
                id.replace(
                    |c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_',
                    "_"
                )
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
        if let Some(level) =
            header(request, "x-holos-clearance").and_then(|c| c.trim().parse().ok())
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
    respond_with(request, status, content_type, body, &[])
}

/// `respond`, plus headers this particular answer needs — `Location`, so far.
fn respond_with(
    request: Request,
    status: u16,
    content_type: &str,
    body: Vec<u8>,
    extra: &[(&str, &str)],
) -> Result<()> {
    let mut response = Response::from_data(body).with_status_code(status);
    for (name, value) in [
        ("Content-Type", content_type),
        // The console is served from this same origin, but a SPARQL endpoint is routinely
        // queried from a page somewhere else, and refusing that by default makes the
        // endpoint useless for its most common job.
        ("Access-Control-Allow-Origin", "*"),
        ("Access-Control-Allow-Headers", "content-type, accept"),
        // The Graph Store Protocol uses all five verbs, so a browser client that can only
        // GET and POST cannot reach half of it.
        (
            "Access-Control-Allow-Methods",
            "GET, HEAD, POST, PUT, DELETE, OPTIONS",
        ),
        // Without this a browser cannot read the name of a graph it just created.
        ("Access-Control-Expose-Headers", "location"),
    ]
    .into_iter()
    .chain(extra.iter().copied())
    {
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
            Ok(Engine::with_store(holos_store::Store::with_storage(
                storage,
            )))
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
            "--backup-dir" => c.backup_dir = Some(value(&mut i)?),
            "--backup-role" => c.backup_role = Some(value(&mut i)?),
            "--purge-role" => c.purge_role = Some(value(&mut i)?),
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
            "--gsp-base" => c.gsp_base = Some(value(&mut i)?),
            "--gsp-path" => c.gsp_path = value(&mut i)?,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_post_body_with_no_content_type_is_refused() {
        // A form-encoded body and a query body are indistinguishable without one, and
        // guessing means running text the client never said was a query.
        assert!(unusable_body(None).is_some());
    }

    #[test]
    fn a_charset_other_than_utf8_is_refused() {
        // The protocol fixes both media types at UTF-8. Decoding UTF-16 as UTF-8 would
        // answer over mojibake rather than say no.
        assert!(unusable_body(Some("application/sparql-query; charset=UTF-16")).is_some());
        assert!(unusable_body(Some("application/sparql-query; charset=utf-8")).is_none());
        assert!(unusable_body(Some("application/sparql-query; charset=\"UTF-8\"")).is_none());
        // No charset at all is the default, which is UTF-8.
        assert!(unusable_body(Some("application/sparql-query")).is_none());
    }

    #[test]
    fn a_duplicated_query_parameter_is_reported() {
        assert!(repeated("query=a&query=b", "query").is_some());
        assert!(repeated("query=a", "query").is_none());
    }

    #[test]
    fn the_dataset_parameters_become_graph_names() {
        let params = http::parse_form(
            "using-graph-uri=http%3A%2F%2Fa&using-graph-uri=http%3A%2F%2Fb\
             &using-named-graph-uri=http%3A%2F%2Fc",
        );
        let (default_graphs, named_graphs) = named_dataset(&params).expect("valid IRIs");
        assert_eq!(default_graphs.len(), 2);
        assert_eq!(named_graphs.len(), 1);
    }

    #[test]
    fn a_dataset_parameter_that_is_not_an_iri_is_a_bad_request() {
        // Not a 500: the client sent it, and the message should say which one.
        let params = http::parse_form("using-graph-uri=not%20an%20iri");
        let error = named_dataset(&params).expect_err("should refuse");
        assert!(
            matches!(error, holos_engine::EngineError::BadRequest(_)),
            "got {error:?}"
        );
    }
}
