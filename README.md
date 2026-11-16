# elbert-subsonic-server

The [hyper](https://hyper.rs) HTTP host for
[`elbert-subsonic-core`](https://github.com/evolvedmesh/elbert-subsonic-core): it turns the protocol
of core into a Subsonic and OpenSubsonic server on a socket, for the
[Elbert](https://github.com/evolvedmesh/elbert) player.

It holds the accept loop, the local-network peer filter, CORS and form-body handling, files with
`Range`, covers and audio, the share pages, and the optional parts (transcoding, podcasts and
HLS). It depends on core and on a `tokio::runtime::Handle` it is given, holds no globals (the app
that embeds it owns the runtime and how it is stopped) and is `#![forbid(unsafe_code)]`. What only
the app can do is a trait the app implements (core's `Link`, and `sonic::SonicIndex`) or
something core's `Backend` answers.

Licensed under the [MIT licence](LICENSE).

## Using it before 0.1.0 is on crates.io

Neither crate is on crates.io until `0.1.0`. Until then pin a git revision (a commit hash, never a
branch) of each, and use the same core revision this crate was built against:

```toml
[dependencies]
elbert-subsonic-server = { git = "https://github.com/evolvedmesh/elbert-subsonic-server", rev = "<commit>" }
elbert-subsonic-core = { git = "https://github.com/evolvedmesh/elbert-subsonic-core", rev = "<commit>" }
```

This crate itself depends on core by `git` and `version`: the `git` part serves until core is
published, and `cargo publish` uses the `version`, so core has to be published first.

## Public API

```rust
use elbert_subsonic_core::Server;

// `server` is a core `Server` (a `Link`, a `Backend`, the persisted `State`).
let handle = elbert_subsonic_server::spawn(&runtime_handle, server, 4533, stop_rx, |port| {
    println!("listening on {port}");
});
```

| Item | What it is |
|---|---|
| `spawn` | Serves a core `Server` on a port (any free port when it is taken) on the given runtime until the `watch` receiver says stop. Returns the task's `JoinHandle`. |
| `Http` | What one running host holds beside the library (caches of what only this side reads). |
| `bind`, `is_local_peer`, `is_link_local` | The listening socket and the peer filter: the API sends a password over plain HTTP, so only loopback, private, link-local and unique-local peers are answered. |
| `serve_file` | A file with `Range`, `HEAD` and conditional headers. |
| `Body`, `empty`, `full`, `status`, `channel_body` | The response body type and its constructors. |
| `sonic::SonicIndex` | The seam for sonic similarity: the app hears a file and returns its print. |
| `media` | `stream`, `download`, cover art and the landing page. |
| `audio`, `fetch`, `podcast` | The feature-gated parts (see below). |

The calls themselves (browse, search, edits, accounts) are core's: this crate only carries them over
HTTP and adds the calls that need a socket, a file or a runtime.

### Features

All are on by default. Without a feature, `stream` sends the file as it is, and the calls of the
missing group answer "no such call".

| Feature | What it adds |
|---|---|
| `transcode` | Decoding (symphonia), the MP3 and Opus encoders and the resampler: `stream` with a format, a bit-rate limit or an offset, and the `transcoding` and `transcodeOffset` extensions. |
| `podcasts` | The feed client (hyper over rustls, no OpenSSL) and parser: the podcast calls and the `getPodcastEpisode` extension. |
| `hls` | `hls.m3u8`. |

## Local development

The repository's `Cargo.toml` names core by git. To build against a sibling checkout of core
(`../elbert-subsonic-core`) create `.cargo/config.toml` (it is listed in `.gitignore`, so it is
never committed):

```toml
[patch."https://github.com/evolvedmesh/elbert-subsonic-core"]
elbert-subsonic-core = { path = "../elbert-subsonic-core" }
```

Then:

```bash
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo deny check
# every feature combination: none, and each alone
cargo test --no-default-features
cargo test --no-default-features --features transcode   # likewise podcasts, hls
```

The toolchain is pinned in `rust-toolchain.toml`; the minimum supported Rust version of the crate
is 1.88. CI (`.github/workflows/ci.yml`) runs all of the above (without the patch, so it needs the
core repository to be pushed). Pushing a `v*` tag runs `release.yml`: the same checks, then
`cargo publish` with the repository secret `CARGO_REGISTRY_TOKEN`.

The tests are the unit tests of the moved code and `tests/loopback.rs`, which runs the host on
loopback over a small in-memory `Backend`. The golden responses of the whole protocol stay in the
Elbert repository, which is the consumer of these crates.
