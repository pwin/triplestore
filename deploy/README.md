# deploy/ — the scripts, and the patterns they are for

The server takes flags and nothing else; `holos-server --help` is the complete surface.
Everything in this directory is a translation layer over that: one configuration file,
read by every script, turned into flags at start. This page is the set of configurations
people actually run, each as a `holos.env.local` you can copy and the commands that go with
it. [OPERATIONS.md](../OPERATIONS.md) has the reasoning behind each setting; this has the
settings.

## How the pieces fit

| Script | Does | Reads |
|---|---|---|
| `setup.sh` / `setup.ps1` | Checks prerequisites, builds, runs the tests | — |
| `load.sh` / `load.ps1` | Loads files into the persistent store with `holos stats --bulk` | `HOLOS_STORE` |
| `run.sh` / `run.ps1` | Starts `holos-server` with the configuration as flags | everything |
| `smoke.sh` / `smoke.ps1` | Probes a running instance; non-zero exit on the first failure | — |
| `backup.sh` | Checkpoints the store while it is being served; keeps the last `HOLOS_BACKUP_KEEP` | `HOLOS_STORE` |
| `install-service.ps1` | Registers `holos-server` as a Windows service, flags baked in | everything, at install time |
| `holos.service` | The systemd unit; it runs `run.sh`, so the configuration applies | — |
| `Dockerfile`, `docker-compose.yml`, `Caddyfile`, `nginx.conf` | The container and the two front doors | — |

Configuration is layered, and the order is the same for both shells:

1. `holos.env` — every key, documented, with the defaults. Under version control. Do not
   edit it for a deployment.
2. `holos.env.local` — your values. Git ignores it. This is the file the patterns below
   are written for.
3. The environment — wins over both files, so a service manager, a container or a one-off
   `HOLOS_STORE=/mnt/big deploy/load.sh dump.nq` can override one setting without
   touching disk. An *empty* environment variable does not clear a file setting; to turn
   something off, set it to `off` or to the value you want.

`load.sh` and `run.sh` read the same `HOLOS_STORE`. That is the point of the file: the
load and the server name the same directory because neither of them names it.

**Windows:** run the `.ps1` scripts from PowerShell 7. Git Bash rewrites arguments that
look like Unix paths, and can turn `--store E:/store` into something under
`C:/Program Files/Git`.

---

## 1. First run

Build, load a file, serve it, check it.

```sh
deploy/setup.sh
deploy/load.sh examples/hr.trig
deploy/run.sh
deploy/smoke.sh          # from another terminal
```

```powershell
deploy\setup.ps1
deploy\load.ps1 examples\hr.trig
deploy\run.ps1
deploy\smoke.ps1
```

The store is `./var/store`, the console is at `http://127.0.0.1:7878/`. The server's
first line of output names the store it opened and how many quads are in it:

```
  store    ./var/store — 21 quads
```

**Read that line every time.** A server that opened the wrong directory says `— empty`
right there, and nothing else will tell you.

## 2. The store on another disk

The most common way a deployment goes wrong: the load is run with `--store E:/holos` by
hand, the server is started with `run.sh`, and it serves `./var/store`, which is empty.
Put the path in the file and let both scripts read it.

```sh
# deploy/holos.env.local
HOLOS_STORE=E:/holos/store
```

```sh
deploy/load.sh dumps/*.ttl.gz
deploy/run.sh
```

`holos stats --store E:/holos/store` (no `--data`) reports what is in a store without
serving it, and is the check to run when a query returns nothing.

## 3. A big load

Hundreds of millions of triples. Nothing changes in the commands; what changes is what to
expect and what to have ready. Measured on a 30.4 GB Turtle file of 653.8 million triples,
loaded to an mSATA SSD behind a USB bridge:

| | |
|---|---|
| Time | 55 minutes, about 200,000 quads/s |
| Memory | 3.4 GB peak, flat through the load; it does not grow with the file |
| Store | 21 GB, about 33 bytes a quad |
| Scratch | up to 44 GB of sorted runs under `<store>/holos-ingest/`, plus the index files being written, until the merge at the end consumes them; removed when the load finishes |
| CPU | about two cores: one interning, one parsing, and the threads that sort and write |

So: **the scratch and the store occupy the disk at once** — and the source too, if it is
on the same disk — and the scratch is about twice the store's final size. Give the store's
filesystem three times the size of the store you expect. The load does not check first.

Load into a **fresh directory**, and if there are several files, load them in **one
invocation** — `load.sh` takes several. An empty dictionary lets the load keep an in-memory
filter over every term it interns, so a new term costs no disk read; a second load into a
populated store looks every term up, and is several times slower.

While it runs, stderr carries one line per dictionary flush, about every six million terms,
with the read cost of that window; at the end, `loaded N quads in Ts` and a
`dictionary reads:` line. A load interrupted part-way cannot be resumed: delete the
directory and run it again. Do not run anything else heavy on the same disk while it
loads — a build and a benchmark sharing the disk for twenty minutes cost one run 7%.

## 4. Replace the data without a gap

The server holds the store directory exclusively, so a load into the served directory means
stopping the server for the whole load. Load into a new directory instead, beside the old,
while the old one serves; then switch and restart.

```sh
HOLOS_STORE=E:/holos/store-2026-09-20 deploy/load.sh dumps/*.ttl.gz   # the old store keeps serving
# edit deploy/holos.env.local: HOLOS_STORE=E:/holos/store-2026-09-20
systemctl restart holos                                                 # or stop run.sh and start it again
```

Downtime is the restart. The old directory is the rollback: switch the path back and
restart. Delete it when you are sure.

## 5. A demo, or a throwaway instance

No persistent store; the files are parsed at every start and discarded at exit. Right for
a small dataset and wrong for a large one, because the load happens on every restart.

```sh
# deploy/holos.env.local
HOLOS_STORE=
HOLOS_DATA="examples/hr.trig examples/geosparql-example.rdf"
HOLOS_DEV_ROLES=admin
```

`HOLOS_DEV_ROLES` gives every request that role, which is what makes a demo usable and
what makes this configuration wrong for anything reachable by other people.

## 6. A read-only endpoint behind a front door

Data arrives by `load.sh` and by nothing else; the public sees a query endpoint and a
console. The server binds to loopback; Caddy or nginx listens on the network, terminates
TLS, and sets the identity headers. `HOLOS_TRUST_FORWARDED` is what lets the server believe
those headers, and it is safe *only* because the bind address is loopback: on an open port
it would let any caller name its own roles.

```sh
# deploy/holos.env.local
HOLOS_STORE=/var/lib/holos/store
HOLOS_LISTEN=127.0.0.1:7878
HOLOS_TRUST_FORWARDED=on
HOLOS_READ_ONLY=on
HOLOS_REORDER=on
HOLOS_TIMEOUT=60
HOLOS_MAX_QUERY_MEMORY=4
HOLOS_EXTRA_ARGS="--max-blocking-rows 5000000"
```

`HOLOS_READ_ONLY` answers 403 to `/update` and to every writing Graph Store verb;
`load.sh` still works, because it opens the store itself. `HOLOS_REORDER` makes query cost
independent of how the query was written and is what `--max-blocking-rows` needs to act:
the first start on a large store builds statistics, which is a scan and takes minutes at
hundreds of millions of triples; they are kept with the store, so later starts are
immediate. `HOLOS_TIMEOUT` and `HOLOS_MAX_QUERY_MEMORY` are what stop one query from taking
the instance down; `holos.env` already sets the timeout to 300 s, and a public door wants
less. A query that runs past it is answered with a problem document naming the limit. Set
the memory ceiling to a third of what you can spare, at most
([OPERATIONS.md](../OPERATIONS.md#configuration) says why). `Caddyfile` and `nginx.conf` are
the two front doors, with the header handling already in them.

The console can only talk to this server — its security policy names this origin, the
script CDN and the basemap's tile host, and nothing else. The tiles are the one disclosure
left: which ones a map fetches says where the user is looking. `HOLOS_UI_TILES=none` draws
geometries over a blank background instead. For a machine-to-machine endpoint add
`HOLOS_UI=off`; the process then needs no outbound network access at all.

## 7. Locked down

Nothing readable until granted; graphs classified; refusal is an error rather than a
silently smaller answer.

```sh
# deploy/holos.env.local
HOLOS_STORE=/var/lib/holos/store
HOLOS_LISTEN=127.0.0.1:7878
HOLOS_TRUST_FORWARDED=on
HOLOS_DENY_ALL=on
HOLOS_ALLOW_GRAPHS="http://example.com/public http://example.com/catalogue"
HOLOS_DENY_PREDICATES="http://example.com/salary"
HOLOS_LABEL_GRAPHS="http://example.com/reviews=3 http://example.com/legal=5"
HOLOS_FAIL_CLOSED=on
```

Filtering is the default because it gives a principal the answer they are entitled to;
`HOLOS_FAIL_CLOSED` is right where a partial answer would be read as a complete one.
[ACCESS-CONTROL.md](../ACCESS-CONTROL.md) is the model; the front door decides who the
principal is.

## 8. Development

A scratch store you can delete, every request an admin, and the console on.

```sh
# deploy/holos.env.local
HOLOS_STORE=./var/dev
HOLOS_DEV_ROLES=admin
HOLOS_DEV_CLEARANCE=5
```

`run.sh` prints a warning for each of the `HOLOS_DEV_*` keys every time it starts. That is
deliberate; they should never be quiet.

## 9. As a service

**systemd:** `holos.service` runs `deploy/run.sh` from `/opt/holos`, so the configuration is
the same file. Install the unit, put your values in `holos.env.local`, and:

```sh
sudo systemctl enable --now holos
sudo systemctl restart holos      # after editing holos.env.local
journalctl -u holos -n 50
```

**Windows:** there is no ExecStart, so `install-service.ps1` resolves the configuration to
flags *at install time* and bakes them into the service's command line. Re-run it after
changing `holos.env.local`; a restart alone does not pick the change up.

```powershell
deploy\install-service.ps1                 # from an elevated PowerShell 7
deploy\install-service.ps1                 # again, after editing holos.env.local
deploy\install-service.ps1 -Remove
```

The service discards stderr, which is where the server's diagnostics go. For logs, run it
under NSSM or a scheduled task with redirection.

## 10. Backups

`backup.sh` checkpoints the store while it is being served — the log is flushed and the
files hard-linked, so it is consistent and near-instant — and removes all but the last
`HOLOS_BACKUP_KEEP` (default 7). Retention matters: a checkpoint pins files compaction can
no longer delete.

```sh
# crontab
15 2 * * * /opt/holos/deploy/backup.sh /backups
```

A checkpoint on the same filesystem shares the store's files and is not an off-machine
backup; copy the newest one elsewhere. To restore, point `HOLOS_STORE` at a checkpoint (or
copy it back with the server stopped) and restart.

To offer backups over HTTP instead, set `HOLOS_BACKUP_DIR` and `HOLOS_BACKUP_ROLE`; the
endpoint exists only when both are set, and a client never names the path.

## 11. In a container

`docker-compose.yml` passes flags to the server directly rather than through `holos.env`,
because a container's configuration lives in the compose file. The `proxy` service is the
front door; the store is a named volume. `deploy/smoke.sh http://host:7878` verifies it
from outside.

---

## Checks

| Question | Command |
|---|---|
| What is in the store, and is there room? | `holos stats --store DIR` |
| Is the server up and answering? | `deploy/smoke.sh` |
| Which store did the server open? | its first line of output |
| Why does a query return nothing? | that first line; then `holos stats --store` on the directory the load used |
