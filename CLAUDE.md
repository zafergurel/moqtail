# Claude Code Guidelines

## Monorepo structure

- `libs/moqtail-rs` — Rust protocol library (`moqtail` crate)
- `libs/moqtail-ts` — TypeScript protocol library (`moqtail-ts` package)
- `apps/relay` — Rust relay server
- `apps/client` — Rust client app
- `apps/client-js` — Browser subscriber (TypeScript/Vite)
- `apps/meet` — WebRTC-over-MoQ conferencing demo (TypeScript/Vite)

## Constructor style

- **Rust**: Always provide a `pub fn new()` static constructor. Do not use struct literal construction at call sites when `new()` exists.
- **TypeScript**: Prefer the direct constructor `new Foo(...)`. Do not add static factory methods like `Foo.create()` or `Foo.new()`.

## TypeScript imports

- Always use static imports at the top of the file. Never use dynamic imports (e.g. `await import(...)`). Vite's test suite prevents the project from building if dynamic imports are used in test code.

## Parameter extraction from Vec<MessageParameter>

- Use `MessageParameterVecExt` trait methods (`get_param(MessageParameterType::Foo)`, `set_param(...)`) instead of manually iterating with `find_map` or filtering and pushing.
- Import: `use moqtail::model::parameter::message_parameter::MessageParameterVecExt;`
- Use `MessageParameterType::Foo` (from `moqtail::model::parameter::constant`) to identify parameter types — do not use raw type values or construct a dummy instance just to call `.type_value()`.

## Wire-format field removal pattern

When removing a fixed wire-format field from a control message and replacing it with a `MessageParameter`:

1. Remove the field from the struct, `serialize()`, and `parse_payload()` in both Rust and TypeScript.
2. Add the parameter type to `is_valid_for()` in `message_parameter.rs` if it wasn't already valid for that message type.
3. Update all factory method call sites to pass the value as a `MessageParameter` entry in the params vec.
4. Update `MessageParameter::deserialize` to handle any newly needed enum variants (e.g. `GroupOrder::Original` = 0 was missing before).
5. Run `cargo test -p moqtail && cargo build --bin relay` and `npm run test && npm run build` after each message type change.

## How and when to test

### When to test

- After completing changes to any Rust library code (`libs/moqtail-rs`): run Rust tests and build.
- After completing changes to any TypeScript library code (`libs/moqtail-ts`): run TS tests and build.
- After completing changes to relay or client app code: run the relay build.
- Do not batch multiple message type changes before testing — test after each one.

### Rust

```
cargo fmt --all -- --check     # formatting (CI enforces this)
cargo clippy --all-targets --all-features -- -D warnings  # linting (warnings = errors in CI)
cargo test -p moqtail          # library unit tests
cargo build --bin relay        # verify relay compiles
```

### TypeScript (run from `libs/moqtail-ts`)

```
npm run test     # vitest inline tests
npm run build    # tsc typecheck + tsup + api-extractor
```

## Terminology

- Use **"switching delay"** and **`switch_delay_ms`** — never "switching latency" or `switch_latency_ms`.

## Changesets

This repo uses [changesets](https://github.com/changesets/changesets) for versioning. When making changes to any published package (`moqtail-rs`, `moqtail-ts`, `relay`, `client`), add a changeset:

```
npm run changeset   # run from repo root; prompts for affected packages and bump type
```

The pre-commit hook will also prompt you interactively. Changeset files (`.changeset/*.md`) must be committed alongside the code change — CI and the release workflow depend on them.
