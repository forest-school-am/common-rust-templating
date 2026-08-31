//! # stand-render — shared template + static-asset rendering (CODESTYLE.md §9)
//!
//! The stand's one substitution engine (minijinja) plus a cache over a
//! validated asset directory. Deliberately SEPARATE from `stand-log` (§9.7):
//! logging is dependency-light and used everywhere; rendering pulls minijinja
//! and is used only by services that actually serve assets.
//!
//! Two cache modes, both invalidating on the next request after their key
//! changes (no stale serve after an edit — §9.5/§9.5a):
//! - [`AssetCache::render`] — a minijinja template, keyed by (file mtime,
//!   parameters).
//! - [`AssetCache::static_file`] — a static file, keyed by (file mtime) alone.
//!
//! Boot validation (§9.6): the directory must exist, and every required
//! template must be present and parse — checked in [`Builder::build`], which
//! refuses otherwise.
//!
//! Integrity pins (§9.7b, §9.8): a file may be pinned to an expected sha256,
//! checked at boot AND on every cache reload, refusing to serve on mismatch —
//! for logic the server enforces (dual-use assets) and for library-shipped
//! templates that must not drift from the crate version.
//!
//! ```ignore
//! let cache = stand_render::Builder::new(assets_dir)
//!     .require_template("stand-oidc.js.jinja")
//!     .pin("stand-oidc.js.jinja", STAND_OIDC_JS_SHA256) // §9.8 no-skew
//!     .build()?;                                          // §9.6 boot refusal
//! let js = cache.render("stand-oidc.js.jinja", &[("login_path", "/oidc/login")])?;
//! ```

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::SystemTime;

use minijinja::{AutoEscape, Environment};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum RenderError {
    #[error("asset dir does not exist or is not a directory: {0}")]
    BadDir(PathBuf),
    #[error("required template not found: {0}")]
    Missing(String),
    #[error("template {0} failed to parse: {1}")]
    Parse(String, String),
    #[error("integrity pin failed for {0}: on-disk content does not match the expected hash")]
    PinMismatch(String),
    #[error("io error on {0}: {1}")]
    Io(String, String),
    #[error("render of {0} failed: {1}")]
    Render(String, String),
    /// The asset name is not a safe relative path under the asset root — an
    /// absolute path, a `..` segment, or a symlink escaping the root (§9.5b).
    /// Asset names from requests are untrusted; this rejection lives here so no
    /// adopter can forget it.
    #[error("unsafe asset name (path traversal / escape rejected): {0}")]
    UnsafeName(String),
}

enum Entry {
    Template { mtime: SystemTime, key: u64, out: Arc<str> },
    Static { mtime: SystemTime, bytes: Arc<[u8]> },
}

/// §9.3 autoescape policy by extension: HTML contexts on, JS/text off.
fn autoescape(name: &str) -> AutoEscape {
    if name.ends_with(".html") || name.ends_with(".htm") {
        AutoEscape::Html
    } else {
        AutoEscape::None
    }
}

/// A long-lived minijinja `Environment` for `render_ctx` (loader-backed, so
/// `{% extends %}`/`{% include %}` resolve) plus the mtimes of the templates it
/// has loaded. The steady state re-parses NOTHING; an edit to the entry OR any
/// loaded parent is detected by stat-vs-recorded-mtime and drops the whole
/// compiled set via `clear_templates()`, so §9.2a edit-without-restart holds
/// for partials too (which the old per-call `Environment::new()` achieved only
/// by brute-force re-parsing every call).
struct CtxEnv {
    env: Environment<'static>,
    mtimes: HashMap<String, SystemTime>,
    /// Slow-path (load/reload) count — steady-state renders must NOT bump it.
    /// Observability, and the anchor the no-re-parse test asserts against.
    loads: u64,
}

/// Rendering cache over a validated asset directory. Cheap to clone-share
/// (wrap in `Arc` in your `AppState`).
pub struct AssetCache {
    root: PathBuf,
    /// The asset root with symlinks resolved — the containment boundary every
    /// resolved asset path must stay under (§9.5b).
    canonical_root: PathBuf,
    env: Environment<'static>,
    /// Long-lived loader env for `render_ctx`, with mtime-driven invalidation.
    /// Write-locked only on (re)load, so steady-state renders don't serialize.
    ctx_env: RwLock<CtxEnv>,
    pins: HashMap<String, [u8; 32]>,
    entries: RwLock<HashMap<String, Entry>>,
}

/// Builds an [`AssetCache`], performing all boot validation (§9.6) up front so
/// a bad asset dir / missing template / failed pin refuses to boot rather than
/// surfacing at render time.
pub struct Builder {
    root: PathBuf,
    required: Vec<String>,
    pins: HashMap<String, [u8; 32]>,
}

impl Builder {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into(), required: Vec::new(), pins: HashMap::new() }
    }

    /// Require a template to exist and parse at boot (§9.6).
    pub fn require_template(mut self, name: impl Into<String>) -> Self {
        self.required.push(name.into());
        self
    }

    /// Pin a file's sha256, verified at boot and on every reload (§9.7b/§9.8).
    /// A pinned file is also implicitly required.
    pub fn pin(mut self, name: impl Into<String>, expected_sha256: [u8; 32]) -> Self {
        let name = name.into();
        self.pins.insert(name.clone(), expected_sha256);
        self.required.push(name);
        self
    }

    pub fn build(self) -> Result<AssetCache, RenderError> {
        if !self.root.is_dir() {
            return Err(RenderError::BadDir(self.root));
        }
        // Resolve the root once — the containment boundary for §9.5b.
        let canonical_root = self
            .root
            .canonicalize()
            .map_err(|e| RenderError::Io(self.root.display().to_string(), e.to_string()))?;
        // `env` serves render()'s ad-hoc `template_from_named_str` (no loader);
        // its own entries cache handles invalidation.
        let mut env = Environment::new();
        env.set_auto_escape_callback(autoescape);

        // `ctx_env` is the long-lived loader env for render_ctx (§9.4). Loader
        // + autoescape installed ONCE here, not per call.
        let mut ctx_env = Environment::new();
        ctx_env.set_auto_escape_callback(autoescape);
        ctx_env.set_loader(minijinja::path_loader(&canonical_root));

        let cache = AssetCache {
            root: self.root,
            canonical_root,
            env,
            ctx_env: RwLock::new(CtxEnv { env: ctx_env, mtimes: HashMap::new(), loads: 0 }),
            pins: self.pins,
            entries: RwLock::new(HashMap::new()),
        };

        // pins first (also reads the bytes), then parse-check required templates
        for (name, expected) in &cache.pins {
            let path = cache.safe_path(name)?;
            cache.read_verified(&path, name, Some(expected))?;
        }
        for name in &self.required {
            let path = cache.safe_path(name)?;
            let src = String::from_utf8(cache.read_verified(&path, name, cache.pins.get(name))?)
                .map_err(|e| RenderError::Parse(name.clone(), e.to_string()))?;
            cache
                .env
                .template_from_named_str(name, &src)
                .map_err(|e| RenderError::Parse(name.clone(), e.to_string()))?;
        }
        Ok(cache)
    }
}

impl AssetCache {
    /// Resolve an untrusted asset `name` to a filesystem path that is proven to
    /// stay under the asset root (§9.5b). Rejects absolute paths and any
    /// non-`Normal`/`CurDir` component (`..`, root, prefix) BEFORE touching the
    /// filesystem, then canonicalizes and requires containment under
    /// `canonical_root` — which catches a symlink INSIDE the root pointing out
    /// (the component check alone would not). `Ok` therefore always means
    /// "safe AND real": a name that does not resolve to an existing file
    /// returns `Missing` here rather than an unvalidated path, so no caller
    /// ever touches a path safe_path hasn't cleared. This also closes the
    /// intermediate-symlink gap by construction — a `link/newfile` where
    /// `link` escapes the root but `newfile` is absent is `Missing`, never a
    /// path a later read would follow out. (`canonicalize` is realpath: a
    /// permissions error surfaces as `Io`, so `NotFound` genuinely means
    /// absent.)
    fn safe_path(&self, name: &str) -> Result<PathBuf, RenderError> {
        let rel = Path::new(name);
        for component in rel.components() {
            match component {
                std::path::Component::Normal(_) | std::path::Component::CurDir => {}
                _ => return Err(RenderError::UnsafeName(name.to_owned())),
            }
        }
        match self.root.join(rel).canonicalize() {
            Ok(real) if real.starts_with(&self.canonical_root) => Ok(real),
            Ok(_) => Err(RenderError::UnsafeName(name.to_owned())), // symlink escaped the root
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(RenderError::Missing(name.to_owned()))
            }
            Err(e) => Err(RenderError::Io(name.to_owned(), e.to_string())),
        }
    }

    fn mtime(&self, path: &Path, name: &str) -> Result<SystemTime, RenderError> {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    RenderError::Missing(name.to_owned())
                } else {
                    RenderError::Io(name.to_owned(), e.to_string())
                }
            })
    }

    /// Read a pre-validated (`safe_path`) file's bytes, verifying its pin if one
    /// is expected (§9.7b/§9.8). Takes the resolved path so it can never be
    /// handed a raw untrusted name.
    fn read_verified(
        &self,
        path: &Path,
        name: &str,
        expected: Option<&[u8; 32]>,
    ) -> Result<Vec<u8>, RenderError> {
        let bytes = std::fs::read(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                RenderError::Missing(name.to_owned())
            } else {
                RenderError::Io(name.to_owned(), e.to_string())
            }
        })?;
        if let Some(expected) = expected {
            let got: [u8; 32] = Sha256::digest(&bytes).into();
            if &got != expected {
                return Err(RenderError::PinMismatch(name.to_owned()));
            }
        }
        Ok(bytes)
    }

    /// §9.5 template mode: render `name` with `params`, cached by (mtime,
    /// params). Re-renders on the next call after either changes.
    pub fn render(&self, name: &str, params: &[(&str, &str)]) -> Result<Arc<str>, RenderError> {
        let path = self.safe_path(name)?; // §9.5b: reject traversal before any FS/cache touch
        let mtime = self.mtime(&path, name)?;
        let key = params_key(params);

        if let Ok(entries) = self.entries.read() {
            if let Some(Entry::Template { mtime: m, key: k, out }) = entries.get(name) {
                if *m == mtime && *k == key {
                    return Ok(out.clone());
                }
            }
        }

        let src = String::from_utf8(self.read_verified(&path, name, self.pins.get(name))?)
            .map_err(|e| RenderError::Render(name.to_owned(), e.to_string()))?;
        let ctx: BTreeMap<&str, &str> = params.iter().copied().collect();
        let tmpl = self
            .env
            .template_from_named_str(name, &src)
            .map_err(|e| RenderError::Parse(name.to_owned(), e.to_string()))?;
        let out: Arc<str> =
            Arc::from(tmpl.render(ctx).map_err(|e| RenderError::Render(name.to_owned(), e.to_string()))?);

        if let Ok(mut entries) = self.entries.write() {
            entries.insert(name.to_owned(), Entry::Template { mtime, key, out: out.clone() });
        }
        Ok(out)
    }

    /// §9.4 data-driven mode: render `name` with a full serializable context,
    /// UNCACHED by definition — data-driven pages change per request, so a
    /// cache keyed on the context would only ever miss (see the cron-viewer
    /// exemption that motivated this entry point). Guarantees preserved from
    /// the cached paths:
    ///
    /// - the entry template's integrity pin is verified on EVERY call via
    ///   `read_verified` — an uncached path must not become a pin-bypass path;
    /// - the template is re-read from disk each call (a fresh per-call
    ///   `Environment`, so minijinja's internal parse cache cannot serve a
    ///   stale template) — §9.2a's edit-without-restart holds here too;
    /// - the same autoescape policy applies (§9.3).
    ///
    /// `{% extends %}`/`{% include %}` resolve through a path loader rooted at
    /// the validated asset dir. Note: pins are verified for the ENTRY template;
    /// a pinned file pulled in only via extends/include is verified when it is
    /// itself rendered or served, not transitively.
    pub fn render_ctx<S: serde::Serialize>(
        &self,
        name: &str,
        ctx: &S,
    ) -> Result<String, RenderError> {
        // §9.5b: reject traversal, then pin + existence check on the safe path.
        let path = self.safe_path(name)?;
        let _ = self.read_verified(&path, name, self.pins.get(name))?;

        // Fast path (steady state): entry already loaded and no loaded template
        // (entry OR parent) changed on disk — a READ lock, no re-parse, no
        // serialization against concurrent renders.
        {
            let g = self.ctx_env.read().unwrap_or_else(|e| e.into_inner());
            if g.mtimes.contains_key(name) && !self.loaded_stale(&g) {
                return self.ctx_render(&g.env, name, ctx);
            }
        }

        // Slow path: (re)load under the WRITE lock. If any loaded template
        // changed, drop the whole compiled set (clear_templates is
        // all-or-nothing — fine at this scale) so the edit is picked up (§9.2a
        // for partials); then render (loading the entry + its extends/include
        // parents via the loader) and record every loaded template's mtime.
        let mut g = self.ctx_env.write().unwrap_or_else(|e| e.into_inner());
        g.loads += 1;
        if self.loaded_stale(&g) {
            g.env.clear_templates();
            g.mtimes.clear();
        }
        let out = self.ctx_render(&g.env, name, ctx)?;
        let loaded: Vec<String> = g.env.templates().map(|(n, _)| n.to_owned()).collect();
        for tname in loaded {
            if let Ok(mt) = self.canonical_root.join(&tname).metadata().and_then(|m| m.modified()) {
                g.mtimes.insert(tname, mt);
            }
        }
        Ok(out)
    }

    /// get_template (loader-backed) + render, mapping minijinja errors.
    fn ctx_render<S: serde::Serialize>(
        &self,
        env: &Environment<'static>,
        name: &str,
        ctx: &S,
    ) -> Result<String, RenderError> {
        let tmpl = env.get_template(name).map_err(|e| {
            if e.kind() == minijinja::ErrorKind::TemplateNotFound {
                RenderError::Missing(name.to_owned())
            } else {
                RenderError::Parse(name.to_owned(), e.to_string())
            }
        })?;
        tmpl.render(minijinja::value::Value::from_serialize(ctx))
            .map_err(|e| RenderError::Render(name.to_owned(), e.to_string()))
    }

    /// Has any template the ctx env has loaded changed (or vanished) on disk
    /// since it was recorded? Checks the entry AND every extends/include parent.
    fn loaded_stale(&self, g: &CtxEnv) -> bool {
        g.mtimes.iter().any(|(tname, recorded)| {
            match self.canonical_root.join(tname).metadata().and_then(|m| m.modified()) {
                Ok(now) => now != *recorded,
                Err(_) => true,
            }
        })
    }

    /// §9.5a static mode: serve `name`'s bytes, cached by (mtime) alone.
    /// Re-reads on the next call after the file's mtime changes.
    pub fn static_file(&self, name: &str) -> Result<Arc<[u8]>, RenderError> {
        let path = self.safe_path(name)?; // §9.5b: reject traversal before any FS/cache touch
        let mtime = self.mtime(&path, name)?;

        if let Ok(entries) = self.entries.read() {
            if let Some(Entry::Static { mtime: m, bytes }) = entries.get(name) {
                if *m == mtime {
                    return Ok(bytes.clone());
                }
            }
        }

        let bytes: Arc<[u8]> = Arc::from(self.read_verified(&path, name, self.pins.get(name))?);
        if let Ok(mut entries) = self.entries.write() {
            entries.insert(name.to_owned(), Entry::Static { mtime, bytes: bytes.clone() });
        }
        Ok(bytes)
    }
}

/// Compute the sha256 of some bytes — for producing the constant a caller
/// pins against (`Builder::pin`).
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

fn params_key(params: &[(&str, &str)]) -> u64 {
    // deterministic regardless of caller order
    let sorted: BTreeMap<&str, &str> = params.iter().copied().collect();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (k, v) in sorted {
        k.hash(&mut h);
        v.hash(&mut h);
    }
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("stand-render-{}", uniq()));
        fs::create_dir_all(&d).unwrap();
        d
    }
    fn uniq() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: AtomicU64 = AtomicU64::new(0);
        let n = C.fetch_add(1, Ordering::Relaxed);
        SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() as u64 ^ n
    }
    fn write(dir: &Path, name: &str, body: &str) {
        fs::write(dir.join(name), body).unwrap();
    }
    fn bump_mtime(dir: &Path, name: &str) {
        let t = filetime::FileTime::from_unix_time(2_000_000_000, 0);
        filetime::set_file_mtime(dir.join(name), t).unwrap();
    }

    #[test]
    fn render_substitutes_and_caches_by_params_and_mtime() {
        let d = tmpdir();
        write(&d, "shim.js.jinja", "const P = {{ login_path }};");
        let c = Builder::new(&d).require_template("shim.js.jinja").build().unwrap();

        let a = c.render("shim.js.jinja", &[("login_path", "\"/oidc/login\"")]).unwrap();
        assert_eq!(&*a, "const P = \"/oidc/login\";");
        // same params + mtime -> cache hit (same Arc)
        let b = c.render("shim.js.jinja", &[("login_path", "\"/oidc/login\"")]).unwrap();
        assert!(Arc::ptr_eq(&a, &b), "same params+mtime must be a cache hit");
        // different params -> re-render
        let e = c.render("shim.js.jinja", &[("login_path", "\"/x\"")]).unwrap();
        assert_eq!(&*e, "const P = \"/x\";");
        assert!(!Arc::ptr_eq(&a, &e));
    }

    #[test]
    fn edit_takes_effect_without_restart_via_mtime() {
        let d = tmpdir();
        write(&d, "a.txt.jinja", "one {{ x }}");
        let c = Builder::new(&d).build().unwrap();
        let first = c.render("a.txt.jinja", &[("x", "!")]).unwrap();
        assert_eq!(&*first, "one !");
        // edit the file AND move its mtime forward (same-second writes wouldn't
        // change mtime on coarse clocks)
        std::thread::sleep(Duration::from_millis(5));
        write(&d, "a.txt.jinja", "two {{ x }}");
        bump_mtime(&d, "a.txt.jinja");
        let second = c.render("a.txt.jinja", &[("x", "!")]).unwrap();
        assert_eq!(&*second, "two !", "edit must take effect without restart");
    }

    #[test]
    fn static_file_caches_by_mtime() {
        let d = tmpdir();
        write(&d, "logo.svg", "<svg/>");
        let c = Builder::new(&d).build().unwrap();
        let a = c.static_file("logo.svg").unwrap();
        assert_eq!(&*a, b"<svg/>");
        let b = c.static_file("logo.svg").unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn boot_refuses_bad_dir_and_missing_template() {
        let missing = std::env::temp_dir().join(format!("nope-{}", uniq()));
        assert!(matches!(Builder::new(&missing).build(), Err(RenderError::BadDir(_))));

        let d = tmpdir();
        assert!(matches!(
            Builder::new(&d).require_template("absent.jinja").build(),
            Err(RenderError::Missing(_))
        ));
    }

    #[test]
    fn boot_refuses_unparseable_template() {
        let d = tmpdir();
        write(&d, "bad.jinja", "{{ unclosed ");
        assert!(matches!(
            Builder::new(&d).require_template("bad.jinja").build(),
            Err(RenderError::Parse(..))
        ));
    }

    #[test]
    fn pin_matches_at_boot_and_mismatches_on_drift() {
        let d = tmpdir();
        write(&d, "logic.js", "authored();");
        let good = sha256(b"authored();");
        // correct pin builds
        let c = Builder::new(&d).pin("logic.js", good).build().unwrap();
        assert_eq!(&*c.static_file("logic.js").unwrap(), b"authored();");

        // wrong pin refuses to boot
        let wrong = sha256(b"different");
        assert!(matches!(
            Builder::new(&d).pin("logic.js", wrong).build(),
            Err(RenderError::PinMismatch(_))
        ));

        // drift after boot -> refuses to serve on reload
        std::thread::sleep(Duration::from_millis(5));
        write(&d, "logic.js", "tampered();");
        bump_mtime(&d, "logic.js");
        assert!(matches!(c.static_file("logic.js"), Err(RenderError::PinMismatch(_))));
    }

    #[test]
    fn render_ctx_takes_structured_context_and_iterates() {
        let d = tmpdir();
        write(&d, "list.html", "{% for t in tasks %}<li>{{ t.name }}</li>{% endfor %}");
        let c = Builder::new(&d).require_template("list.html").build().unwrap();
        #[derive(serde::Serialize)]
        struct Ctx {
            tasks: Vec<Row>,
        }
        #[derive(serde::Serialize)]
        struct Row {
            name: String,
        }
        let out = c
            .render_ctx(
                "list.html",
                &Ctx { tasks: vec![Row { name: "a".into() }, Row { name: "<b>".into() }] },
            )
            .unwrap();
        assert_eq!(out, "<li>a</li><li>&lt;b&gt;</li>", "iteration + autoescape");
    }

    #[test]
    fn render_ctx_supports_extends_and_edits_without_restart() {
        let d = tmpdir();
        write(&d, "base.html", "[{% block body %}{% endblock %}]");
        write(&d, "page.html", "{% extends \"base.html\" %}{% block body %}{{ n }}{% endblock %}");
        let c = Builder::new(&d).require_template("page.html").build().unwrap();
        #[derive(serde::Serialize)]
        struct Ctx {
            n: u32,
        }
        assert_eq!(c.render_ctx("page.html", &Ctx { n: 1 }).unwrap(), "[1]");
        // edit the base template: must take effect on the very next render
        std::thread::sleep(Duration::from_millis(5));
        write(&d, "base.html", "({% block body %}{% endblock %})");
        bump_mtime(&d, "base.html");
        assert_eq!(
            c.render_ctx("page.html", &Ctx { n: 2 }).unwrap(),
            "(2)",
            "template edits must take effect without restart on the uncached path"
        );
    }

    #[test]
    fn render_ctx_verifies_pins_and_reports_missing() {
        let d = tmpdir();
        write(&d, "pinned.html", "ok {{ x }}");
        let good = sha256(b"ok {{ x }}");
        let c = Builder::new(&d).pin("pinned.html", good).build().unwrap();
        #[derive(serde::Serialize)]
        struct Ctx {
            x: u32,
        }
        assert_eq!(c.render_ctx("pinned.html", &Ctx { x: 7 }).unwrap(), "ok 7");
        // drift -> the uncached path must also refuse (no pin bypass)
        std::thread::sleep(Duration::from_millis(5));
        write(&d, "pinned.html", "tampered {{ x }}");
        bump_mtime(&d, "pinned.html");
        assert!(matches!(
            c.render_ctx("pinned.html", &Ctx { x: 7 }),
            Err(RenderError::PinMismatch(_))
        ));
        assert!(matches!(
            c.render_ctx("absent.html", &Ctx { x: 7 }),
            Err(RenderError::Missing(_))
        ));
    }

    #[test]
    fn html_autoescapes_but_js_does_not() {
        let d = tmpdir();
        write(&d, "p.html", "<b>{{ v }}</b>");
        write(&d, "p.js.jinja", "x = {{ v }}");
        let c = Builder::new(&d).build().unwrap();
        assert_eq!(&*c.render("p.html", &[("v", "<x>")]).unwrap(), "<b>&lt;x&gt;</b>");
        assert_eq!(&*c.render("p.js.jinja", &[("v", "<x>")]).unwrap(), "x = <x>");
    }

    // §9.5b negative-space (§7.1): untrusted names must not escape the root.
    #[test]
    fn traversal_names_rejected_on_every_entry_point() {
        let d = tmpdir();
        write(&d, "ok.txt", "ok");
        write(&d, "t.js.jinja", "x = {{ v }}");
        let c = Builder::new(&d).build().unwrap();
        // sanity: a legitimate name still works
        assert_eq!(&*c.static_file("ok.txt").unwrap(), b"ok");

        for bad in ["../../etc/passwd", "../secret", "/etc/passwd", "a/../../b", "./../x"] {
            assert!(
                matches!(c.static_file(bad), Err(RenderError::UnsafeName(_))),
                "static_file({bad:?}) not rejected"
            );
            assert!(
                matches!(c.render(bad, &[]), Err(RenderError::UnsafeName(_))),
                "render({bad:?}) not rejected"
            );
            assert!(
                matches!(c.render_ctx(bad, &()), Err(RenderError::UnsafeName(_))),
                "render_ctx({bad:?}) not rejected"
            );
        }
    }

    #[test]
    fn symlink_escaping_root_is_rejected() {
        // root/ holds inside.txt; its PARENT holds outside.txt; a symlink inside
        // the root points at the parent — reading through it must be rejected,
        // not served (the `..`-component check alone wouldn't catch this).
        let parent = tmpdir();
        fs::write(parent.join("outside.txt"), "SECRET").unwrap();
        let root = parent.join("assets");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("inside.txt"), "ok").unwrap();
        std::os::unix::fs::symlink(&parent, root.join("up")).unwrap();

        let c = Builder::new(&root).build().unwrap();
        assert_eq!(&*c.static_file("inside.txt").unwrap(), b"ok");
        // "up" resolves (via symlink) to the parent, escaping the root
        assert!(matches!(
            c.static_file("up/outside.txt"),
            Err(RenderError::UnsafeName(_))
        ));
        // intermediate symlink escapes but the final target is absent: Missing,
        // NOT an unvalidated path a later read would follow out (the NotFound-arm
        // hardening — §9.5b).
        assert!(matches!(
            c.static_file("up/does-not-exist.txt"),
            Err(RenderError::Missing(_))
        ));
    }

    #[test]
    fn nonexistent_name_is_missing_not_ok_or_unsafe() {
        let d = tmpdir();
        write(&d, "real.txt", "x");
        let c = Builder::new(&d).build().unwrap();
        // a structurally-safe name that simply does not exist resolves to
        // Missing at safe_path — never Ok(unvalidated path), never UnsafeName.
        assert!(matches!(c.static_file("absent.txt"), Err(RenderError::Missing(_))));
        assert!(matches!(c.render("absent.js.jinja", &[]), Err(RenderError::Missing(_))));
    }

    #[test]
    fn render_ctx_steady_state_no_reparse_but_partial_edit_invalidates() {
        use std::collections::BTreeMap;
        let d = tmpdir();
        write(&d, "base.html", "<html>{% block body %}{% endblock %}</html>");
        write(
            &d,
            "page.html",
            "{% extends \"base.html\" %}{% block body %}v{{ n }}{% endblock %}",
        );
        let c = Builder::new(&d).build().unwrap();
        let loads = || c.ctx_env.read().unwrap().loads;

        // first render: loads the entry AND its parent via the loader (slow path)
        assert_eq!(&c.render_ctx("page.html", &BTreeMap::from([("n", 1)])).unwrap(), "<html>v1</html>");
        assert_eq!(loads(), 1);

        // steady state: unchanged tree -> FAST path, no reload/re-parse
        assert_eq!(&c.render_ctx("page.html", &BTreeMap::from([("n", 2)])).unwrap(), "<html>v2</html>");
        assert_eq!(loads(), 1, "steady-state render must not reload");

        // edit the PARTIAL (base.html, a parent — NOT the entry): §9.2a must hold
        std::thread::sleep(Duration::from_millis(5));
        write(&d, "base.html", "<div>{% block body %}{% endblock %}</div>");
        bump_mtime(&d, "base.html");
        assert_eq!(&c.render_ctx("page.html", &BTreeMap::from([("n", 3)])).unwrap(), "<div>v3</div>");
        assert_eq!(loads(), 2, "a parent-partial edit must trigger exactly one reload");

        // and back to steady state after the reload
        assert_eq!(&c.render_ctx("page.html", &BTreeMap::from([("n", 4)])).unwrap(), "<div>v4</div>");
        assert_eq!(loads(), 2);
    }
}
