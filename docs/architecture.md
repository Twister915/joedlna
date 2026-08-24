# Architecture and storage decision

## Data flow

JoeDLNA has five intentionally narrow components:

1. The config loader validates named directory roots and rejects overlap.
2. The scanner recursively walks those roots using `std::fs`, selects recognized media extensions, rejects files younger than the settle window, and constructs deterministic IDs. Recursion is configurable independently from symlink following.
3. The catalog is an immutable parent/child snapshot behind `Arc<RwLock<Arc<Catalog>>>`. Request handlers clone one `Arc`; scans never mutate a snapshot visible to a client.
4. UPnP control maps `Browse` calls to catalog slices and serializes DIDL-Lite. SSDP only advertises HTTP endpoints; GENA reports catalog update IDs.
5. The media handler resolves only catalog IDs, then streams the already-authorized path with bounded byte ranges. No request path is ever interpreted as a filesystem path.

The scanner builds off to the side. The native watcher maps to FSEvents on macOS, inotify on Linux, ReadDirectoryChangesW on Windows, and kqueue on supported BSDs. It treats events only as dirty hints. Relevant event bursts reset one quiet-time deadline; after the tree settles, a full scan rebuilds authority from current filesystem state. A successful changed scan replaces the current `Arc` in a very short write-lock section. A failed scan retains the previous snapshot.

Files whose modification age is below `settle_time_seconds` are excluded and cause another settle scan to be scheduled. This stops both event-triggered and periodic fallback scans from publishing a live encode. The periodic fallback remains necessary because filesystem notification APIs can lose events and some network filesystems do not provide them.

## Object identity

Object IDs use a fixed FNV-1a implementation over the canonical share root and relative platform-native path representation: raw bytes on Unix and UTF-16 code units on Windows. They are opaque hexadecimal values with a directory/file prefix. They remain stable across restarts and config reorderings on the same operating system, but intentionally change when a path changes. IDs are not promised to match when the same library is moved between operating systems.

`SystemUpdateID` and per-container `UpdateID` values are deterministic fingerprints of relevant catalog metadata. An unchanged cold restart therefore preserves the values without persistent state. As with every 32-bit UPnP update ID, wraparound or collision is theoretically possible.

This is the right semantic trade for a path-oriented server. Rename-stable IDs would require persistent filesystem identity tracking and therefore a cache.

## In-memory catalog versus SQLite

| Concern | In-memory snapshot | Global SQLite cache | Per-folder hidden database |
|---|---|---|---|
| Plain filename browsing | Simple and sufficient | Unnecessary write path | Unnecessary write path |
| Startup | Full metadata walk | Incremental when cache is valid | Incremental per mounted root |
| Expensive tags/thumbnails | Recomputed | Strong fit | Technically possible |
| Hot add/remove share | Atomic config reconciliation | Atomic reconciliation plus cache rows | Must coordinate many databases |
| Read-only/network volumes | Works | Works; cache lives locally | Often impossible or undesirable |
| Folder portability | Nothing to manage | Cache is intentionally local | Database travels with media |
| Failure model | Drop failed candidate scan | Migrations/corruption need handling | Partial availability and schema skew |
| Privacy/cleanliness | No artifacts | One known local artifact | Metadata debris in every share |

The current workload does not justify SQLite. A future cache is warranted when at least one of these becomes true:

- cold scans are measurably too slow for the actual library;
- clients require duration, resolution, codec, bitrate, EXIF, album art, or thumbnails;
- rename-stable object IDs or playlists are required;
- ContentDirectory `Search` must be fast over a large metadata set.

If introduced, the cache should use the operating system's user-cache directory, remain disposable,
and never decide whether a file currently exists. Samsung resume positions remain separate user state
in `bookmarks.toml`; they do not justify a relational catalog.

## Concurrency and backpressure

Filesystem scanning batches the entire blocking metadata walk into one `spawn_blocking` task. Tokio's filesystem API also delegates ordinary filesystem operations to its blocking pool; keeping one synchronous traversal avoids an await and task handoff per directory entry. HTTP file bodies use Tokio streaming and a length-limited reader, so large media is not buffered in memory. SOAP requests operate on a single immutable snapshot even if a rescan completes mid-request. SSDP search responses apply a bounded delay within the caller's `MX` window.

GENA callback delivery is bounded by a three-second timeout and never holds the subscription mutex during network I/O. Callback URLs must be literal IPv4 HTTP addresses in the same RFC 1918 address family as the configured interface, limiting the event endpoint's SSRF surface.

## Deliberate boundaries

- IPv4 only for the first compatibility milestone.
- Pull-mode HTTP means a MediaServer does not need `AVTransport`.
- No transcoding: a file is advertised with its actual MIME family and `DLNA.ORG_CI=0`.
- No `DLNA.ORG_PN` is claimed from an extension alone. Profile names require actual bitstream/container validation.
- No ContentDirectory `Search`; `GetSearchCapabilities` truthfully returns an empty set.
- One HTTP range per request. Multipart ranges are rejected with 416.
