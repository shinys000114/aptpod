# aptpod plan

Status: draft, 2026-11-20.

aptpod is an apt repository server written from scratch in Rust: local
repositories (dist x component matrices), mirrors of remote repositories
(byte-identical or re-signed), snapshots, retention, scheduled jobs, a REST
API, a web UI and a CLI. It does not depend on aptly; aptly's source (MIT) is
used as a reference for formats and edge cases.

## Reference deployment

One host that today runs aptly plus mirror scripts:

| Today | aptpod |
|---|---|
| aptly local repos: 3 publishes (9, 4 and 1 components x 4-5 dists, arm64/amd64), CI uploads | local repository engine |
| rsync mirrors of Debian, Debian security, Ubuntu, Ubuntu ports | verbatim mirror, rsync source |
| apt-mirror of a third-party PPA | verbatim mirror, HTTP source |
| (none) | re-signed mirrors with auto-replicated layout |
| static web server (with host aliasing) serves all trees | unchanged: aptpod only produces directories |
| cron + sequential script | aptpod-server scheduler and job queue |

## Components

| Binary / package | Role |
|---|---|
| `aptpod-server` (`aptpod-server.service`) | Owns the database, the package pool and the published trees. REST API, web UI, jobs, scheduler. |
| `aptpod` | CLI client of the API (login + token): `add`, `rm`, `promote`, `ls`, `find`, `status`, `prune-versions`, `snapshot ...`, `rollback`, `history`, `--json`, plus `publish`, `mirror`, `job`, `config`, `user`, `token` (see "Carry-over from aptrix"). |

Web UI is served by `aptpod-server` (server-rendered templates + htmx,
embedded in the binary).

## Decisions

| Topic | Decision |
|---|---|
| Language | Rust (stable), cargo workspace |
| Backend | Own implementation; no aptly at runtime |
| Layout source of truth | Server database, edited via API/UI/CLI, revision history. TOML is only a transport: `aptpod config import <file>` / `export`, or set things one by one with CLI commands (`aptpod publish create ...`, `aptpod mirror add ...`) |
| Auth | Local accounts (argon2) + API tokens (sha256-stored); roles `reader`, `publisher`, `admin` |
| Web UI | askama templates + htmx (vendored), embedded |
| Serving files | Existing web server; aptpod writes directory trees |
| Signing | Call `gpg` (existing keyring + passphrase file); verification in-process (rpgp) |
| Mirrors first | Verbatim (keep upstream signature), then re-signed |
| Storage | SQLite (bundled), content-addressed package pool on disk |
| License | GPL-3.0-only; derived parts credited to aptly (MIT) in NOTICE |
| Builds | static musl binaries x86_64 + aarch64, two .deb packages (cargo-deb), GitHub Actions. Later: a `debian/` directory so `debuild` / `dpkg-buildpackage` build source and binary packages directly (debhelper, crates vendored with `cargo vendor` for offline builds) |

## Carry-over from aptrix

aptrix wrapped aptly, so part of it only existed to drive aptly's model.

**Dropped** (aptly artifacts):

- local repo names (`repo_name`, repo `comment`): cells are internal rows
- `storage` + `prefix` and their URL escaping: a publish has one output `path`
- `plan`/`sync` creating repos and staging publish sources, `--discard-staged`:
  a layout change regenerates the tree directly, there is no staged state
- `retire = true` and `--prune`: deleting a publish is an explicit
  `publish delete` (non-empty needs `--force`)
- drift markers (`!` missing repo, `?` unpublished) and Origin/Label/arch
  drift warnings: there is a single source of truth
- `<repo>@<label>` snapshot naming and its problems (blocked repo drops,
  orphaned snapshots): snapshots are per publish
- manual `gc` (aptly db cleanup): pool files are refcounted and removed when
  unreferenced
- upload dirs, `noRemove`, `FailedFiles`, ForceOverwrite on publish
- aptly package keys (`P<arch> <name> <ver> <hash>`)
- `--no-publish`: one request is one transaction
- `--no-snapshot`, automatic `pre-*` snapshots, `snapshot_max_age`: every
  change is recorded as a revision; retention is `history_max_age`
- shared config URL, `APTRIX_URL`, `--url`, profile.d: the server owns the
  layout, clients only need the server URL and a token
- `signing.skip` loopback rule: replaced by a per-publish `sign = false`
- client-side read-only guard: replaced by the `reader` role

**Changed**:

- snapshots: a label on a publish revision (cheap); `rollback` restores a
  label or a revision; `snapshot prune` becomes retention on labels/history
- `--force-replace` -> `--replace` (same name/version/arch, different
  content is refused without it)
- `plan` -> dry-run preview of a layout edit ("removing component X drops
  N packages")
- Release metadata per publish: Origin, Label, Suite/Codename, Description,
  Valid-Until, NotAutomatic/ButAutomaticUpgrades

**Kept**: dists x components with `only`, architectures; `add` (files, URLs
+ `--checksums`, `~<dist>` detection, `--dist all`); `rm` (globs,
`name=version`, `--version`, `--keep`); `promote` (`--to-dist`, `--move`,
`--all-versions`); `ls`, `find --newest`; `keep = N` and `prune-versions`
(consider per-cell rules for packages shared by several builds, e.g.
`linux-libc-dev`); `--json`; confirmations with `--yes` (required without a
TTY).

## Workspace

```
crates/
  aptpod-core     deb parsing (ar + control.tar.{gz,xz,zst}), dpkg version
                  compare, control/RFC 822 fields, Packages/Release/InRelease
                  writer, compression, by-hash, gpg signing, rpgp verify,
                  pool layout, atomic tree swap
  aptpod-repo     local repository engine on SQLite: layout edits (dry-run),
                  add/rm/promote, keep, revisions + snapshot labels, rollback,
                  diff, refcounted pool cleanup
  aptpod-mirror   fetch engine: apt-over-HTTP (InRelease -> indices ->
                  pool, by-hash, parallel, resume, verify), rsync two-pass,
                  re-signed import into aptpod-repo
  aptpod-server   axum API, auth, single writer (jobs with resource
                  keys, SSE logs), scheduler (cron), audit, layout
                  revisions, web UI
  aptpod-cli      clap CLI over the API, credentials file, --json
tests/            end-to-end: real apt in Debian/Ubuntu containers
```

Main crates: tokio, axum, tower-http, rusqlite (bundled), serde, toml, clap,
reqwest (rustls), ar, tar, flate2, xz2/liblzma (static), zstd, sha2, sha1,
md-5, pgp (rpgp), argon2, askama, cron, tracing.

## Repository format (what "compatible" means)

- Pool: `pool/<component>/<prefix>/<source>/<file>`, prefix = first letter or
  `lib` + letter. Files are hardlinked from the content-addressed store.
- `Packages`: control fields in order, then `Filename`, `Size`, `MD5sum`,
  `SHA1`, `SHA256`, `SHA512`; sorted by name, version, arch; `Architecture:
  all` packages appear in every `binary-<arch>`. Written plain, `.gz`, `.xz`.
- Per-component `Release` in each `binary-<arch>`.
- `dists/<dist>/Release`: Origin, Label, Suite, Codename, Date (UTC,
  RFC 2822), optional Valid-Until, Architectures, Components,
  `Acquire-By-Hash: yes`, MD5Sum/SHA1/SHA256/SHA512 file lists.
  `InRelease` (clearsigned) and `Release.gpg` (detached, armored).
- `by-hash/SHA256/<hash>` copies of every index.
- A publish is built in a temporary directory next to the target and swapped
  in per dist; pool files are in place before the new indices appear.
- Optional (later): `Contents-<arch>`, `Sources`, udebs.

Findings from P1 (verified, not assumed):

- `Packages` stanzas, their order (name, newest version first, arch) and the
  trailing blank line match aptly byte for byte: regenerating a published
  aptly dist from its own pool gives identical `Packages`, identical
  per-component `Release` and identical pool paths, so the checksums in the
  dist `Release` are the same.
- Release header: Origin, Label, Suite, Codename are reproduced exactly
  (apt warns when those change); `Description` is per-publish metadata.
- apt 2.6 (bookworm) fails with "Internal parsing error" when a binary's
  `Source: x (ver)` is newer than the binary version. apt-ftparchive emits
  the same stanza, so this is an input problem; `add` should warn about it.
- by-hash is off by default and per publish; when enabled, a dist swap must
  carry the previous generation's `by-hash` files forward.
- On filesystems without hard links or `RENAME_EXCHANGE` (e.g. mergerfs)
  pool files are copied and the dist swap falls back to two renames.

Compatibility is proven by tests, not by inspection: CI runs `apt-get update`
and `apt-get install` against generated trees in Debian (bookworm, trixie)
and Ubuntu (jammy, noble, resolute) containers, and compares indices with
`apt-ftparchive` output for the same inputs.

## Data model (SQLite)

- `publishes`: alias, output path, dists, components, only, architectures,
  Release metadata (origin, label, suite/codename, description,
  valid_until, not_automatic), sign, keep, history_max_age.
- `packages`: key (arch, name, version, sha256), control fields, size, hashes,
  pool path. `files`: content store (sha256, size, refcount).
- `cells`: (publish, dist, component) -> set of package keys.
- `revisions`: every change to a publish (who, when, what), each one a
  frozen set of cell contents; `snapshots` are labels on revisions.
- `mirrors`: name, mode (`verbatim` | `resign`), source (`http` | `rsync`), url,
  dists (`auto` or list), components, architectures, filter globs,
  upstream keyring, target path or publish, schedule, keep_snapshots, state.
- `layout_revisions`, `users`, `tokens`, `jobs` (status, log, result),
  `audit`.

## Mirrors

**Verbatim / HTTP**: fetch `InRelease` (or `Release` + `.gpg`), verify with
the mirror's upstream key, read the listed indices (prefer by-hash), verify
size and hashes, collect every referenced pool file, download missing ones in
parallel with retries and resume, then swap `dists/` in. Removed files are
deleted after the swap (optionally after a grace period). The output tree is
byte-identical to upstream for the selected dists/components/architectures.

**Verbatim / rsync**: pass 1 `pool/` (no delete), pass 2 `dists/` + delete,
then a trace file. Same scheduling, locking, logging and status as HTTP.

**Re-signed**: same fetch engine, then the packages are imported into an
aptpod publish whose layout is generated from the upstream `Release`
(components/architectures auto or filtered, package globs), snapshotted per
sync and published with our key. Rollback = republish an older snapshot.

Dist discovery: listed explicitly, or `auto` when the upstream exposes a
directory listing of `dists/`.

## API and CLI

As in the aptrix design (`/api/v1`, Bearer token or session + CSRF; every
write is a job; stateless preview/apply bound to head revisions, see P3
notes; SSE job logs). Read: status, plan,
packages, snapshots, diff, mirrors, jobs. Write: sync, add (upload or URL +
checksums), rm, promote, prune-versions, snapshot, rollback, mirror sync, gc.
Admin: publishes, mirrors, users, tokens, layout revisions.

## Phases

| Phase | Scope | Done when |
|---|---|---|
| P0 ✅ | Repo, workspace, CI (fmt, clippy, test), release pipeline (musl x86_64/aarch64, two .deb), license/NOTICE | a tag produces both .deb packages |
| P1 ✅ | aptpod-core: version compare (cross-checked with dpkg), .deb parsing (gz/xz/zst), index writer, by-hash, gpg sign, rpgp verify, pool, atomic swap | containers `apt-get update && install` from a generated tree; indices match apt-ftparchive |
| P2 ✅ | aptpod-repo: layout edits with dry-run, publish create/delete, add (local files, optional expected sha256), rm with globs and version conditions, promote, keep, prune-versions, revisions + snapshot labels, diff, rollback, history retention, refcounted pool cleanup, repair | the aptrix integration scenarios (minus the dropped aptly-only parts) ported and passing |
| P3 ✅ | aptpod-server + aptpod CLI: API, users/tokens/roles, single writer + jobs with resource keys + SSE, audit, layout revisions, TOML import/export, add from URLs (fetched by the CLI, then uploaded) + `--checksums`, systemd unit | CI uploads and every CLI command go through the server |
| P4 | aptpod-mirror verbatim (HTTP, then rsync), scheduler | the reference PPA and Debian/Ubuntu mirrors run from aptpod with identical output |
| P5 | Web UI: matrix, packages, upload/promote with preview, snapshots/diff/rollback, layout editor + history, mirrors, jobs, users/tokens | day-to-day work possible from a browser |
| P6 | Re-signed mirrors | auto-replicated layout, filters, snapshot per sync |
| P6.5 | Native Debian packaging: `debian/` (control, rules with vendored crates, changelog, copyright, systemd unit via debhelper), source package builds with `debuild`; CI builds it too | `debuild -us -uc` produces `aptpod` and `aptpod-server` .debs equivalent to the cargo-deb ones |
| P7 | Migration of the reference host: import aptly's published trees (`dists` + `pool`, no aptly API needed), publish side by side, compare, switch the web server's symlinks, switch CI, move mirror jobs one by one, remove aptly and the old scripts | same paths and key, no client-visible change |

## P2 notes

Consistency rules the engine enforces:

- The database is the truth. Each dist records the revision it was last
  written from; `apply` commits first, then syncs the tree, and `repair`
  (or the next apply) finishes a publish that failed half way.
- Every change is a plan bound to the head revisions it was computed from;
  `apply` refuses a stale plan.
- A pool path is shared by every dist of a component: conflicts are checked
  across the component, and `--replace` swaps the content in every cell that
  lists it.
- aptpod only deletes under `pool/` and `dists/<dist>` of its own trees;
  publish paths may not be equal or nested.
- Ingest writes the file before its row; gc deletes rows before files.
- Revisions share unchanged cells (content-addressed cell sets); unchanged
  dists are not rewritten.
- A signed publish without a signer is an error, never an unsigned publish.
- A dist left without components (via `only`) is not published.
- Pool placement is decided per pool path from what the published
  revisions put there: same content -> trusted without re-hashing, other
  content -> replaced (rollback over `--replace`, interrupted replace),
  unknown -> verified. Two different files with the same pool path in one
  `add` are refused.
- A publish path that already holds `pool/` or one of its `dists/<dist>` is
  refused unless `adopt` (which rewrites those dists and deletes unlisted
  pool files). A path move removes only the dists aptpod recorded.
- After the commit, tree failures are `Unsynced` (repair, do not re-plan).
- `gc` keeps unreferenced packages ingested in the last hour (a pending
  plan may use them). The store copies files (mode 0644), never links the
  caller's file.

aptrix scenarios: ported - matrix lifecycle (detect/all/list dists, cell and
arch errors, conflict and replace, newest-first listing, rm keep and
name=version, add/remove component with force, drop dist), signed publish,
glob rm + rollback + undo, keep per name, snapshot create/drop, keep on add,
find (globs, newest, version conditions), promote (copy, no-op, to-dist,
move, same-cell refusal), prune-versions, diff, history expiry + gc keeping
labelled content, cross-publish promote (arch refusal, shared dists only),
publish delete. New - stale plan refusal, replace across dists, repair after
a signing failure half way, path overlap. Dropped - `sync` confirmation and
staged sources, `--prune`/`retire`, `--no-snapshot`/`pre-*` labels, aptly
`gc`, URL add (P3).

## P3 notes

- **One writer.** A single thread owns the writing repository handle and
  runs jobs in arrival order; each job records a resource key
  (`publish:<alias>`, later `mirror:<name>`) in the job log. Queries use a
  second, read-only handle and never wait for the writer. Per-resource
  parallelism can come later without changing the API.
- **Stateless preview/apply.** A change request computes a plan and returns
  it with `bases` (the head revision of each publish it touches). Applying
  repeats the same request with `apply: true` and `expect: <bases>`; the
  writer recomputes the plan and refuses with `stale` (409) if any head
  moved. Nothing is kept on the server between the two calls.
- **Uploads.** `POST /api/v1/uploads` streams the body to
  `<data_dir>/uploads/<random id>.deb` (limit `upload_limit`, default
  2 GiB); later requests refer to the id. Applied uploads are deleted at
  once, others after an hour. URLs given to `aptpod add` are downloaded by
  the CLI and uploaded: the server never fetches on a client's behalf.
- **Auth.** Users with argon2 passwords and a role: `reader` (GET),
  `publisher` (package changes, uploads), `admin` (publishes, users, gc,
  repair, config import). Tokens are random, stored as sha256, shown once.
  Users live in `server.db` (mode 0600) apart from the repository
  database, so `aptpod-server user ...` works next to a running server.
  Passwords never come from argv (`--password-stdin` or a no-echo prompt);
  the CLI's credentials file is mode 0600.
- **Publish paths** must be under a `publish_roots` entry of server.toml.
- **Startup**: lock `<data_dir>/lock` (one server per data dir), mark jobs
  left running as interrupted, clean interrupted publishes, repair, drop
  stale uploads. SIGTERM lets the running job finish.
- **Errors** map to statuses and CLI exit codes: invalid 400 (1),
  unauthorized/forbidden 401/403 (7), not found 404 (1), stale 409 (3),
  conflict 409 (4), blocked 409 (5), unsynced 500 (6); usage errors exit 2.

Progress: P3a done - config, lock, users/tokens/roles, writer, job log,
uploads, publish create, add with preview/apply, packages listing. P3b
done - rm, promote (components, dists, publishes), prune-versions,
rollback, layout set/edit (TOML via file, stdin or $EDITOR), publish
delete, history, diff, snapshots (create, drop, prune), find, gc (history
expiry + unreferenced packages) and repair, each with its CLI command;
`rm`/`promote` refuse an empty selection without `--all`. Both are covered
by end-to-end tests of the real server and CLI. P3c/d done - users and tokens through the API (`aptpod token`, `aptpod
user`, `aptpod passwd`; the last admin cannot be removed or demoted),
`add --checksums` (sha256sum file or URL, checked against the server's
hash of each upload), `aptpod config export/import` (all publish layouts
as `[publishes.<alias>]` TOML, imported as one plan; publishes missing from
the file are left alone, unchanged ones get no revision), job events over
SSE (`GET /api/v1/events`, `aptpod jobs --follow`; streams end on
shutdown), README quick start. Release signing through `[signing]` is tested end
to end (a broken key leaves the change saved and the old tree intact;
startup repair signs it once the key works). The .deb packages are
install-tested in Debian bookworm/trixie and Ubuntu jammy/noble containers
(postinst user and directories, `sudo` refusal, server as the service user,
publish, `apt-get install` from the tree, clean stop, purge); this found the
missing `ca-certificates` dependency.

Open points: previews also run on the writer (they ingest uploads), so P4
mirror syncs must not hold it for long stretches (download outside the
writer, commit inside). Signing through server.toml `[signing]` has no
end-to-end test yet (the engine's signing is covered in P1/P2). A saved
token is sent only to the server URL it was issued by.

## P4 design (verbatim mirrors, scheduler)

Decided: aptpod takes over everything the reference host's mirror tool
does, including the plain-tree rsync mirrors and the public status JSON
and SVG badges, so that tool can be removed in P7; mirrors live under
their own `mirror_roots`; schedules use a configurable time zone, UTC by
default.

What the reference host runs today, to be replaced one job at a time:

- one third-party PPA over HTTP: 7 dists x 21 components, one
  architecture, no sources; every payload verified, failed ones re-fetched
  (3 tries); a short ignore list of pool paths the upstream indexes still
  reference but the server answers 404/410 for (checksum, timeout and other
  server errors stay fatal);
- four full distribution archives over rsync in "apt" mode: pass 1 without
  indices (`Packages*`, `Sources*`, `Release*`, `InRelease`), pass 2 with
  `--delete-after`, `--max-delete` as a brake, hard links kept,
  `Archive-Update-in-Progress-*` excluded, then a `project/trace/<host>`
  file;
- plain-tree rsync mirrors (cloud images), single pass;
- runs from one timer, sequential or with rsync jobs in parallel after the
  HTTP mirror; per-job logs, a status JSON per job and an SVG status badge
  (synced/failed/interrupted + timestamp) served publicly; stop with a
  timeout; an upstream probe command.

Upstream facts (the HTTP PPA, probed read-only): `InRelease` and
`Release` + `Release.gpg` are both served; no `Acquire-By-Hash`; indices
come plain and gzip; some indices listed in `Release` answer 404 (a whole
component is advertised but not served); `dists/` has a directory listing.
The engine must not assume the shape aptpod-core writes.

Model: a mirror is server configuration (DB, API, CLI, `[mirrors.<name>]`
in `aptpod config`): name, kind (`http`, `rsync-apt`, `rsync-tree`),
source URL, destination path (under `mirror_roots`), enabled, schedule,
and per kind (destination rules below):

- `http`: dists (explicit), components, architectures, sources and udebs
  on/off, upstream keyring (armored or binary; required unless
  `insecure = true`), `ignore_missing` paths, parallel downloads, retries;
- `rsync-*`: max_delete, extra excludes, timeouts, trace host.

Engine (`aptpod-mirror`, no server dependency, testable on its own):

1. `http`: fetch `InRelease` (fallback `Release` + `Release.gpg`) per
   dist, verify with the keyring, parse the checksums. Index selection: for
   each selected component and architecture, `binary-<arch>/` (plus
   `binary-all` when listed), `debian-installer/` only with udebs,
   `source/` only with sources, `i18n`, `Contents-*`, `dep11` only when
   listed in `extras`; of each index every listed compression variant that
   exists is mirrored (`by-hash` when advertised), the best one (xz, gz,
   plain) is parsed. A listed index that answers 404 is absent, not an
   error; one with a wrong size or hash is an error. Everything is checked
   against the one `Release` read at the start; if upstream changes it
   mid-sync (hash mismatches on indices), the dist restarts, at most 3
   times. Then parse the package lists; download missing or changed pool files into place
   (`.part` + rename, resume, size + sha256 checked, parallel, retries);
   only then write the dist's index files and Release files byte for byte
   into a staging `dists/<dist>` and swap it in atomically (core's swap);
   finally delete pool files no dist references any more, with a
   `max_delete` brake. The reference set is computed from the dists as
   they are on disk after the run, including a dist that failed and kept
   its old indices; if any dist's set cannot be read, nothing is deleted.
   Pool files listed by an upstream index but answering 404/410 are an
   error unless their path is in `ignore_missing`. A pool file already present with the expected size
   and a recorded hash is not re-read; a full verify is a separate option
   (`verify = "all"`).
2. `rsync-apt` / `rsync-tree`: run rsync as above (two passes / one),
   stream its output to the job log, map exit codes (24 "vanished" is not
   a failure), write the trace file.

Destinations: absolute paths under `mirror_roots`, a server.toml list of
their own (mirrors usually live on another disk and grow far larger than
local repositories), never overlapping a publish or another mirror
(`paths_overlap`). A non-empty destination needs
`adopt`; on an adopted tree (for example one an older tool filled without
cleaning) the first sync reports what it would delete and deletes
nothing, and `aptpod mirror sync <name> --prune` deletes it once reviewed.

Server: mirror syncs do not touch the repository database, so they run on
a mirror worker pool next to the single writer (`mirror:<name>` resource
keys, one sync per mirror at a time, `mirror_parallel` workers, default 1).
Every job gets a log file (`<data_dir>/logs/<job>.log`) streamed over SSE;
`aptpod jobs logs <id> [--follow]`. SIGTERM stops downloads between files
and sends SIGTERM to rsync; the job ends `interrupted`. Status per mirror:
last start/end/result/bytes/files, exposed by the API and written as JSON
and SVG badge into an optional public `status_dir`.

Scheduler: per-mirror cron expressions (5 fields; UTC unless server.toml
sets `timezone`, e.g. `Asia/Seoul` - read from `/usr/share/zoneinfo`,
since local time in a static multi-threaded binary cannot come from libc;
verified before relying on it) plus
`aptpod mirror sync <name|--all>` on demand; a run that is still busy is not
started twice; missed runs while the server was down are not replayed.

CLI: `aptpod mirror ls|show|create|edit|delete|sync|status|check`, `jobs
logs`. Done when, for the reference PPA and the same selection, every file
the mirrored indices reference is present with the right hash, every
index and Release file written is byte-equal to upstream, `apt-get update
&& apt-get install` works from the result in a container, and the rsync
jobs run from the scheduler with the same results as today. (The current
tree is never cleaned, so "same files" against it cannot hold.)

rsync runs in the server's cgroup: a memory-hungry sync can get the server
killed with it. Run children with a higher `oom_score_adj`, and document
`MemoryMax` for the unit.

Order: P4a engine for `http` with tests against a local HTTP server: a
signed tree built by aptpod-core plus a hand-built fixture with the
upstream quirks (no InRelease, no by-hash, listed-but-absent indices,
`binary-all`, an index changing mid-sync), covering verify, resume,
missing files, corrupt payloads, the ignore list, deletes, the brake and a
failed dist; and real apt in a container against a mirrored subset;
P4b mirror model, worker pool, job logs, API and CLI; P4c rsync kinds;
P4d scheduler, status JSON/badges, probe; P4e side-by-side comparison on
the reference host (into a separate directory, nothing switched).

## Testing

- Unit tests per crate; dpkg cross-check for version ordering.
- Golden files for index generation.
- Container tests with real apt (docker) in CI.
- End-to-end server tests: start `aptpod-server` on a temp dir, drive it with
  the CLI.
- Mirror tests against a local HTTP fixture repo and a local rsync daemon.
