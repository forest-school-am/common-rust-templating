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
}

enum Entry {
    Template { mtime: SystemTime, key: u64, out: Arc<str> },
    Static { mtime: SystemTime, bytes: Arc<[u8]> },
}

/// Rendering cache over a validated asset directory. Cheap to clone-share
/// (wrap in `Arc` in your `AppState`).
pub struct AssetCache {
    root: PathBuf,
    env: Environment<'static>,
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
        let mut env = Environment::new();
        // §9.3: escape only HTML contexts; JS/text assets are not escaped.
        env.set_auto_escape_callback(|name| {
            if name.ends_with(".html") || name.ends_with(".htm") {
                AutoEscape::Html
            } else {
                AutoEscape::None
            }
        });

        let cache = AssetCache {
            root: self.root,
            env,
            pins: self.pins,
            entries: RwLock::new(HashMap::new()),
        };

        // pins first (also reads the bytes), then parse-check required templates
        for (name, expected) in &cache.pins {
            let bytes = cache.read_verified(name, Some(expected))?;
            let _ = bytes; // just verifying it reads + matches
        }
        for name in &self.required {
            let src = String::from_utf8(cache.read_verified(name, cache.pins.get(name))?)
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
    fn path(&self, name: &str) -> PathBuf {
        self.root.join(name)
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

    /// Read a file's bytes, verifying its pin if one is expected (§9.7b/§9.8).
    fn read_verified(&self, name: &str, expected: Option<&[u8; 32]>) -> Result<Vec<u8>, RenderError> {
        let path = self.path(name);
        let bytes = std::fs::read(&path).map_err(|e| {
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
        let path = self.path(name);
        let mtime = self.mtime(&path, name)?;
        let key = params_key(params);

        if let Ok(entries) = self.entries.read() {
            if let Some(Entry::Template { mtime: m, key: k, out }) = entries.get(name) {
                if *m == mtime && *k == key {
                    return Ok(out.clone());
                }
            }
        }

        let src = String::from_utf8(self.read_verified(name, self.pins.get(name))?)
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

    /// §9.5a static mode: serve `name`'s bytes, cached by (mtime) alone.
    /// Re-reads on the next call after the file's mtime changes.
    pub fn static_file(&self, name: &str) -> Result<Arc<[u8]>, RenderError> {
        let path = self.path(name);
        let mtime = self.mtime(&path, name)?;

        if let Ok(entries) = self.entries.read() {
            if let Some(Entry::Static { mtime: m, bytes }) = entries.get(name) {
                if *m == mtime {
                    return Ok(bytes.clone());
                }
            }
        }

        let bytes: Arc<[u8]> = Arc::from(self.read_verified(name, self.pins.get(name))?);
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
    fn html_autoescapes_but_js_does_not() {
        let d = tmpdir();
        write(&d, "p.html", "<b>{{ v }}</b>");
        write(&d, "p.js.jinja", "x = {{ v }}");
        let c = Builder::new(&d).build().unwrap();
        assert_eq!(&*c.render("p.html", &[("v", "<x>")]).unwrap(), "<b>&lt;x&gt;</b>");
        assert_eq!(&*c.render("p.js.jinja", &[("v", "<x>")]).unwrap(), "x = <x>");
    }
}
