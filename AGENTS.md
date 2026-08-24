# Contributor guidance

- Follow [Joey's Rust style guide](docs/rust-style.md).
- Keep the filesystem authoritative. Any future database is a disposable cache outside media shares.
- Only emit `DLNA.ORG_PN` after validating the required media properties.
- Tests and `check-config` must never bind SSDP or advertise on the LAN.
- Do not run `joedlna serve` without approval when another production DLNA server is active.
