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

`--bulk` buffers writes and skips the write-ahead log: **3.3–3.6× faster** (see
[BENCHMARKS.md](BENCHMARKS.md)), and the advantage grows with the dataset because the
write-ahead log it skips grows with it too. Measured 36k vs 10k quads/s at 7.5M quads
on a million triples), at the cost of a part-way-interrupted load having to be discarded
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

## Backup and restore

[deploy/backup.sh](deploy/backup.sh) — and it **requires the service to be stopped**, which
is a real limitation rather than caution:

> RocksDB checkpoints are named in `DESIGN.md` §6.1 but are **not built yet**, so there is
> no way to take a consistent snapshot of an open store. When checkpoints land this becomes
> a hard-linked online copy and the stop goes away.

The script refuses to run if it can see the lock held. Restore by stopping the service and
copying the directory back.

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
- **It is all-or-nothing.** If any operation in the request fails, the store is left
  exactly as it was — including the operations that had already succeeded.
- **Policy applies to the write path.** Every quad written is checked for `WRITE`, and the
  `WHERE` clause is filtered by read policy on the same path as a `SELECT` — so a principal
  cannot delete what it cannot see. `SILENT` suppresses an operation's own error but never
  a policy refusal, which would otherwise turn it into a way to probe what one may not touch.

`--read-only` makes `/update` answer 403 without affecting the store, which is the right
setting for a public endpoint sharing a store with a separate loader.

`LOAD <http://…>` is **refused**. Fetching an arbitrary URL named inside a request is a
server-side request forgery primitive — it would make the server issue requests to hosts
only the server can reach. `file:` URLs load; remote ones return an error saying so.

## Monitoring

| Endpoint | Use |
|---|---|
| `GET /health` | Liveness. Cheap, no store access |
| `GET /stats` | `{"quads":…, "dictionaryTerms":…, "namedGraphs":…}` |

`deploy/smoke.sh` exits non-zero on the first failure, so it works as a deployment gate or a
container health check. It checks eight things including that a syntax error is a 400 rather
than a 500, and that `/update` still answers 501.

Memory goes to RocksDB's block cache plus the term dictionary. Size from `/stats` before
setting a container limit — a store whose dictionary does not fit will thrash rather than
fail. For scale: a million triples produced 489,479 dictionary terms and 48 MB on disk,
because the `TermId` encoding inlines every integer, float and short string into the id
itself so they never reach storage at all.

---

## What is missing, and how to work around it

Stated plainly, because finding these out in production is worse.

| Gap | Consequence | Workaround |
|---|---|---|
| **No Graph Store Protocol** | No REST graph verbs (`PUT /graph?graph=…`) | Use SPARQL Update, which is implemented |
| **No TLS in the server** | Plain HTTP only | Terminate at the front door. Both configs do |
| **Timeouts are not absolute** | `--timeout` stops a query that is reading or streaming rows; one blocked inside a single in-memory step is not interruptible | Bound the result size in the query |
| **No online backup** | Backups need a stop | Above |
| **Single process per store** | No read replicas over one directory | Run replicas over separate copies |
| **CORS is `*`** | Any origin may query | Intentional — a SPARQL endpoint is routinely queried from a page elsewhere, and refusing that makes it useless for its most common job. Restrict at the proxy if you need to |
| **No cost-based planner** | Query order matters: a measured **3×** on a five-pattern query | Write the most selective pattern first. See `DESIGN.md` §16 |
| **`--data` reloads every start** | Slow restarts | Load once with `load.sh`, leave `HOLOS_DATA` empty |

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
