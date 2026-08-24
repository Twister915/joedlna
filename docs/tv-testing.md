# First TV test

JoeDLNA has not yet completed a real-LAN interoperability test. Tests, documentation builds, and
`check-config` do not bind UDP 1900.

## Prepare

Build and validate the intended catalog without advertising:

```sh
cargo build --profile distribute -p joedlna-bin
target/distribute/joedlna check-config --config config.toml
```

Stop any existing DLNA server on the host through its normal service manager. Keep its
configuration and state intact so rollback is immediate.

## Test

Run JoeDLNA in the foreground:

```sh
RUST_LOG=joedlna_core=debug,joedlna=debug \
  target/distribute/joedlna serve --config config.toml
```

On a television or other renderer:

1. Find JoeDLNA and browse nested folders.
2. Play representative files for each configured media type.
3. Exercise seeking, pause/resume, and stop.
4. Add or remove a test share and verify the catalog reloads after the settle interval.
5. Copy a file into a share and verify it appears only after writes remain quiet for the settle
   interval.

Keep foreground logs for failed requests. SOAP actions, object IDs, range headers, and status codes
usually identify the affected compatibility path.

## Roll back

Press Control-C so JoeDLNA sends `ssdp:byebye`, then restore the previous DLNA server through its
service manager.
