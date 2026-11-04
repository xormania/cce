# Golden fixtures

Each directory is a complete store directory written by the workload in `golden_fixtures.rs`
(`write_golden_store`). The files are committed as written; tests copy them before opening.

- `format-4`: written by the format-4 engine (commit e8b24e6, the last revision writing format
  version 4). This engine must refuse it as an unsupported format and leave every file unchanged.
- `format-5`: written by this engine. It must open with exactly the workload's contents, and this
  engine must still write it byte for byte. Rewrite it only together with a format version change:
  `cargo test -p cce-storage --test golden_fixtures -- --ignored regenerate_the_format_5_fixture`.
