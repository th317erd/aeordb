use std::env;
use std::path::{Path, PathBuf};

use aeordb_error_squelch_audit::{load_inventory, refreshed_inventory, reviewed_inventory, scan_workspace, validate_inventory, write_inventory};

fn main() {
  if let Err(error) = run() {
    eprintln!("error-squelch audit failed: {error}");
    std::process::exit(1);
  }
}

fn run() -> Result<(), String> {
  let mut arguments = env::args().skip(1);
  let command = arguments.next().unwrap_or_else(|| "check".to_string());
  let workspace_root = env::current_dir().map_err(|error| format!("failed to resolve current directory: {error}"))?;
  let inventory_path =
    arguments.next().map(PathBuf::from).unwrap_or_else(|| workspace_root.join("aeordb-lib/spec/fixtures/error-squelch-allowlist-v1.json"));
  let discovered = scan_workspace(&workspace_root)?;

  match command.as_str() {
    "check" => {
      let inventory = load_inventory(&inventory_path)?;
      let errors = validate_inventory(&discovered, &inventory);
      if errors.is_empty() {
        println!("reviewed {} production error-suppression occurrences", discovered.len());
        return Ok(());
      }
      Err(errors.join("\n"))
    }
    "write" => {
      let allow_baseline_growth = arguments.any(|argument| argument == "--allow-baseline-growth");
      let previous = if inventory_path.exists() { Some(load_inventory(&inventory_path)?) } else { None };
      let inventory = refreshed_inventory(&discovered, previous.as_ref(), allow_baseline_growth)?;
      write_inventory(&inventory_path, &inventory)?;
      println!("wrote {} candidate occurrences to {}", inventory.entries.len(), display_relative(&workspace_root, &inventory_path));
      Ok(())
    }
    "review" => {
      let allow_baseline_growth = arguments.any(|argument| argument == "--allow-baseline-growth");
      if inventory_path.exists() {
        let previous = load_inventory(&inventory_path)?;
        if discovered.len() > previous.entries.len() && !allow_baseline_growth {
          return Err(format!(
            "refusing to raise the reviewed suppression baseline from {} to {}; rerun with explicit baseline-growth approval",
            previous.entries.len(),
            discovered.len()
          ));
        }
      }
      let inventory = reviewed_inventory(&discovered)?;
      write_inventory(&inventory_path, &inventory)?;
      println!("wrote {} reviewed occurrences to {}", inventory.entries.len(), display_relative(&workspace_root, &inventory_path));
      Ok(())
    }
    "summary" => {
      let mut counts = std::collections::BTreeMap::new();
      for occurrence in discovered {
        *counts.entry(occurrence.kind.as_str()).or_insert(0usize) += 1;
      }
      for (kind, count) in counts {
        println!("{kind}: {count}");
      }
      Ok(())
    }
    _ => Err(format!("unknown command {command:?}; expected check, write, review, or summary")),
  }
}

fn display_relative(root: &Path, path: &Path) -> String {
  path.strip_prefix(root).unwrap_or(path).display().to_string()
}
