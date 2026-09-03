# Running HOLOS as a service

Everything here is in [deploy/](deploy/). Scripts come in matched pairs — `.sh` for
Linux/macOS, `.ps1` for Windows — reading the same [deploy/holos.env](deploy/holos.env).

For *using* the store — queries, geospatial, SHACL, holons — see the manual. This document
is about keeping it running.

---

## Quick start

```sh
deploy/setup.sh                     # check prerequisites, build, test
deploy/load.sh examples/hr.trig     # load data into the persistent store
deploy/run.sh                       # serve it
deploy/smoke.sh                     # verify, from another terminal
```

```powershell
deploy\setup.ps1
deploy\load.ps1 examples\hr.trig
deploy\run.ps1
deploy\smoke.ps1
```

Then open `http://127.0.0.1:7878/` for the YASGUI console.

The console has a **Map** tab alongside Table. It appears when a result carries a
`geo:wktLiteral` or `geo:geoJSONLiteral` binding, draws the geometries on an OpenStreetMap
base layer, and puts the rest of each row in the shape's popup — so a map is a result view
rather than a picture. It reads CRS84 only, which is GeoSPARQL's default; a geometry in any
other CRS is counted and reported under the map rather than drawn somewhere wrong.

YASR's own map and chart plugins are not open source. This one was written for HOLOS and
lives in `crates/holos-server/src/ui.rs`.

### Prerequisites

| | Why |
|---|---|
| Rust 1.87+ | The workspace's `rust-version` |
| clang / libclang | RocksDB generates its bindings with it. **The most common cause of a failed first build**, and the error it produces is unhelpful, so `setup.sh` checks for it up front |
| A C++ toolchain | RocksDB compiles C++ (`build-essential`, Xcode CLT, or MSVC "Desktop development with C++") |

Without persistence you need none of the above beyond Rust:
`cargo build --release --no-default-features`.

---

## The three ways to run it

### 1. Directly, for development

`deploy/run.sh` reads `deploy/holos.env` and translates it into flags. The server itself
takes **flags only** — no environment variables — so that `holos-server --help` remains the
complete surface. The script is the configuration layer, not a second one.

### 2. As a managed service

**Linux** — [deploy/holos.service](deploy/holos.service) is a hardened systemd unit:
`ProtectSystem=strict`, `NoNewPrivileges`, a `SystemCallFilter`, and exactly one
`ReadWritePaths`. It also sets `LimitNOFILE=65536`, because RocksDB opens a file per SST and
the default 1024 is far too low for a large dataset.

```sh
sudo useradd --system --home /var/lib/holos --shell /usr/sbin/nologin holos
sudo mkdir -p /opt/holos && sudo cp -r target/release/holos* deploy OPERATIONS.md /opt/holos/
sudo chown -R holos:holos /var/lib/holos
sudo cp deploy/holos.service /etc/systemd/system/ && sudo systemctl enable --now holos
```

**Windows** — `deploy\install-service.ps1` registers the service with `sc.exe`. Note the
limitation it prints: a bare Windows service discards stderr, which is where `holos-server`
writes its diagnostics. For real log capture, run it under NSSM or a scheduled task with
redirection.

### 3. In a container

[deploy/Dockerfile](deploy/Dockerfile) is a two-stage build — the build stage needs clang
and a C++ toolchain, the runtime stage carries neither.
[deploy/docker-compose.yml](deploy/docker-compose.yml) is a complete deployment: the store
with **no published ports**, reachable only by the Caddy front door on a shared network.
That is what makes `--trust-forwarded-identity` safe to turn on there.

---

## Configuration

Every setting lives in [deploy/holos.env](deploy/holos.env), which documents each one
inline. The summary:

| Setting | Default | Notes |
|---|---|---|
| `HOLOS_LISTEN` | `127.0.0.1:7878` | Keep on loopback whenever there is a front door |
| `HOLOS_THREADS` | `8` | One request holds one thread for its whole life, so this is the ceiling on concurrent queries — not a pool over an async runtime |
| `HOLOS_STORE` | `./var/store` | RocksDB directory. Empty means in-memory, discarded on exit |
| `HOLOS_DATA` | *(empty)* | Files loaded at **every** start. Idempotent but re-parsed each time — prefer `load.sh` once |
| `HOLOS_UI` | `on` | `off` serves the endpoints without the console, and then the process needs no network access of any kind |
| `HOLOS_TRUST_FORWARDED` | `off` | See below. This is the security-critical one |
| `HOLOS_DENY_ALL` | `off` | Deny-by-default instead of permit-all |
| `HOLOS_FAIL_CLOSED` | `off` | Error on refusal instead of filtering silently |
| `HOLOS_ALLOW_GRAPHS` | | Space-separated IRIs |
| `HOLOS_DENY_PREDICATES` | | Space-separated IRIs |
| `HOLOS_LABEL_GRAPHS` | | Space-separated `IRI=LEVEL` pairs |
| `HOLOS_DEV_ROLES`, `HOLOS_DEV_CLEARANCE` | | Grant to *every* request. Development only; the scripts warn when set |

`holos.env.local` overrides `holos.env`, and the environment overrides both — so a service
manager or container can change one setting without editing anything on disk.

### Filter or fail?

Refusal has two semantics, and the choice matters more than it looks:

- **Filter** (default) — the query runs and returns the answer *the principal is entitled
  to*. Formally: the answer to Q equals the answer over the sub-dataset the principal may
  see. Nothing reveals that anything was withheld.
- **Fail** (`--fail-closed`) — the query errors instead.

Filtering is right when a partial answer is a correct answer for that principal. Failing is
right when a partial answer would be **misread as a complete one** — a compliance report or
a reconciliation total, where silently missing rows is worse than an error.

---

## Identity: the front door contract

*[ACCESS-CONTROL.md](ACCESS-CONTROL.md) is the full guide to the policy model; this
section covers only what a deployment has to get right.*

**`holos-server` authenticates nobody, deliberately.** Token verification, Kerberos, mTLS
and SAML belong at the edge; the server sits behind that edge and reads *already-verified*
claims from three headers:

```
X-Holos-Principal: alice
X-Holos-Roles: hr,finance
X-Holos-Clearance: 3
```

It refuses to read them at all unless started with `--trust-forwarded-identity`, and prints
which way it is running at start-up.

> **The rule:** the front door must **strip whatever the client sent** under those names
> before setting its own. Without that, `--trust-forwarded-identity` means any caller can
> add `X-Holos-Roles: admin` to a curl command and be believed.

[deploy/Caddyfile](deploy/Caddyfile) does this with three `request_header -X-Holos-*` lines
before any handler runs. [deploy/nginx.conf](deploy/nginx.conf) does it differently and the
difference is a trap: **nginx passes unknown client headers straight through**, so each of
the three must be set explicitly — setting one is what overrides what the client sent.
Omitting any single one of them opens the hole.

`run.sh` and `run.ps1` warn if `--trust-forwarded-identity` is combined with a non-loopback
bind address, which is the shape that mistake takes.

### What this looks like working

Against `examples/hr.trig` with `--deny-predicate http://example.com/salary` and
`--label-graph http://example.com/reviews=3`:

```sh
# names come back; the salary column is empty — the scan never saw those triples
$ curl -s -H 'Accept: text/csv' --data-urlencode \
    'query=SELECT ?name ?salary WHERE { GRAPH ?g { ?s ex:name ?name OPTIONAL { ?s ex:salary ?salary } } }' \
    localhost:7878/query
name,salary
Alice,
Bob,

# the classified graph, anonymous: nothing
$ curl -s ... 'query=SELECT ?note WHERE { GRAPH ex:reviews { ?s ex:reviewNote ?note } }'
s,note

# the same query with clearance 3 forwarded
$ curl -s -H 'X-Holos-Clearance: 3' ... 
s,note
http://example.com/alice,Ready for promotion.
```

Policy is enforced **at the scan**, not by rewriting the query, so this holds for every
query shape — including geospatial ones. Denying `geo:asWKT` makes a spatial join find
nothing; there is no geospatial exemption.

---

## Loading data

```sh
deploy/load.sh data/*.ttl        # stop the service first
```

`--bulk` buffers writes, skips the write-ahead log, and since 0.3.0 hands the index
orders to RocksDB as pre-sorted files rather than writing them key by key: **5.4–8.5× faster**
(see [BENCHMARKS.md](BENCHMARKS.md)). Measured 156k vs 18k quads/s at 7.5M quads, at the cost
of a part-way-interrupted load having to be discarded
rather than resumed. That is the right trade for a load you can simply re-run.

**Only one process may hold the store directory.** RocksDB takes an exclusive lock on
`LOCK`, so a second process gets:

```
IO error: Failed to create lock file: .../LOCK: The process cannot access the file
because it is being used by another process.
```

That is expected, not a bug. Stop the service before loading.

Formats are taken from the extension: `.ttl .nt .trig .nq .rdf .owl .jsonld .n3` — **each
also with `.gz`**, which is how large RDF is normally distributed:

```sh
deploy/load.sh dumps/wikidata.nq.gz dumps/dbpedia.nt.gz
```

Decompression is **streamed**, so a 60 GB dump costs no more memory than a small one, and
**multi-member** archives are read in full. That last point matters: large dumps are often
produced by concatenating compressed chunks, which is valid gzip, and a single-member
decoder stops at the end of the first chunk while reporting success — losing most of the
file silently.

> **Windows note:** run these under PowerShell. Git Bash rewrites arguments that look like
> Unix paths, which can silently mangle `--store` into something like
> `C:/Program Files/Git/...`.

---

### Blank nodes are scoped to the file they came from

Each load is a document, and RDF scopes a blank node label to the document it appears in. So
`_:a` in two files denotes two different things, and loading both gives you two nodes:

```sh
holos stats --store ./var/store --bulk --data a.ttl
holos stats --store ./var/store --bulk --data b.ttl   # b.ttl's `_:a` is not a.ttl's
```

This matters more than it sounds, because `_:a`, `_:b0` and `_:genid1` are what every
serialiser reaches for first — merging on the label would join unrelated records whenever two
exports happened to agree on a name.

The consequence to plan for: **loading the same file twice does not deduplicate** its
blank-node triples. The second load asserts a second existential, which is what RDF says it
does. Anything with an IRI subject deduplicates normally.

---

## Backup and restore

**The service keeps running.** RocksDB checkpoints flush the log and hard-link the SST files
into a new directory, so the snapshot is consistent, near-instant, and initially costs almost
no disk — taken while the store is open and being written to.

```sh
deploy/backup.sh /backups          # or: holos backup --store ./var/store --to /backups/tonight
```

Two consequences of hard links, both of which matter:

- **A checkpoint on the same filesystem shares the store's files.** That is what makes it
  cheap, and it means **it is not an off-machine backup** — losing the disk loses both. Copy
  or replicate the result elsewhere if that is what you need. To a different filesystem
  RocksDB copies instead: correct, no longer instant, genuinely independent.
- **A checkpoint pins the files it links**, so compaction cannot delete them. Disk use climbs
  as the snapshot and the live store diverge. `deploy/backup.sh` keeps the last
  `HOLOS_BACKUP_KEEP` (default 7) and removes the rest — retention is part of the job, not an
  afterthought.

Restore by pointing `--store` at a checkpoint, or by copying it back with the service stopped.

Two refusals worth knowing, both preventing a backup that looks fine and is not:

- **During a bulk load.** Those writes are buffered in the process rather than in RocksDB, so
  a checkpoint taken then would be internally consistent and missing data.
- **An in-memory store.** It has no files to snapshot, and says so rather than writing nothing
  and reporting success.

### Over HTTP

`POST /backup` does the same thing, and is **off unless both flags are set**:

```sh
holos-server --store ./var/store              --backup-dir /backups --backup-role ops --trust-forwarded-identity

curl -X POST -H 'X-Holos-Roles: ops' http://127.0.0.1:7878/backup
# {"path":"/backups/holos-1787763950","quads":1048576}
```

| Condition | Answer |
|---|---|
| Either flag unset | **404** — the endpoint is absent, not merely defended |
| Identity not trusted | **403** — every request is anonymous, so nobody holds the role |
| Role missing or wrong | **403** |
| Role held | **201** with the path and quad count |
| Bulk load in progress, or in-memory store | **409** with the reason |

**The caller never names the destination.** `POST /backup?to=/anywhere` would be an
arbitrary-write primitive — whatever the server process can write, a caller could ask it to
fill with a copy of the database. The server owns `--backup-dir` and mints a timestamped
child inside it.

A backup copies the whole store, ignoring the policy that governs every query. It is the one
operation where the ordinary access controls do not apply, which is why it has its own role —
and why `--role` on the server, which grants a role to *everyone*, must not be used to
provide it.


`holos dump` is **not** a backup — it emits what the *principal* is allowed to see, through
the same policy-filtered view as any query. That is usually a smaller thing, and that is the
point of it. It defaults to N-Quads, the only line-based format that carries the graph name:

```sh
holos dump --store ./var/store                       # N-Quads
holos dump --store ./var/store --format trig         # or nt, ttl, rdf, jsonld
```

Dumping a quad store as N-Triples silently flattens named graphs into one, so the format
choice is a correctness question rather than a preference.

---

## Writing over HTTP

`POST /update` takes SPARQL 1.1 Update, form-encoded or as `application/sparql-update`:

```sh
curl -X POST -H 'Content-Type: application/sparql-update'   --data 'INSERT DATA { <http://example.com/a> <http://example.com/p> "x" }'   http://127.0.0.1:7878/update
# {"inserted":1,"deleted":0,"graphsCreated":0,"graphsDropped":0}
```

Three things worth knowing:

- **It takes the write lock.** `/update` is the only endpoint that does, so a writer
  excludes readers for the update's duration. That is what makes the update's failure
  atomicity behave as isolation in this deployment.
- **It is all-or-nothing, including across a crash.** The whole request runs inside one
  commit scope: the writes accumulate into a single RocksDB batch and are written once. If
  any operation fails, the store is left exactly as it was — including the operations that
  had already succeeded — and if the process dies mid-request, so is the store on disk,
  because nothing had been written yet. The scope refuses past a few hundred thousand quads
  in one request; load data that size with `--data`, which is built for the volume.
- **Policy applies to the write path.** Every quad written is checked for `WRITE`, and the
  `WHERE` clause is filtered by read policy on the same path as a `SELECT` — so a principal
  cannot delete what it cannot see. `SILENT` suppresses an operation's own error but never
  a policy refusal, which would otherwise turn it into a way to probe what one may not touch.

`--read-only` makes `/update` answer 403 without affecting the store, which is the right
setting for a public endpoint sharing a store with a separate loader.

`LOAD <http://…>` is **refused**. Fetching an arbitrary URL named inside a request is a
server-side request forgery primitive — it would make the server issue requests to hosts
only the server can reach. `file:` URLs load; remote ones return an error saying so.

### Naming the dataset from the request

`using-graph-uri` and `using-named-graph-uri` say which graphs an update's `WHERE` matches
against, without putting `USING` in the text:

```sh
curl -X POST -H 'Content-Type: application/sparql-update' \
     --data 'DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }' \
     'http://127.0.0.1:7878/update?using-graph-uri=http%3A%2F%2Fexample.com%2Fg1'
```

Both are repeatable. An update that **already** names its own dataset with `USING` or `WITH`
and *also* carries these parameters is a **400**: the protocol makes carrying both a client
error rather than a precedence question, and resolving it either way would run the update
over a dataset its author did not choose.

The query endpoint's equivalents are `default-graph-uri` and `named-graph-uri`, also
repeatable.

### What the endpoints refuse

The protocol requires these, and each was a real gap until the W3C protocol suite was run
against the server:

| Request | Answer |
|---|---|
| Two `query=` or two `update=` parameters | **400** — ambiguous, not a list |
| A POST body with no `Content-Type` | **400** — a form body and a query body are indistinguishable without one |
| `charset=UTF-16` on either media type | **400** — the protocol fixes both at UTF-8 |

Relative IRIs in a query or update resolve against a **service base URI** built from the
`Host` header and the endpoint path, so `CONSTRUCT { <s> <p> 1 } WHERE {}` parses and its
IRIs come back absolute and addressable by the client that sent them.

## Graph Store Protocol

REST verbs on whole graphs, for the jobs SPARQL Update makes awkward. **On by default at
`/graph`**, using indirect identification — the graph named by a query parameter. Two flags
change that:

```sh
holos-server --store ./db --gsp-path /gsp --gsp-base https://data.example.org
```

| Verb | Effect | Exists | Absent |
|---|---|---|---|
| `GET` / `HEAD` | Fetch the graph as a document | 200 | **404** |
| `PUT` | Replace it wholesale | 204 | **201** |
| `POST` | Merge into it | 204 | **201** |
| `DELETE` | Remove it, not just its contents | 204 | **404** |

```sh
# indirect identification: the graph is a parameter
curl -X PUT -H 'Content-Type: text/turtle' --data-binary @people.ttl \
     'http://127.0.0.1:7878/graph?graph=http%3A%2F%2Fexample.org%2Fpeople'

curl -H 'Accept: text/turtle' 'http://127.0.0.1:7878/graph?graph=http%3A%2F%2Fexample.org%2Fpeople'

# the default graph
curl -H 'Accept: application/n-triples' 'http://127.0.0.1:7878/graph?default'

# let the server name a new graph; the name comes back in Location
curl -i -X POST -H 'Content-Type: text/turtle' --data-binary @batch.ttl \
     http://127.0.0.1:7878/graph
```

Four things worth knowing:

- **`--gsp-base` is what enables *direct* identification**, where the request URI *is* the
  graph name (`PUT /graph/people`). It is off without one because a server behind a reverse
  proxy sees `/graph/people` while the world sees `https://data.example.org/graph/people`, and
  guessing would mint graph names that do not match the ones clients later ask for. Set it
  to the base the *outside* sees. It is also required for `POST` to the endpoint itself,
  since the server has to name the graph it creates.
- **`DELETE` removes the graph, not just its quads.** Otherwise a second `DELETE` could not
  answer 404, and a client could not tell "I removed it" from "it was not there".
- **A graph the principal may not read is 404 to them**, not 200-and-empty. Reporting it as
  present-but-empty would confirm it exists, which is the thing the policy was withholding.
  The same policy governs writes: a REST verb is not a way around it.
- **File uploads work.** A `multipart/form-data` body carries one RDF document per part,
  each with its own prefixes, and **all** of them are merged. One part this store cannot
  parse makes the whole upload a 415 rather than a partial success reported as a 204.

`PUT` is the operation people otherwise write as `DROP GRAPH … ; INSERT DATA { GRAPH … }`,
which is two operations that can half-succeed where this is one that cannot.

## Maintenance

Three jobs that are not automatic, and are not automatic on purpose. Each is cheap to run and
none of them is urgent; a server that decides for itself to do whole-store work is a server
that does something surprising at three in the morning.

### Reclaiming space: `holos compact`

`backup` preserves the store *exactly*, dictionary included. That is the right behaviour for a
backup and the wrong one for reclaiming space, because the dictionary is append-only: deleting
quads leaves their terms behind, and nothing in the normal course of running removes them.

```sh
holos compact --store ./var/store --to ./var/store.new
```

It writes a **new** store holding only the live data, and refuses if the destination exists —
a failure then leaves the original untouched. Two things to plan for:

- It needs room for both stores at once.
- It does not copy writes that arrive while it runs. Stop writers, or accept that the window
  is lost, then swap the directories.

It reads the store directly rather than through a policy, so it copies everything regardless
of who runs it. Treat the output as you would the original.

### Entailment: `holos entail`

Materialises the RDFS closure, so every reader sees the entailed triples without knowing about
entailment — the query path, the topology rewrite, SHACL, the statistics.

```sh
holos entail --store ./var/store                    # into <https://holos.dev/ns#entailed>
holos entail --store ./var/store --entail-graph <IRI> --entail-budget 1000000
```

It writes into a **graph of its own**, `<https://holos.dev/ns#entailed>` unless you name
another, which is what lets you undo it:

```sparql
DROP GRAPH <https://holos.dev/ns#entailed>
``` The cost is that queries only see the entailed triples
under the union default graph, or by naming the graph. That trade is the reason for the
default, not an accident of it.

The budget is a bound on a mistake rather than a tuning knob. A schema entailing ten million
triples is worth reading before it is materialised, so the closure is abandoned and **nothing
is written** rather than the machine running out of memory.

Two entailments are true and cannot be written down, so they are not:
`ex:age rdfs:range xsd:integer` with `ex:alice ex:age 30` entails that 30 is an integer, and a
triple term denotes a proposition. RDF has no subject position for a literal or a triple term.

### Spatial index upkeep: `POST /maintenance/purge`

The spatial index tracks the **dictionary**, which never forgets. That is what lets it catch
up with a write in a fraction of a millisecond instead of rebuilding — and it means deleted
geometries leave entries behind. Nothing is wrong while they are there: the index is a
superset filter, so a geometry with no quads fails to join and contributes no row. It is
memory, not correctness. **Restarting does not clear them**, because the index is rebuilt from
the dictionary and comes back holding every geometry ever interned.

Off unless `--purge-role` is set, and then the caller must hold that role:

```sh
holos-server --store ./var/store --purge-role ops --trust-forwarded-identity
curl -X POST -H 'X-Auth-Request-Groups: ops' http://127.0.0.1:7878/maintenance/purge
```

Without `--trust-forwarded-identity` every request is anonymous and holds no roles, so the
endpoint is unreachable. A switched-off endpoint answers **404** rather than 403, so probing
cannot map the configuration.

There is no timer inside the server. Point cron, a systemd timer or a Kubernetes CronJob at
it, the same way `deploy/backup.sh` calls `/backup`.

---

## Disk headroom

The store grows and nothing shrinks it on its own. Two maintenance operations need room
beyond what the store already occupies, and both now refuse rather than filling the disk and
failing part-way.

**`holos stats --store DIR` is the thing to look at.** It reports what the store occupies,
what is free on that filesystem, and whether a compaction would fit:

```
quads            1000000
dictionary terms 489479
named graphs     0
on disk          48.2 MiB
free here        112.0 GiB
room to compact  yes (needs up to 48.2 MiB)
```

### What each operation costs

| Operation | Needs | Why |
|---|---|---|
| **`holos compact`** | The store's size again, immediately | It writes a **second** store beside the first and leaves the original untouched, which is what makes it safe to abandon |
| **`holos backup`** / `POST /backup` onto the same filesystem | Nothing at first, the store's size eventually | It hard-links, so the copy is free — but the links **pin files compaction can no longer delete**, so the space is owed as soon as anything churns |
| **`holos backup`** onto another filesystem | The store's size, immediately | Hard links do not cross a mount, so RocksDB copies |
| **A bulk load** | Roughly the index size again, briefly | Sorted ingestion writes its files under the store directory and then moves them in |

The estimate is the source store's size on disk — the whole directory, not just the SST
files, because the write-ahead log and the manifest have to be copied too. For `compact` it
is deliberately generous: the point of compacting is that the result is smaller, often much
smaller, so a store with a lot of deleted data in it will need far less than the check
demands. `--force` is there for exactly that case.

`POST /backup` answers **507 Insufficient Storage** rather than 500 when it will not fit —
the request was fine and the server is healthy, so a scheduler can alert on it differently
from a fault. That endpoint is the one worth watching: it is called nightly by something that
is not looking, and its hard links are what stop the space coming back.

### If it is already full

A write that runs out of space fails and, since 0.3.0, fails **atomically** — an update or a
holon tick either lands whole or not at all, so a full disk costs you the write rather than
the store's consistency. That is the recovery position, not a substitute for headroom.

Order of operations from there: drop what you do not need with `DROP GRAPH` or `DELETE`,
then `holos compact --to` somewhere with room, because deletion alone reclaims nothing — the
dictionary is append-only and compaction is the only thing that shrinks it.

---

## Monitoring

| Endpoint | Use |
|---|---|
| `GET /health` | Liveness. Cheap, no store access |
| `GET /stats` | `{"quads":…, "dictionaryTerms":…, "namedGraphs":…}` |

`deploy/smoke.sh` exits non-zero on the first failure, so it works as a deployment gate or a
container health check. It checks thirteen things and changes nothing: content negotiation on
each endpoint, that a syntax error is a 400 rather than a 500, that the protocol's refusals
are refusals, and that the graph store answers if it was configured.

Memory goes to RocksDB's block cache plus the term dictionary. Size from `/stats` before
setting a container limit — a store whose dictionary does not fit will thrash rather than
fail. For scale: 7.5 million quads produced 2,002,681 dictionary terms and 253 MB on disk —
about 35 bytes per quad across three index copies of each — because the `TermId` encoding
inlines every integer, float and short string into the id itself so they never reach storage
at all. `holos stats --store DIR` reports both for a store you have.

---

## What is missing, and how to work around it

Stated plainly, because finding these out in production is worse.

| Gap | Consequence | Workaround |
|---|---|---|
| **No TLS in the server** | Plain HTTP only | Terminate at the front door. Both configs do |
| **Timeouts are not absolute** | `--timeout` stops a query that is reading or streaming rows; one blocked inside a single in-memory step is not interruptible | Bound the result size in the query |
| **A checkpoint is not off-machine** | Hard links share the live store's files, so one disk failure loses both | Copy or replicate the checkpoint elsewhere; or checkpoint to a different filesystem, which copies |
| **Single process per store** | No read replicas over one directory | Run replicas over separate copies |
| **CORS is `*`** | Any origin may query | Intentional — a SPARQL endpoint is routinely queried from a page elsewhere, and refusing that makes it useless for its most common job. Restrict at the proxy if you need to |
| **Direct graph identification is off by default** | `PUT /graph/people` answers 400; the parameter form works | Set `--gsp-base` to the base URI the *outside* sees |
| **No cost-based planner** | Query order matters: a measured **3×** on a five-pattern query | Reordering is applied automatically when statistics are built; write the most selective pattern first if they are not. See `DESIGN.md` §16 |
| **`--data` reloads every start** | Slow restarts | Load once with `load.sh`, leave `HOLOS_DATA` empty |
| **Shapes beyond SHACL Core are refused by the default validator** | A `sh:sparql` constraint, a SHACL-AF rule or a node expression makes `holos validate` and the holon Boundary refuse the shapes graph, naming the construct | Deliberate: that validator is what gates a commit, and it used to drop what it could not check and answer *conforms* — a gate that fails open. Run `holos validate --engine adapted`, which implements them; the Boundary keeps the native one because only it revalidates a delta |
| **Compaction is offline** | Writes arriving during `holos compact` are not copied | Stop writers for the window, then swap directories |

---

## Troubleshooting

**The build fails somewhere inside `librocksdb-sys`.** Missing clang or a C++ toolchain.
`deploy/setup.sh` checks both before starting; run it.

**`Failed to create lock file`.** Another process holds the store. Usually the service,
during a load.

**Every request is anonymous although the proxy sets the headers.** The server was started
without `--trust-forwarded-identity`. It says so at start-up:

```
identity forwarded headers are NOT trusted; every request is anonymous
```

**A query returns fewer rows than expected.** That is what filtering semantics look like.
Run the same query with `holos query --audit` to see what policy did, or start with
`--fail-closed` so refusals become errors instead.

**Restarts are slow.** `HOLOS_DATA` is re-parsing files on every start.
