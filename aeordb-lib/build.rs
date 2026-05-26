//! Build-time setup for the embedded portal assets.
//!
//! `src/server/portal_routes.rs` calls `include_str!` 53 times against
//! files under `src/portal/aeor/...` and `src/portal/shared/...`. Those
//! paths are not real directories — they're symlinks (Linux/macOS) or
//! junctions (Windows) materialized at build time by this script,
//! pointing at the `aeor-web-components` and `aeordb-web-components`
//! sibling checkouts.
//!
//! Why not just commit the symlinks?
//!   - Git can't represent platform-correct symlinks portably. Default
//!     Linux/macOS clones get real symlinks; default Windows clones get
//!     plain text files containing the link target (because
//!     `core.symlinks` defaults to false there). Those text files break
//!     `include_str!`, which expects a directory.
//!
//! Sibling-repo locations vary by developer layout. We support both:
//!
//!   Layout A (Linux dev / per-workspace style):
//!     <root>/
//!     ├── aeor-web-components/        (outside aeordb-workspace)
//!     └── aeordb-workspace/
//!         ├── aeordb/                 (this repo)
//!         └── aeordb-web-components/  (sibling of aeordb)
//!
//!   Layout B (flat, common on Windows/Mac):
//!     <root>/
//!     ├── aeor-web-components/        (sibling of aeordb)
//!     ├── aeordb/                     (this repo)
//!     └── aeordb-web-components/      (sibling of aeordb)
//!
//! For each link we search upward from the repo for a directory of the
//! expected name. First match wins. If we can't find one anywhere, we
//! panic with a clear message naming the missing sibling — better than
//! letting `include_str!` fail later with 53 cryptic errors.

use std::path::{Path, PathBuf};

/// One link to materialize: `aeordb-lib/src/portal/<name>` →
/// nearest ancestor directory containing `<sibling_dir_name>`.
struct LinkSpec {
    /// Link basename under `aeordb-lib/src/portal/`.
    name: &'static str,
    /// Name of the sibling repo's checkout directory to point at.
    sibling_dir_name: &'static str,
}

const LINKS: &[LinkSpec] = &[
    LinkSpec { name: "aeor",   sibling_dir_name: "aeor-web-components" },
    LinkSpec { name: "shared", sibling_dir_name: "aeordb-web-components" },
];

fn main() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let portal_dir   = manifest_dir.join("src").join("portal");

    for spec in LINKS {
        ensure_link(&manifest_dir, &portal_dir, spec);
    }

    // build.rs is idempotent and cheap; don't ask cargo to rerun it on
    // every source change. It'll rerun on cargo clean / fresh checkout
    // anyway, and that's when it actually has work to do.
    println!("cargo:rerun-if-changed=build.rs");
}

fn ensure_link(manifest_dir: &Path, portal_dir: &Path, spec: &LinkSpec) {
    let link_path = portal_dir.join(spec.name);

    // If the link already exists AS a working directory, we're done.
    // (Real symlink on Unix, junction on Windows — both pass `is_dir`.)
    if link_path.is_dir() {
        return;
    }

    // It exists as something else — a text file from a Windows git
    // checkout (pre-fix), a broken symlink, etc. Remove it before
    // recreating.
    if link_path.exists() || link_path.symlink_metadata().is_ok() {
        let _ = std::fs::remove_file(&link_path);
        let _ = std::fs::remove_dir(&link_path);
    }

    // Search upward from this crate for a directory named `sibling_dir_name`.
    // First match wins. We cap the walk so we don't drift up to `/`.
    let target = find_sibling_dir(manifest_dir, spec.sibling_dir_name)
        .unwrap_or_else(|| panic!(
            "portal asset sibling `{0}` not found in any ancestor of `{1}`. \
             The build needs the `{0}` repo checked out near this repo; see \
             the top-level README for the expected layout.",
            spec.sibling_dir_name,
            manifest_dir.display(),
        ));

    create_directory_link(&link_path, &target, spec.name);
}

/// Walk up from `start` looking for any ancestor that contains a direct
/// child directory matching `dir_name`. Returns an absolute, canonical
/// path so the symlink/junction stays correct even if cwd changes.
///
/// Cap the walk at 12 levels — enough to escape `aeordb-workspace/aeordb`
/// or whatever extra prefix a dev uses, but far short of `/`. If we
/// walked all the way to root we'd risk pointing at someone's
/// unrelated `Documents/aeor-web-components` test folder.
fn find_sibling_dir(start: &Path, dir_name: &str) -> Option<PathBuf> {
    let mut cursor: Option<&Path> = Some(start);
    for _ in 0..12 {
        let dir = cursor?;
        let candidate = dir.join(dir_name);
        if candidate.is_dir() {
            if let Ok(canonical) = candidate.canonicalize() {
                return Some(canonical);
            }
        }
        cursor = dir.parent();
    }
    None
}

#[cfg(unix)]
fn create_directory_link(link_path: &Path, target: &Path, _name: &str) {
    // Use a RELATIVE symlink target so the link survives moving the
    // workspace to a different absolute prefix (e.g. clone to
    // `/srv/build/aeordb` instead of `/home/wyatt/Projects/...`). The
    // absolute target was correct but pinned to one machine's layout.
    let link_parent = link_path.parent()
        .expect("link path always has a parent (portal_dir)");
    let relative = relative_path_from(link_parent, target);
    std::os::unix::fs::symlink(&relative, link_path)
        .expect("create relative symlink for portal asset");
}

/// Compute the path of `target` expressed relative to `base`. Both inputs
/// must be absolute and canonical. Walks up from `base` to the deepest
/// common ancestor, then descends into `target`.
///
/// Example:
///   base   = /home/u/Projects/aeordb-workspace/aeordb/aeordb-lib/src/portal
///   target = /home/u/Projects/aeordb-workspace/aeordb-web-components
///   result = ../../../../aeordb-web-components
#[cfg(unix)]
fn relative_path_from(base: &Path, target: &Path) -> PathBuf {
    let base_components:   Vec<_> = base.components().collect();
    let target_components: Vec<_> = target.components().collect();

    let mut common = 0;
    while common < base_components.len()
       && common < target_components.len()
       && base_components[common] == target_components[common]
    {
        common += 1;
    }

    let mut relative = PathBuf::new();
    for _ in 0..(base_components.len() - common) {
        relative.push("..");
    }
    for component in &target_components[common..] {
        relative.push(component.as_os_str());
    }
    relative
}

#[cfg(windows)]
fn create_directory_link(link_path: &Path, target: &Path, name: &str) {
    // NTFS junctions don't require admin / Developer Mode and behave like
    // regular directories to every Windows API (and to Rust's std::fs).
    // We invoke `cmd /C mklink /J` because Rust's
    // `std::os::windows::fs::symlink_dir` creates a real symlink, which
    // DOES require Developer Mode and trips up most checkouts.
    //
    // Note: junctions MUST point at an absolute path — NTFS doesn't
    // support relative reparse points. The Unix branch above uses a
    // relative symlink (workspace-portable); the Windows side accepts
    // the absolute-path tradeoff because it's a hard NTFS limitation,
    // not something we can work around. If you move the workspace on
    // Windows, run `cargo build` once and the junction will be
    // recreated against the new absolute target.
    let status = std::process::Command::new("cmd")
        .args(&[
            "/C",
            "mklink",
            "/J",
            link_path.to_str().expect("link path is UTF-8"),
            target.to_str().expect("target path is UTF-8"),
        ])
        .status()
        .unwrap_or_else(|e| panic!("failed to invoke mklink for `{}`: {}", name, e));
    if !status.success() {
        panic!(
            "mklink /J failed for `{}` (target: {})",
            name,
            target.display(),
        );
    }
}
