//! Build-time setup for the embedded portal assets.
//!
//! `src/server/portal_routes.rs` calls `include_str!` 53 times against
//! files under `src/portal/aeor/...` and `src/portal/shared/...`. Those
//! paths are not real directories — they're symlinks (Linux/macOS) or
//! junctions (Windows) that point at the `aeor-web-components` and
//! `aeordb-web-components` sibling checkouts.
//!
//! Git can't represent platform-correct symlinks portably:
//!   - Default Linux/macOS clones get real symlinks.
//!   - Default Windows clones get plain text files containing the link
//!     target path (because `core.symlinks` defaults to false there).
//!     Those text files break `include_str!`, which expects a directory.
//!
//! So instead of committing the links and hoping every developer has the
//! right `core.symlinks` setting, we don't track them at all
//! (`.gitignore` covers them) and materialize them here on every build:
//!
//!   - Linux/macOS: `std::os::unix::fs::symlink` — a real relative symlink
//!     pointing at the sibling checkout.
//!   - Windows:     `mklink /J` (directory junction) — works without
//!     admin privileges and is transparent to `std::fs` / `include_str!`.
//!
//! If a developer manually created their own symlink/junction, we leave
//! it alone (`is_dir()` reads as true, we skip). The build is idempotent
//! across `cargo clean` / fresh clones / repeated builds.
//!
//! Sibling-repo layout expectations (documented in the top-level README):
//!
//!   <projects-root>/
//!   ├── aeor-web-components/        (sibling of aeordb-workspace)
//!   └── aeordb-workspace/
//!       ├── aeordb/                 (this repo)
//!       └── aeordb-web-components/  (sibling of aeordb)

use std::path::{Path, PathBuf};

/// Link name (placed under `aeordb-lib/src/portal/`) and the directory it
/// must point at, expressed relative to the link's parent directory. The
/// relative form is what works on Unix; on Windows we resolve to absolute
/// before invoking `mklink /J`.
struct LinkSpec {
    name:     &'static str,
    relative: &'static str,
}

const LINKS: &[LinkSpec] = &[
    // `portal/aeor` → `<projects-root>/aeor-web-components/`
    LinkSpec {
        name:     "aeor",
        relative: "../../../../../aeor-web-components",
    },
    // `portal/shared` → `<aeordb-workspace>/aeordb-web-components/`
    LinkSpec {
        name:     "shared",
        relative: "../../../../aeordb-web-components",
    },
];

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let portal_dir   = manifest_dir.join("src").join("portal");

    for spec in LINKS {
        ensure_link(&portal_dir, spec);
    }

    // Don't re-run on incremental builds unless a link disappears.
    // The links themselves aren't watched as files (build.rs would loop);
    // we rely on the user noticing if a sibling repo's contents changed
    // and running `cargo clean` or touching a source file. Anything that
    // does change inside the linked dirs is already covered by the
    // `include_str!` invariant — cargo tracks the actual files those
    // expand to.
    println!("cargo:rerun-if-changed=build.rs");
}

fn ensure_link(portal_dir: &Path, spec: &LinkSpec) {
    let link_path = portal_dir.join(spec.name);

    // Resolve the target relative to the link's parent directory. We do
    // this here (rather than only on Windows) so the symlink's contents
    // and the junction's target agree about what they point at.
    let target_resolved = portal_dir.join(spec.relative);

    // If the link already exists AS a working directory, we're done.
    // (Real symlink on Unix, junction on Windows — both pass `is_dir`.)
    if link_path.is_dir() {
        return;
    }

    // It exists as something else (a text file from a Windows git
    // checkout, or a broken symlink) — remove and recreate.
    if link_path.exists() || link_path.symlink_metadata().is_ok() {
        // symlink_metadata catches broken symlinks; exists() returns
        // false for those because it follows. Either way, remove.
        let _ = std::fs::remove_file(&link_path);
        let _ = std::fs::remove_dir(&link_path);
    }

    // Confirm the target sibling repo is actually present. If not, give
    // a clear error pointing at the README layout so the user knows what
    // to clone, instead of letting `include_str!` fail with 53 cryptic
    // "couldn't read" errors later in the build.
    let canonical = match target_resolved.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            panic!(
                "portal asset sibling missing: {} (resolved to {}). \
                 The build needs `aeor-web-components` and \
                 `aeordb-web-components` checked out as sibling repos; \
                 see the README for the expected layout. ({})",
                spec.relative,
                target_resolved.display(),
                e,
            );
        }
    };

    create_directory_link(&link_path, &canonical, spec.name);
}

#[cfg(unix)]
fn create_directory_link(link_path: &Path, target: &Path, _name: &str) {
    // Symlink with the relative path the spec carries — keeps the link
    // portable across users who might clone the workspace at a different
    // absolute prefix. Resolution via `target.canonicalize()` is only
    // used here to validate that the target actually exists.
    let _ = target; // suppress unused-warning when relative form is used
    let portal_dir = link_path.parent()
        .expect("link path always has a parent (portal_dir)");
    let spec_relative = LINKS.iter()
        .find(|s| portal_dir.join(s.name) == link_path)
        .map(|s| s.relative)
        .expect("link_path was constructed from a LinkSpec");
    std::os::unix::fs::symlink(spec_relative, link_path)
        .expect("create relative symlink for portal asset");
}

#[cfg(windows)]
fn create_directory_link(link_path: &Path, target: &Path, name: &str) {
    // NTFS junctions don't require admin / developer mode and behave like
    // regular directories to every Windows API (and to Rust's std::fs).
    // We invoke `cmd /C mklink /J` because Rust's `std::os::windows::fs::
    // symlink_dir` creates a real symlink, which DOES require dev mode.
    let status = std::process::Command::new("cmd")
        .args(&[
            "/C",
            "mklink",
            "/J",
            link_path.to_str().expect("link path is UTF-8"),
            target.to_str().expect("target path is UTF-8"),
        ])
        .status()
        .unwrap_or_else(|e| {
            panic!("failed to invoke mklink for `{}`: {}", name, e)
        });
    if !status.success() {
        panic!(
            "mklink /J failed for `{}` (target: {})",
            name,
            target.display(),
        );
    }
}
