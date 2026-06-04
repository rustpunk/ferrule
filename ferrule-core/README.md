# ferrule-core

> Result formatters, output rendering, and credential-resolution glue for the [`ferrule`](https://github.com/rustpunk/ferrule) database CLI.

`ferrule-core` sits between the embeddable SQL driver core
([`ferrule-sql`](https://crates.io/crates/ferrule-sql)) and the `ferrule`
command-line interface. It adds the pieces that the driver core deliberately
leaves out:

- **Output formatters** — table (`tabled`), JSON / JSONL, CSV, YAML, Markdown,
  and HTML rendering over the neutral `Value`/`Row` types.
- **Resolver** — the one module that bridges `ferrule-config` credential
  resolution into a connect call (kept here so `ferrule-sql` stays config-free).
- **Schema / dump helpers** built on the driver core's introspection.

Backend support is feature-gated and forwards straight through to
`ferrule-sql`: enable `postgres`, `mysql`, `mssql`, `sqlite`, `oracle`, and/or
`ssh` as needed (the `default` set is empty).

Most embedders who only need to talk to databases should depend on
[`ferrule-sql`](https://crates.io/crates/ferrule-sql) directly; `ferrule-core`
is for callers that also want ferrule's rendering and credential plumbing.

## License

Licensed under either of MIT or Apache-2.0 (`SPDX: MIT OR Apache-2.0`) at your
option.
