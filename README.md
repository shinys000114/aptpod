# aptpod

An apt repository server written from scratch in Rust:

- local repositories as dist x component matrices (add, remove, promote,
  version retention, snapshots, rollback),
- mirrors of remote repositories, either byte-identical (upstream signature
  kept) or re-signed with your key, with the upstream layout replicated,
- a REST API, a web UI and the `aptpod` CLI, scheduled jobs.

**Status: pre-alpha.** Local repositories and verbatim HTTP mirrors work
through the server and the CLI; rsync mirrors, schedules and the web UI
are not there yet. See
[docs/plan.md](docs/plan.md).

## Quick start

```sh
# server: allow publishes under /srv/apt, then add the first admin
sudoedit /etc/aptpod/server.toml        # publish_roots = ["/srv/apt"]
sudo -u aptpod aptpod-server user add alice --role admin
sudo systemctl enable --now aptpod-server

# client
aptpod login --server https://apt.example.org --user alice
aptpod publish create ppa --path /srv/apt/ppa \
    --dist jammy,noble --component main --arch amd64,arm64 --origin Example
aptpod add ppa main hello_1.0-1~noble_amd64.deb      # dist from ~noble
aptpod ls ppa
```

Every change is previewed first and applied after confirmation (`-y` to
skip the question, `-n` to only preview); applying is refused if the
repository changed since the preview.

| Task | Command |
|---|---|
| add files or URLs, verify checksums | `aptpod add ppa main *.deb --checksums SHA256SUMS` |
| remove, keep the newest N | `aptpod rm ppa main 'foo-*'`, `aptpod prune ppa --keep 3` |
| copy to another dist/component/publish | `aptpod promote ppa main foo --to-component stable` |
| labels, history, diff, undo | `aptpod snapshot create ppa before-x`, `aptpod history ppa`, `aptpod diff ppa before-x`, `aptpod rollback ppa before-x` |
| search everywhere | `aptpod find 'linux-image-*' --newest` |
| change a layout | `aptpod publish edit ppa` (TOML in `$EDITOR`), `aptpod publish set ppa layout.toml` |
| all layouts as one file | `aptpod config export > repos.toml`, `aptpod config import repos.toml` |
| CI token | `aptpod token create jenkins`, then `APTPOD_TOKEN=... aptpod add ...` |
| users (admin) | `aptpod user add bob --role publisher`, `aptpod user ls` |
| watch jobs | `aptpod jobs --follow`, `aptpod log <job> -f` |
| mirror a remote repository (upstream signature kept) | `aptpod mirror create debian --url http://deb.example.org/debian/ --path /srv/mirror/debian --dist bookworm --arch amd64 --keyring debian.gpg`, `aptpod mirror sync debian --wait` |
| maintenance (admin) | `aptpod gc`, `aptpod repair` |

Roles: `reader` (read only), `publisher` (packages, snapshots, rollback),
`admin` (publishes, users, config import, gc, repair). Exit codes: 2 usage,
3 stale preview, 4 conflict, 5 needs `--force`, 6 saved but a tree is
behind (`aptpod repair`), 7 authentication or permission.

The server only writes `pool/` and `dists/<dist>` under each publish path;
serve the trees with any web server.

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
