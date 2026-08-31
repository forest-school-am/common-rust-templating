# AGENTS.md — stand-render

## Purpose
The stand's shared template + static-asset rendering (CODESTYLE.md §9.7):
minijinja setup + an mtime/params-keyed `AssetCache` over a validated asset
directory. Used only by services that serve assets — kept SEPARATE from
`stand-log` so binaries that serve nothing don't pull minijinja.

## Layout
- `src/lib.rs` — `Builder` (boot validation + pins), `AssetCache`
  (`render` / `static_file`), `RenderError`, `sha256`, and the test suite.

## Invariants
- Two cache modes, both invalidate on the next request after their key
  changes: `render` on (mtime, params), `static_file` on (mtime). Editing a
  served file on disk MUST take effect without a restart.
- Boot validation (§9.6): bad dir / missing / unparseable required template
  refuses to boot — never a render-time surprise.
- Integrity pins (§9.7b/§9.8): a pinned file's sha256 is verified at boot AND
  on every reload; mismatch refuses to serve. Pins are for server-enforced
  logic and library templates that must not drift.
- Autoescape (§9.3): HTML on, JS/text off — by extension.
- Scope is SERVED content only (§9.7a); embedded non-served data is fine
  elsewhere.

## Run / test
`nix develop --impure -c cargo test` (frozen 1.98.0;
`CARGO_TARGET_DIR=/home/dev/.cache/stand-render-target`). Library only.

## Stand context
Implements DECISIONS.md R6 / CODESTYLE.md §9.7–§9.8. First consumers:
stand-oidc (its served shim → a pinned library template, §9.8) and mint
(searchbase.js). Builds serialized under R4's disk regime.
