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
  aptpod-server   axum API, auth, jobs (per-prefix locks, SSE logs),
                  scheduler (cron), audit, layout revisions, web UI
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
write is a job; `dry_run` for previews; SSE job logs). Read: status, plan,
packages, snapshots, diff, mirrors, jobs. Write: sync, add (upload or URL +
checksums), rm, promote, prune-versions, snapshot, rollback, mirror sync, gc.
Admin: publishes, mirrors, users, tokens, layout revisions.

## Phases

| Phase | Scope | Done when |
|---|---|---|
| P0 ✅ | Repo, workspace, CI (fmt, clippy, test), release pipeline (musl x86_64/aarch64, two .deb), license/NOTICE | a tag produces both .deb packages |
| P1 ✅ | aptpod-core: version compare (cross-checked with dpkg), .deb parsing (gz/xz/zst), index writer, by-hash, gpg sign, rpgp verify, pool, atomic swap | containers `apt-get update && install` from a generated tree; indices match apt-ftparchive |
| P2 | aptpod-repo: layout edits with dry-run, publish create/delete, add incl. URL + checksums, rm with globs and version conditions, promote, keep, prune-versions, revisions + snapshot labels, diff, rollback, history retention, refcounted pool cleanup | the aptrix integration scenarios (minus the dropped aptly-only parts) ported and passing |
| P3 | aptpod-server + aptpod CLI: API, users/tokens/roles, jobs + SSE + per-prefix locks, audit, layout revisions, TOML import/export, systemd unit | CI uploads and every CLI command go through the server |
| P4 | aptpod-mirror verbatim (HTTP, then rsync), scheduler | the reference PPA and Debian/Ubuntu mirrors run from aptpod with identical output |
| P5 | Web UI: matrix, packages, upload/promote with preview, snapshots/diff/rollback, layout editor + history, mirrors, jobs, users/tokens | day-to-day work possible from a browser |
| P6 | Re-signed mirrors | auto-replicated layout, filters, snapshot per sync |
| P6.5 | Native Debian packaging: `debian/` (control, rules with vendored crates, changelog, copyright, systemd unit via debhelper), source package builds with `debuild`; CI builds it too | `debuild -us -uc` produces `aptpod` and `aptpod-server` .debs equivalent to the cargo-deb ones |
| P7 | Migration of the reference host: import aptly's published trees (`dists` + `pool`, no aptly API needed), publish side by side, compare, switch the web server's symlinks, switch CI, move mirror jobs one by one, remove aptly and the old scripts | same paths and key, no client-visible change |

## Testing

- Unit tests per crate; dpkg cross-check for version ordering.
- Golden files for index generation.
- Container tests with real apt (docker) in CI.
- End-to-end server tests: start `aptpod-server` on a temp dir, drive it with
  the CLI.
- Mirror tests against a local HTTP fixture repo and a local rsync daemon.
