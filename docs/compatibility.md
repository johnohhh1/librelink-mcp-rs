# Compatibility

librelink-mcp-rs uses LibreLinkUp's unofficial v4 follower API, which Abbott can change at any time. Version 1.0 is validated on exactly one configuration. Everything else is **unvalidated**: it may work, but nobody has checked it yet.

## Validated configuration

Checked manually on 2026-11-05 and 2026-11-06, with no readings or account details recorded:

| | |
| --- | --- |
| Account | One LibreLinkUp follower account in the US, following one person |
| Sensor | FreeStyle Libre 3 |
| Sharer's time zone | America/Detroit |
| Host | Linux, with the credentials in the Secret Service |
| Checked | login, patient selection, all six data tools on the live and store paths, factory-UTC inference, the collector (polling, deduplication, file permissions, clean shutdown), `serve` making no network connections while a collector runs, LibreView CSV import (dry run, import, re-import, schema migration), and 14-day statistics within 1 point of the vendor's AGP report on every consensus band |

## Unvalidated

- **Regions other than the US:** EU, EU2, DE, FR, CA, AU, AP, AE, JP, LA, RU, CN, and the global endpoint. Region redirects are tested only with synthetic responses.
- **Account shapes:** accounts following several people (selection is tested only with synthetic responses), and accounts in other countries.
- **Sensors:** FreeStyle Libre 1, 2, and 2 Plus, and other readers. Their graph cadence and record types may differ.
- **Other sharer time zones:** especially ones with an offset that is not a whole hour, or none at all. Factory-UTC inference accepts quarter-hour offsets within ±14 hours.
- **LibreView exports:** exports in other languages (refused today), date orders, or units than the one checked. mmol/L and day-first dates are tested only with synthetic files.
- **Hosts:** macOS and Windows hosts, and the Claude Desktop bundle (`.mcpb`). They are built and unit-tested in CI only.
- **MCP clients** other than Claude Desktop.

## Validating another configuration

You can help by checking a configuration that isn't listed. Please **never** include readings, names, emails, patient or account IDs, serial numbers, tokens, or raw API responses in a report.

1. Install a release ([install guide](install.md)) and run `setup`, then `select-patient` if needed.
2. Run `librelink-mcp-rs check`. Note only:
   - whether it succeeded, or its `error_code`;
   - `time_basis`;
   - `freshness`;
   - `display_minus_factory_minutes`;
   - `graph_points_returned`.
3. In your MCP client, call each data tool once. For each, note whether it returned an error (and its `error_code`), `data_source`, and for history `coverage.estimated_cadence_minutes`.
4. Optional: run `collect` for an hour, then call `get_glucose_history` and note `data_source`, `coverage.interior_gap_count`, and `period_status`.
5. Optional: run `import-csv <export> --timezone <zone> --dry-run` and note the counts it prints.
6. Open an issue titled "Compatibility: <region> / <sensor>". Include:
   - the region;
   - the sensor model;
   - the sharer's time zone, or only its UTC offset;
   - the host OS;
   - the MCP client;
   - the librelink-mcp-rs version;
   - the notes from the steps above.
