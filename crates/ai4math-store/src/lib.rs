//! SQLite-backed runtime state and rebuildable Markdown Wiki index.

mod observation;
mod refutation;
pub mod retained;
mod state;
mod wiki;

pub use ai4math_core::Trust;
pub use observation::*;
pub use refutation::{RefutationRecord, WitnessInput, WitnessSpecBinding};
pub use state::*;
pub use wiki::*;

use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use std::{fs, path::Path, time::Duration};

fn open_db(path: &Path, ddl: &str) -> Result<Connection> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
    db.busy_timeout(Duration::from_secs(5))?;
    db.execute_batch(ddl)
        .with_context(|| format!("initialize {}", path.display()))?;
    let mode: String = db.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
    ensure!(
        mode == "wal" || path == Path::new(":memory:"),
        "SQLite WAL is unavailable for {}",
        path.display()
    );
    Ok(db)
}
