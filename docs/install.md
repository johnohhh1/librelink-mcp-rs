# Installing librelink-mcp-rs

There are three ways to install it:

- a prebuilt release archive;
- the Claude Desktop bundle;
- building from source with `cargo install`.

Whichever you use, credentials are entered only in a local terminal with `librelink-mcp-rs setup`, never through an MCP client or bundle settings.

## 1. Release archives

Each [GitHub Release](https://github.com/johnohhh1/librelink-mcp-rs/releases) has one archive per platform, each holding the binary, `README.md`, `LICENSE`, and `CHANGELOG.md`:

| Platform | Archive |
| --- | --- |
| Linux x86_64 | `librelink-mcp-rs-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux ARM64 | `librelink-mcp-rs-<version>-aarch64-unknown-linux-gnu.tar.gz` |
| macOS Apple silicon | `librelink-mcp-rs-<version>-aarch64-apple-darwin.tar.gz` |
| macOS Intel | `librelink-mcp-rs-<version>-x86_64-apple-darwin.tar.gz` |
| Windows x86_64 | `librelink-mcp-rs-<version>-x86_64-pc-windows-msvc.zip` |

### Verify the download

Check the download against `SHA256SUMS` from the same release before running it:

```sh
sha256sum --check --ignore-missing SHA256SUMS
```

On macOS use `shasum -a 256 --check --ignore-missing SHA256SUMS`. On Windows, compare the output of `Get-FileHash <file> -Algorithm SHA256` with the matching line.

### Install the binary

Unpack the archive and put `librelink-mcp-rs` (or `librelink-mcp-rs.exe`) somewhere on your `PATH`, for example `~/.local/bin` or `/usr/local/bin`.

## 2. Build from source

You need Rust 1.89 or newer. On Debian or Ubuntu, also install `libdbus-1-dev` and `pkg-config`.

```sh
cargo install --locked --git https://github.com/johnohhh1/librelink-mcp-rs --tag v1.0.0
```

That installs `librelink-mcp-rs` into `~/.cargo/bin`.

## Platform notes

- **Linux:** the binary uses the Secret Service (for example GNOME Keyring or KWallet) through D-Bus, so the runtime library `libdbus-1-3` must be installed. It is present on most desktop distributions; on minimal systems run `sudo apt-get install libdbus-1-3`. The keyring must be running and unlocked in your session. See [troubleshooting](troubleshooting.md#the-credential-store) for headless machines and services.
- **macOS:** credentials go to the login Keychain. The release binaries are not signed or notarized, so Gatekeeper may block the first run. After verifying the checksum, allow the binary in System Settings → Privacy & Security, or run `xattr -d com.apple.quarantine <path>`.
- **Windows:** credentials go to the Windows Credential Manager. The binaries are not signed, so SmartScreen may warn on first run.

## First run

```sh
librelink-mcp-rs setup            # email, hidden password, region
librelink-mcp-rs select-patient   # only if the account follows more than one person
librelink-mcp-rs check            # logs in and prints a small connectivity report
```

Use a LibreLinkUp **follower** account. `check` prints one reading in your own terminal; nothing is sent anywhere except LibreLinkUp.

## Connect an MCP client

### Claude Desktop: the bundle

macOS and Windows releases include a bundle: `librelink-mcp-rs-<version>-<target>.mcpb`.

1. Install the release archive for the same platform, and run `setup` (and `select-patient` if needed) from a terminal as above. The bundle has no settings, so it cannot take credentials. It reads the same OS credential store entries.
2. Open the `.mcpb` file with Claude Desktop and confirm the installation. The bundle runs `librelink-mcp-rs serve`.
3. Restart Claude Desktop after changing credentials or the patient selection.

### Claude Desktop: by hand, or any other MCP client

Add the server to the client's configuration. For Claude Desktop, the file is:

- `~/Library/Application Support/Claude/claude_desktop_config.json` on macOS;
- `%APPDATA%\Claude\claude_desktop_config.json` on Windows.

Use an absolute path to the binary:

```json
{
  "mcpServers": {
    "librelink": {
      "command": "/absolute/path/to/librelink-mcp-rs",
      "args": ["serve"]
    }
  }
}
```

To read a collector database at a non-default location, add `"--db", "/path/to/readings.sqlite3"` after `"serve"`.

## Collect longer history (optional)

The live API has about 12 hours of data. For more, run the collector; see the [collector guide](collector.md) for the details.

- **Linux:** run it as a systemd user service. Save the [example unit](collector.md#running-it-as-a-systemd-user-service) with `ExecStart` pointing at your binary, then run:

  ```sh
  systemctl --user daemon-reload
  systemctl --user enable --now librelink-collect.service
  ```

  The Secret Service must be reachable and unlocked from the service; see [troubleshooting](troubleshooting.md#the-credential-store).
- **macOS and Windows:** no service definition is shipped. Run `librelink-mcp-rs collect` in a terminal you keep open, or wrap it in a LaunchAgent or a Task Scheduler task. It stops cleanly on Ctrl+C, and exit status 78 means fix the cause before starting again.

To add older history from a LibreView export, see [Importing a LibreView CSV export](csv-import.md).

## Upgrading

Stop the collector, install the new version, then run `collect` or `import-csv` once; either one upgrades the database. `serve` never upgrades it. See [Upgrading](collector.md#upgrading). There is no downgrade path once the database is upgraded.

## Uninstalling

Run `librelink-mcp-rs clear-local-data` (add `--db <path>` for a non-default database) to remove the credentials, settings, patient selection, and local database. Then delete the binary and remove the bundle or client configuration. See [privacy](privacy.md#deleting-your-data).
