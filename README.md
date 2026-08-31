# stand-render

The Les stand's shared template + static-asset rendering (CODESTYLE.md §9):
minijinja plus a cache over a validated asset directory. Depended on only by
services that actually serve assets — deliberately separate from `stand-log`
(§9.7), which stays dependency-light for every binary.

- **Version:** `0.1.0` · **Toolchain:** Rust 1.98.0.

## Depend on it

```toml
[dependencies]
# NOTE: canonical remote is https://github.com/rebenkoy/stand-render — switch to
# a git dep once it is pushed. Path dep until then.
stand-render = { path = "../stand-render" }
```

## Use

```rust
use stand_render::{Builder, sha256};

// boot: validate the dir + required templates, pin server-enforced logic
let cache = Builder::new(&assets_dir)
    .require_template("stand-oidc.js.jinja")           // §9.6 present + parses
    .pin("stand-oidc.js.jinja", EXPECTED_SHA256)       // §9.8 no drift
    .build()?;                                          // refuses to boot otherwise

// §9.5 template mode — keyed by (mtime, params)
let js = cache.render("stand-oidc.js.jinja", &[("login_path", "\"/oidc/login\"")])?;

// §9.5a static mode — keyed by (mtime) alone
let logo = cache.static_file("logo.svg")?;
```

Both modes invalidate on the next request after their key changes, so editing
a served file on disk takes effect without a restart. Wrap the `AssetCache` in
an `Arc` in your `AppState`.

## Modes and rules

| method | key | for |
|---|---|---|
| `render(name, params)` | (mtime, params) | minijinja templates with config baked in |
| `static_file(name)` | (mtime) | completely static served files |

- **Boot validation (§9.6):** the asset dir must exist and every required
  template must be present and parse — `Builder::build` refuses otherwise. The
  asset dir is a classified config option in the host service.
- **Autoescape (§9.3):** on for `.html`/`.htm`, off for JS/text.
- **Integrity pins (§9.7b / §9.8):** `pin(name, sha256)` verifies a file's
  content hash at boot AND on every reload, refusing to serve on mismatch —
  for logic the server also enforces (dual-use assets) and for library
  templates that must not drift from the crate version. Produce the constant
  with `stand_render::sha256(bytes)`.

## Untrusted names (§9.5b)

Asset names are treated as untrusted input — an adopter that serves a bundle by
URL path passes a request-influenced name straight in. All three name-taking
methods reject absolute paths, `..` segments, and symlinks escaping the root
(canonicalize + containment) before any filesystem access, returning
`RenderError::UnsafeName`. The guard lives in the crate so no adopter has to
remember it.

## Scope (§9.7a)

Served content only. Embedded data never sent to a client (seed data,
fixtures, golden anchors) is out of scope and may stay embedded.

## Test

`cargo test` — substitution + both cache modes + edit-without-restart + boot
refusals (bad dir, missing/unparseable template) + pin match/mismatch/drift +
HTML-escape-vs-JS-raw. No network.
