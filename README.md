# aptpod

An apt repository server written from scratch in Rust:

- local repositories as dist x component matrices (add, remove, promote,
  version retention, snapshots, rollback),
- mirrors of remote repositories, either byte-identical (upstream signature
  kept) or re-signed with your key, with the upstream layout replicated,
- a REST API, a web UI and the `aptpod` CLI, scheduled jobs.

**Status: pre-alpha.** Nothing works yet; see [docs/plan.md](docs/plan.md).

## Packages

| Package | Contents |
|---|---|
| `aptpod-server` | `aptpod-server` service (systemd unit, `/etc/aptpod/server.toml`) |
| `aptpod` | `aptpod` command line client |

Static binaries for x86_64 and aarch64 and both .deb packages are attached to
each GitHub release.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Release: push a `vX.Y.Z` tag; GitHub Actions builds musl binaries
(x86_64, aarch64) and the .deb packages and publishes the release.

## License

GPL-3.0-only. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
