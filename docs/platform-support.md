# Platform support

JoeDLNA's protocol and catalog layers are portable safe Rust. The operating-system boundaries are network sockets, filesystem paths, native change notifications, hostname discovery, and service installation.

## Verified build targets

The following targets compile and pass target-specific Clippy with warnings denied:

| Platform | Rust target | Watch backend | Status |
|---|---|---|---|
| macOS Apple Silicon | `aarch64-apple-darwin` | FSEvents | Native tests and release build |
| Linux x86-64 | `x86_64-unknown-linux-gnu` | inotify | Cross-target check and Clippy |
| Linux ARM64 / Raspberry Pi | `aarch64-unknown-linux-gnu` | inotify | Cross-target check and Clippy |
| Windows x86-64 | `x86_64-pc-windows-gnu` | ReadDirectoryChangesW | Cross-target check and Clippy |

The Linux ARM64 target covers the normal 64-bit operating systems used by Raspberry Pi 3, 4, 5, 400, and Zero 2 W. A 32-bit Pi target has not yet been compiled, but it uses the same Linux code paths. BSD platforms use the Unix path implementation and notify's kqueue backend but have not yet been built or run.

## Runtime considerations

- JoeDLNA currently supports IPv4 UPnP/SSDP only. The host and television must share IPv4 connectivity.
- UDP 1900 and the configured HTTP port, 8201 by default, must be allowed through the host firewall.
- macOS, Linux, and Windows use different native watcher implementations. A 300-second full rescan remains active as a correctness fallback.
- Network and userspace filesystems may not deliver native notifications consistently on any OS. The modification-age guard and fallback scan still help, while producer-side temporary-file plus atomic-rename is the strongest completion signal.
- Recursive symlink traversal may require additional permissions on Windows and behaves according to the host filesystem.
- Windows does not expose Unix `SO_REUSEPORT`; running JoeDLNA beside another SSDP server may therefore behave differently than on macOS/Linux. The supported deployment is one active DLNA server per host.
- Object IDs remain stable on one OS. Moving the same config and media tree between Unix and Windows produces different IDs because their native path encodings differ.
- Bookmark state defaults beside the config file, so the process needs write permission there if Samsung resume positions are desired.

## Installation status

Service definitions, container images, and prebuilt release artifacts are not yet supplied.
