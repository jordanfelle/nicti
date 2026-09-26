use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use shed::{develop, inventory, open, privacy};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "shed",
    about = "Throwaway spike for #61: .lrcat schema mapping research."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List every table's columns and row count.
    Schema { catalog: PathBuf },
    /// Aggregate structural inventory (counts only) as JSON.
    Inventory { catalog: PathBuf },
    /// Develop-settings key-frequency histogram, classified by owner ticket, as JSON.
    Develop { catalog: PathBuf },
    /// Fail if any of `files` contains a keyword/collection/path string pulled from `catalog`.
    PrivacyCheck {
        catalog: PathBuf,
        files: Vec<PathBuf>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Schema { catalog } => cmd_schema(&catalog),
        Command::Inventory { catalog } => cmd_inventory(&catalog),
        Command::Develop { catalog } => cmd_develop(&catalog),
        Command::PrivacyCheck { catalog, files } => cmd_privacy_check(&catalog, &files),
    }
}

fn cmd_schema(catalog: &std::path::Path) -> Result<()> {
    let conn = open::open_backup(catalog)?;
    let mut stmt =
        conn.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
    let tables: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for table in tables {
        let count: i64 = conn
            .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| {
                r.get(0)
            })
            .with_context(|| format!("counting {table}"))?;
        let mut col_stmt = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
        let cols: Vec<(String, String)> = col_stmt
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        println!(
            "{}",
            serde_json::json!({ "table": table, "row_count": count, "columns": cols })
        );
    }
    Ok(())
}

fn cmd_inventory(catalog: &std::path::Path) -> Result<()> {
    let conn = open::open_backup(catalog)?;
    let inv = inventory::inspect(&conn)?;
    println!("{}", serde_json::to_string_pretty(&inv)?);
    Ok(())
}

fn cmd_develop(catalog: &std::path::Path) -> Result<()> {
    let conn = open::open_backup(catalog)?;
    let report = develop::analyze(&conn)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn cmd_privacy_check(catalog: &std::path::Path, files: &[PathBuf]) -> Result<()> {
    let conn = open::open_backup(catalog)?;
    let sensitive = privacy::sensitive_strings(&conn)?;
    let file_refs: Vec<&std::path::Path> = files.iter().map(|p| p.as_path()).collect();
    let hits = privacy::check_files(&sensitive, &file_refs)?;
    if hits.is_empty() {
        println!("privacy-check: clean ({} file(s) checked)", files.len());
        Ok(())
    } else {
        for (file, matched) in &hits {
            eprintln!(
                "privacy-check: {file} contains a match for a redacted string (len {})",
                matched.len()
            );
        }
        anyhow::bail!(
            "privacy-check found {} match(es) -- do not commit",
            hits.len()
        );
    }
}
