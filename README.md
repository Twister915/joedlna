# JoeDLNA

JoeDLNA is a portable, filesystem-first DLNA/UPnP AV media server written in Rust. It
publishes ordinary directories as a read-only `MediaServer:1` hierarchy without a database.

It currently supports:

- IPv4 SSDP discovery and advertisements;
- UPnP `ContentDirectory:1` browsing and `ConnectionManager:1` queries;
- DIDL-Lite metadata and GENA update events;
- HTTP `GET`, `HEAD`, single byte ranges, and DLNA content-feature headers;
- recursive filesystem watching with atomic catalog reloads;
- a settle-time guard that hides files while they are still being written;
- Samsung `X_SetBookmark` resume state in a small TOML file.

JoeDLNA is interoperability-oriented alpha software, not a DLNA Certified product. It does not
yet transcode, probe codecs, generate thumbnails, or support IPv6 discovery.

## Build

The repository selects nightly Rust through `rust-toolchain.toml`.

```sh
cp config.example.toml config.toml
$EDITOR config.toml
cargo run -p joedlna-bin -- check-config --config config.toml
cargo build --profile distribute -p joedlna-bin
```

`check-config` validates the configuration and scans its shares without opening network sockets.

## Run

```sh
target/distribute/joedlna serve --config config.toml
```

Stop other DLNA servers on the host before the first LAN test. All UPnP devices use UDP 1900,
so running multiple servers can make discovery results ambiguous.

JoeDLNA defaults to HTTP port 8201. Set `network.interface` if automatic detection chooses the
wrong LAN IPv4 address. Keep the configuration at a stable absolute path so the derived device
UUID remains stable.

Each `[[shares]]` entry becomes a top-level container:

```toml
[[shares]]
name = "Movies"
path = "/Volumes/Media/Movies"
media = ["video"]
```

`media` accepts any non-empty combination of `"video"`, `"audio"`, and `"image"`; omitting it
enables all three. Share paths, names, and scanner settings reload while the server runs. Network
settings require a restart.

Filesystem events are debounced by `scanner.settle_time_seconds`. A successful rescan atomically
replaces the catalog; invalid configurations and failed scans leave the last good snapshot active.
Hidden entries, unsupported extensions, and overlapping shares are rejected or skipped.

## Design

The filesystem is catalog authority. JoeDLNA builds immutable in-memory snapshots with stable
path-derived object IDs and never writes metadata into media shares. Bookmark state is user data,
not a media index.

See [architecture](docs/architecture.md), [protocol scope](docs/protocol-research.md), and
[platform support](docs/platform-support.md) for details. The reversible first-TV procedure is in
[docs/tv-testing.md](docs/tv-testing.md).

## Development

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Tests and `check-config` do not bind SSDP or advertise on the LAN.

## License

[MIT](LICENSE)
